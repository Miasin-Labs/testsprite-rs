//! Dependency-wave ordering for stored tests.
//!
//! Backend suites often have producers (create a resource, mint a token) that
//! must run before consumers (read/update it), and teardown steps that must run
//! last. A test declares `produces: [caps]` / `needs: [caps]` (capability
//! strings) and optionally `category: "teardown"` in its JSON body. This module
//! orders a run so every `needs` is satisfied by an earlier `produce`, with
//! teardown tests appended at the end.
//!
//! The ordering is a stable topological sort (Kahn, emitting the earliest
//! input-order ready node each step). It never drops a test: unsatisfied needs
//! don't block, and cycles fall back to input order.

use std::collections::{HashMap, HashSet};

use super::LocalTest;

/// Split `tests` into (main-phase dependency LEVELS, teardown tests). Each level
/// is a set of tests whose dependencies are satisfied by earlier levels — safe
/// to run concurrently. Teardown tests run last as their own phase. Stable
/// within a level; total (never drops a test; cycles/unsatisfied-needs fall back
/// to a final input-order level).
pub fn waves(tests: Vec<LocalTest>) -> (Vec<Vec<LocalTest>>, Vec<LocalTest>) {
    let n = tests.len();
    let is_teardown = |i: usize| tests[i].category() == Some("teardown");
    let main_idx: Vec<usize> = (0..n).filter(|&i| !is_teardown(i)).collect();
    let teardown_idx: Vec<usize> = (0..n).filter(|&i| is_teardown(i)).collect();

    let level_idx = topo_levels(&main_idx, &tests);

    let mut slots: Vec<Option<LocalTest>> = tests.into_iter().map(Some).collect();
    let levels: Vec<Vec<LocalTest>> = level_idx
        .into_iter()
        .map(|lvl| lvl.into_iter().filter_map(|i| slots[i].take()).collect())
        .collect();
    let teardown: Vec<LocalTest> = teardown_idx
        .into_iter()
        .filter_map(|i| slots[i].take())
        .collect();
    (levels, teardown)
}

/// Kahn level-BFS of `indices` (a subset of `tests`) by needs/produces. Level 0
/// = in-degree-0 tests (input order); each next level = tests unblocked by the
/// previous ones. Cycles / unsatisfied leftovers form a final level.
fn topo_levels(indices: &[usize], tests: &[LocalTest]) -> Vec<Vec<usize>> {
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
    // Level-BFS: each pass emits every currently in-degree-0 node (input order).
    let mut levels: Vec<Vec<usize>> = Vec::new();
    let mut done: HashSet<usize> = HashSet::new();
    loop {
        let ready: Vec<usize> = indices
            .iter()
            .copied()
            .filter(|i| !done.contains(i) && indeg.get(i).copied().unwrap_or(0) == 0)
            .collect();
        if ready.is_empty() {
            break;
        }
        for &i in &ready {
            done.insert(i);
        }
        for &i in &ready {
            if let Some(consumers) = adj.get(&i).cloned() {
                for c in consumers {
                    if let Some(d) = indeg.get_mut(&c) {
                        *d = d.saturating_sub(1);
                    }
                }
            }
        }
        levels.push(ready);
    }
    // Cycle / leftover safety net: any not-yet-emitted form a final level.
    let leftover: Vec<usize> = indices
        .iter()
        .copied()
        .filter(|i| !done.contains(i))
        .collect();
    if !leftover.is_empty() {
        levels.push(leftover);
    }
    levels
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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

    /// Flatten waves to a single ordered list (test-only convenience).
    fn flat(tests: Vec<LocalTest>) -> Vec<LocalTest> {
        let (levels, teardown) = waves(tests);
        let mut out: Vec<LocalTest> = levels.into_iter().flatten().collect();
        out.extend(teardown);
        out
    }

    #[test]
    fn producer_runs_before_consumer() {
        let out = flat(vec![
            t("consumer", json!({ "needs": ["token"] })),
            t("producer", json!({ "produces": ["token"] })),
        ]);
        assert_eq!(ids(&out), ["producer", "consumer"]);
    }

    #[test]
    fn teardown_runs_last() {
        let out = flat(vec![
            t("teardown", json!({ "category": "teardown" })),
            t("a", json!({})),
            t("b", json!({})),
        ]);
        assert_eq!(ids(&out), ["a", "b", "teardown"]);
    }

    #[test]
    fn independent_tests_keep_input_order() {
        let out = flat(vec![
            t("x", json!({})),
            t("y", json!({})),
            t("z", json!({})),
        ]);
        assert_eq!(ids(&out), ["x", "y", "z"]);
    }

    #[test]
    fn chain_orders_transitively() {
        // c needs b's cap, b needs a's cap → a, b, c regardless of input order.
        let out = flat(vec![
            t("c", json!({ "needs": ["capB"] })),
            t("b", json!({ "needs": ["capA"], "produces": ["capB"] })),
            t("a", json!({ "produces": ["capA"] })),
        ]);
        assert_eq!(ids(&out), ["a", "b", "c"]);
    }

    #[test]
    fn cycle_falls_back_without_dropping() {
        // a needs X (produced by b), b needs Y (produced by a) → cycle.
        let out = flat(vec![
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
        let out = flat(vec![
            t("needy", json!({ "needs": ["ghost"] })),
            t("plain", json!({})),
        ]);
        assert_eq!(ids(&out), ["needy", "plain"]);
    }

    fn level_ids(levels: &[Vec<LocalTest>]) -> Vec<Vec<String>> {
        levels.iter().map(|l| ids(l)).collect()
    }

    #[test]
    fn waves_groups_independent_into_one_level() {
        let (levels, teardown) = waves(vec![
            t("x", json!({})),
            t("y", json!({})),
            t("z", json!({})),
        ]);
        assert_eq!(level_ids(&levels), vec![vec!["x", "y", "z"]]);
        assert!(teardown.is_empty());
    }

    #[test]
    fn waves_splits_chain_into_separate_levels() {
        // producer -> consumer are separate levels (cannot run concurrently).
        let (levels, _) = waves(vec![
            t("consumer", json!({ "needs": ["tok"] })),
            t("producer", json!({ "produces": ["tok"] })),
        ]);
        assert_eq!(level_ids(&levels), vec![vec!["producer"], vec!["consumer"]]);
    }

    #[test]
    fn waves_separates_teardown_phase() {
        let (levels, teardown) = waves(vec![
            t("td", json!({ "category": "teardown" })),
            t("a", json!({})),
        ]);
        assert_eq!(level_ids(&levels), vec![vec!["a"]]);
        assert_eq!(ids(&teardown), ["td"]);
    }
}
