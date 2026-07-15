//! Dependency-wave ordering for stored tests.
//!
//! Backend suites often have producers (create a resource, mint a token) that
//! must run before consumers (read/update it), and teardown steps that must run
//! last. A test declares `produces: [caps]` / `needs: [caps]` (capability
//! strings) and optionally `category: "teardown"` in its JSON body. This module
//! orders a run so every `needs` is satisfied by an earlier `produces`, with
//! teardown tests appended at the end.
//!
//! The ordering is a stable topological sort (Kahn, emitting the earliest
//! input-order ready node each step). It never drops a test: unsatisfied needs
//! don't block, and cycles fall back to input order.

use std::collections::{HashMap, HashSet};

use super::LocalTest;

/// Order `tests` into dependency waves: producers before consumers, teardown
/// last. Stable within a wave; total (never drops a test).
pub fn order_by_waves(tests: Vec<LocalTest>) -> Vec<LocalTest> {
    let n = tests.len();
    let is_teardown = |i: usize| tests[i].category() == Some("teardown");
    let main_idx: Vec<usize> = (0..n).filter(|&i| !is_teardown(i)).collect();
    let teardown_idx: Vec<usize> = (0..n).filter(|&i| is_teardown(i)).collect();

    let mut order = topo(&main_idx, &tests);
    order.extend(topo(&teardown_idx, &tests));

    // Reindex into the owned tests, preserving the computed order.
    let mut slots: Vec<Option<LocalTest>> = tests.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|i| slots[i].take())
        .collect()
}

/// Stable topological sort of `indices` (a subset of `tests`) by needs/produces.
fn topo(indices: &[usize], tests: &[LocalTest]) -> Vec<usize> {
    // cap -> producer indices (within this subset).
    let mut producers: HashMap<String, Vec<usize>> = HashMap::new();
    for &i in indices {
        for cap in tests[i].produces() {
            producers.entry(cap).or_default().push(i);
        }
    }

    // Edges producer -> consumer; in-degree per consumer.
    let mut indeg: HashMap<usize, usize> = indices.iter().map(|&i| (i, 0usize)).collect();
    let mut adj: HashMap<usize, Vec<usize>> = HashMap::new();
    for &c in indices {
        for cap in tests[c].needs() {
            if let Some(ps) = producers.get(&cap) {
                for &p in ps {
                    if p != c {
                        adj.entry(p).or_default().push(c);
                        if let Some(d) = indeg.get_mut(&c) {
                            *d += 1;
                        }
                    }
                }
            }
        }
    }

    // Kahn, emitting the earliest input-order ready node each step (stable).
    let mut out = Vec::with_capacity(indices.len());
    let mut done: HashSet<usize> = HashSet::new();
    loop {
        let mut progressed = false;
        for &i in indices {
            if !done.contains(&i) && indeg.get(&i).copied().unwrap_or(0) == 0 {
                out.push(i);
                done.insert(i);
                progressed = true;
                if let Some(consumers) = adj.get(&i).cloned() {
                    for c in consumers {
                        if let Some(d) = indeg.get_mut(&c) {
                            *d = d.saturating_sub(1);
                        }
                    }
                }
                break; // restart scan → strict input-order stability
            }
        }
        if !progressed {
            break;
        }
    }
    // Cycle / leftover safety net: append any not-yet-emitted in input order.
    for &i in indices {
        if !done.contains(&i) {
            out.push(i);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn t(id: &str, extra: serde_json::Value) -> LocalTest {
        LocalTest {
            id: id.to_string(),
            title: String::new(),
            description: String::new(),
            kind: None,
            spec: None,
            extra: extra.as_object().cloned().unwrap_or_default(),
        }
    }

    fn ids(v: &[LocalTest]) -> Vec<String> {
        v.iter().map(|t| t.id.clone()).collect()
    }

    #[test]
    fn producer_runs_before_consumer() {
        let out = order_by_waves(vec![
            t("consumer", json!({ "needs": ["token"] })),
            t("producer", json!({ "produces": ["token"] })),
        ]);
        assert_eq!(ids(&out), ["producer", "consumer"]);
    }

    #[test]
    fn teardown_runs_last() {
        let out = order_by_waves(vec![
            t("teardown", json!({ "category": "teardown" })),
            t("a", json!({})),
            t("b", json!({})),
        ]);
        assert_eq!(ids(&out), ["a", "b", "teardown"]);
    }

    #[test]
    fn independent_tests_keep_input_order() {
        let out = order_by_waves(vec![t("x", json!({})), t("y", json!({})), t("z", json!({}))]);
        assert_eq!(ids(&out), ["x", "y", "z"]);
    }

    #[test]
    fn chain_orders_transitively() {
        // c needs b's cap, b needs a's cap → a, b, c regardless of input order.
        let out = order_by_waves(vec![
            t("c", json!({ "needs": ["capB"] })),
            t("b", json!({ "needs": ["capA"], "produces": ["capB"] })),
            t("a", json!({ "produces": ["capA"] })),
        ]);
        assert_eq!(ids(&out), ["a", "b", "c"]);
    }

    #[test]
    fn cycle_falls_back_without_dropping() {
        // a needs X (produced by b), b needs Y (produced by a) → cycle.
        let out = order_by_waves(vec![
            t("a", json!({ "needs": ["X"], "produces": ["Y"] })),
            t("b", json!({ "needs": ["Y"], "produces": ["X"] })),
        ]);
        // No test is dropped; both present.
        assert_eq!(out.len(), 2);
        let got: HashSet<String> = ids(&out).into_iter().collect();
        assert!(got.contains("a") && got.contains("b"));
    }

    #[test]
    fn unsatisfied_need_does_not_block() {
        // needs a cap nobody produces → still runs, in input order.
        let out = order_by_waves(vec![
            t("needy", json!({ "needs": ["ghost"] })),
            t("plain", json!({})),
        ]);
        assert_eq!(ids(&out), ["needy", "plain"]);
    }
}
