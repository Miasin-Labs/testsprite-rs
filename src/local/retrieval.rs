//! Repository-aware few-shot retrieval for generation prompts.
//!
//! Generation is otherwise context-local: it never mines the repo's OWN
//! passing tests for the setup, argument shapes, and realistic return values a
//! new test should mirror. Retrieving related stored tests as exemplars is a
//! top correctness lever in the literature (roughly halves compile/run errors).
//!
//! "Related" is measured by dependency-set overlap, not lexical similarity: two
//! tests are related when they mention many of the same identifiers (function
//! and type names from the structural surface). This keeps the whole thing
//! local — no embeddings, just the tree-sitter surface and the SQLite store
//! already present.

use super::LocalTest;
use super::coverage::{mentions, test_haystack};

/// The identifiers a test references, drawn from a known vocabulary (the
/// structural surface's function names). Used as the test's "dependency set".
fn referenced(vocab: &[String], test: &LocalTest) -> std::collections::BTreeSet<String> {
    let hay = test_haystack(test);
    vocab
        .iter()
        .filter(|name| mentions(&hay, name))
        .cloned()
        .collect()
}

/// Jaccard overlap of two identifier sets: |A∩B| / |A∪B|. `0.0` when both are
/// empty (nothing to relate on).
fn jaccard(a: &std::collections::BTreeSet<String>, b: &std::collections::BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Up to `k` stored tests most related (by dependency-set Jaccard) to the set
/// of `target` identifiers, as compact `{title, spec?/code?, description}`
/// exemplars for a generation prompt. Only tests with nonzero overlap are
/// returned; ties break by title for determinism.
pub fn exemplars_for(
    vocab: &[String],
    targets: &[String],
    tests: &[LocalTest],
    k: usize,
) -> Vec<serde_json::Value> {
    let want: std::collections::BTreeSet<String> = targets.iter().cloned().collect();
    if want.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(f64, &LocalTest)> = tests
        .iter()
        .map(|t| (jaccard(&want, &referenced(vocab, t)), t))
        .filter(|(s, _)| *s > 0.0)
        .collect();
    // Highest overlap first; deterministic tie-break by title then id.
    scored.sort_by(|(sa, ta), (sb, tb)| {
        sb.partial_cmp(sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| ta.title.cmp(&tb.title))
            .then_with(|| ta.id.cmp(&tb.id))
    });
    scored
        .into_iter()
        .take(k)
        .map(|(_, t)| {
            let case = t.to_case_value();
            let mut ex = serde_json::json!({ "title": t.title });
            for key in ["spec", "steps", "planSteps", "code", "description"] {
                if let Some(v) = case.get(key).filter(|v| !v.is_null()) {
                    ex[key] = v.clone();
                }
            }
            ex
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test(id: &str, title: &str, code: &str) -> LocalTest {
        serde_json::from_value(serde_json::json!({
            "id": id, "title": title, "code": code,
        }))
        .unwrap()
    }

    #[test]
    fn retrieves_tests_that_share_the_most_identifiers() {
        let vocab = vec![
            "compute_totals".to_string(),
            "apply_discount".to_string(),
            "render".to_string(),
        ];
        let tests = vec![
            test(
                "t1",
                "totals + discount",
                "compute_totals(); apply_discount();",
            ),
            test("t2", "just totals", "compute_totals();"),
            test("t3", "unrelated", "render();"),
        ];
        // Target references both totals + discount → t1 (2/2) beats t2 (1/2),
        // and t3 (0 overlap) is excluded entirely.
        let ex = exemplars_for(
            &vocab,
            &["compute_totals".to_string(), "apply_discount".to_string()],
            &tests,
            2,
        );
        assert_eq!(ex.len(), 2);
        assert_eq!(ex[0]["title"], "totals + discount");
        assert_eq!(ex[1]["title"], "just totals");
        assert!(ex[0]["code"].as_str().unwrap().contains("apply_discount"));
    }

    #[test]
    fn no_targets_or_no_overlap_yields_no_exemplars() {
        let vocab = vec!["foo".to_string()];
        let tests = vec![test("t1", "bar", "bar();")];
        assert!(exemplars_for(&vocab, &[], &tests, 3).is_empty());
        assert!(exemplars_for(&vocab, &["foo".to_string()], &tests, 3).is_empty());
    }

    #[test]
    fn jaccard_is_intersection_over_union() {
        let a: std::collections::BTreeSet<String> =
            ["x", "y"].iter().map(|s| s.to_string()).collect();
        let b: std::collections::BTreeSet<String> =
            ["y", "z"].iter().map(|s| s.to_string()).collect();
        assert!((jaccard(&a, &b) - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(jaccard(&a, &a), 1.0);
        let empty = std::collections::BTreeSet::new();
        assert_eq!(jaccard(&empty, &empty), 0.0);
    }
}
