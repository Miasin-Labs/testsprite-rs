//! Prevention corpus — recurring failures distilled into forward-looking
//! guidance for generation prompts.
//!
//! Triage classifies failures into the runs table, but that is a post-hoc log:
//! the same compile/exec/assertion signatures recur and are never turned into
//! reusable "avoid this" advice fed back into generation. Proactive prevention
//! distilled from a project's own failure history is a top-three ablation
//! contributor in the literature. This module mines the runs table for the
//! most frequent failure signatures and renders them as a compact do/don't
//! block a generator can prepend to its prompt.

use std::path::Path;

/// A recurring failure pattern and the advice that would prevent it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Guideline {
    pub kind: String,
    /// The canonicalized error signature (digits/ids/paths stripped).
    pub signature: String,
    pub count: usize,
    /// Deterministic per-kind advice — no LLM.
    pub advice: String,
}

/// Canonicalize an error message into a signature that collapses runs that
/// differ only in incidental data: numbers, hex/uuids, quoted strings, and
/// filesystem paths become placeholders, and whitespace is normalized. So
/// "expected 200, got 404" and "expected 201, got 403" share one signature.
pub fn signature(error: &str) -> String {
    let first_line = error.lines().next().unwrap_or("").trim();
    first_line
        .split_whitespace()
        .map(canonical_token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Canonicalize one whitespace-delimited token: a path collapses to `PATH`, a
/// quoted literal to `STR`, anything containing a digit (a number, status
/// code, uuid, or hex id) to `N`. A plain word (no digit) is left as-is, so
/// "boom" stays "boom" while "404" and "ts_rs_9f3a2" become `N`.
fn canonical_token(tok: &str) -> String {
    let trimmed = tok.trim_matches(|c: char| matches!(c, ',' | ';' | ':' | ')' | '('));
    if trimmed.contains('/') {
        return "PATH".to_string();
    }
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        return "STR".to_string();
    }
    if trimmed.chars().any(|c| c.is_ascii_digit()) {
        return "N".to_string();
    }
    tok.to_string()
}

/// Deterministic advice for a failure kind — the do/don't a generator should
/// heed to avoid reproducing it. A non-empty fallback keeps every kind
/// covered.
fn advice_for(kind: &str) -> &'static str {
    match kind {
        "build_error" => {
            "ensure every import resolves and only the crate's real public API is called; \
             prefer types the source actually exposes"
        }
        "routing_404" => {
            "verify the endpoint path exists in the code summary before asserting its status; \
             do not invent routes"
        }
        "auth" => {
            "supply credentials via ${VAR} placeholders from .testsprite.env / variables.json, \
             and drive the real login/token flow before calling protected endpoints"
        }
        "assertion" => {
            "assert on values the code summary/PRD actually specifies; avoid guessing exact \
             response bodies you cannot infer"
        }
        "network" | "network_timeout" => {
            "target the project's configured URL and assume the app is already up; do not \
             hard-code hosts/ports"
        }
        "suspect_oracle" => {
            "only assert behavior the current code exhibits; a case that fails on unchanged \
             code encodes a wrong oracle"
        }
        "residual_alignment" => {
            "for changed code, assert the NEW behavior the diff introduces, not the pre-change \
             semantics"
        }
        "timeout" => "keep cases fast and avoid unbounded waits or loops",
        _ => "keep cases deterministic, self-contained, and grounded in the provided context",
    }
}

/// One telemetry row shape the miner reads.
type FailRow = (Option<String>, String); // (failure_kind, error)

/// Fold failing runs into the top `max` guidelines, ranked by frequency. Pure
/// over the rows so it is unit-testable without a DB.
pub fn distill(rows: &[FailRow], max: usize) -> Vec<Guideline> {
    use std::collections::HashMap;
    // (kind, signature) -> count
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for (kind, error) in rows {
        let kind = kind.clone().unwrap_or_else(|| "unknown".to_string());
        let sig = signature(error);
        if sig.is_empty() {
            continue;
        }
        *counts.entry((kind, sig)).or_insert(0) += 1;
    }
    let mut ranked: Vec<Guideline> = counts
        .into_iter()
        .map(|((kind, signature), count)| Guideline {
            advice: advice_for(&kind).to_string(),
            kind,
            signature,
            count,
        })
        .collect();
    // Most frequent first; deterministic tie-break by kind then signature.
    ranked.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.signature.cmp(&b.signature))
    });
    ranked.truncate(max);
    ranked
}

/// Query the runs table for FAILED runs and distill the top `max` guidelines.
pub async fn mine(root: &Path, max: usize) -> anyhow::Result<Vec<Guideline>> {
    let pool = crate::local::db::open(root).await?;
    let rows: Vec<FailRow> =
        sqlx::query_as("SELECT failure_kind, error FROM runs WHERE passed = 0")
            .fetch_all(&pool)
            .await?;
    Ok(distill(&rows, max))
}

/// A compact prompt block prepending the mined guidelines to a generation
/// prompt. Empty string when there is nothing to warn about.
pub fn render_prompt_block(guidelines: &[Guideline]) -> String {
    if guidelines.is_empty() {
        return String::new();
    }
    let mut s =
        String::from("\n\nAvoid these recurring failures seen in THIS project's history:\n");
    for g in guidelines {
        s.push_str(&format!(
            "- [{}] {} (×{}) — {}\n",
            g.kind, g.signature, g.count, g.advice
        ));
    }
    s
}

/// `test guidelines`: print the mined prevention corpus (JSON when `json`).
pub async fn guidelines_report(root: &Path, json: bool) -> anyhow::Result<i32> {
    let guidelines = mine(root, 20).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&guidelines)?);
    } else if guidelines.is_empty() {
        println!("guidelines: no recurring failures recorded yet");
    } else {
        println!(
            "guidelines: {} recurring failure pattern(s):",
            guidelines.len()
        );
        for g in &guidelines {
            println!("  [{}] ×{}  {}", g.kind, g.count, g.signature);
            println!("      → {}", g.advice);
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_collapses_incidental_numbers_ids_and_paths() {
        // Numerically-different expected/got lines share one signature.
        assert_eq!(
            signature("expected 200, got 404"),
            signature("expected 201, got 403")
        );
        // uuids/hex and paths canonicalize.
        let a = signature("could not write test file: /tmp/ts_rs_9f3a2/x.rs: denied");
        let b = signature("could not write test file: /tmp/ts_rs_11bb/y.rs: denied");
        assert_eq!(a, b);
        // Only the first line matters (multi-line dumps collapse to their head).
        assert_eq!(signature("boom\nline2\nline3"), "boom");
    }

    #[test]
    fn distill_ranks_by_frequency_and_attaches_advice() {
        let rows = vec![
            (
                Some("routing_404".to_string()),
                "expected 200, got 404".to_string(),
            ),
            (
                Some("routing_404".to_string()),
                "expected 201, got 404".to_string(),
            ),
            (
                Some("routing_404".to_string()),
                "expected 200, got 403".to_string(),
            ),
            (Some("auth".to_string()), "401 Unauthorized".to_string()),
        ];
        let g = distill(&rows, 10);
        // Three routing_404 rows collapse to one signature with count 3, first.
        assert_eq!(g[0].kind, "routing_404");
        assert_eq!(g[0].count, 3);
        assert!(g[0].advice.contains("path exists"));
        // The auth pattern is present with its own advice.
        assert!(
            g.iter()
                .any(|x| x.kind == "auth" && x.advice.contains("credentials"))
        );
    }

    #[test]
    fn render_prompt_block_is_empty_when_nothing_to_warn() {
        assert_eq!(render_prompt_block(&[]), "");
        let g = vec![Guideline {
            kind: "auth".into(),
            signature: "N Unauthorized".into(),
            count: 2,
            advice: "supply credentials".into(),
        }];
        let block = render_prompt_block(&g);
        assert!(block.contains("Avoid these recurring failures"));
        assert!(block.contains("[auth]"));
        assert!(block.contains("×2"));
    }

    #[tokio::test]
    async fn mine_reads_failed_runs_from_the_store() {
        let root = crate::local::tmp_root();
        let id = crate::local::store::add_value(
            &root,
            serde_json::json!({"id":"t1","title":"x","kind":"backend"}),
        )
        .await
        .unwrap();
        for err in ["expected 200, got 404", "expected 201, got 404"] {
            crate::local::store::write_result(
                &root,
                &id,
                &crate::server::executors::Outcome::fail(err, String::new()),
                None,
                crate::server::executors::TestKind::Backend,
            )
            .await
            .unwrap();
        }
        let g = mine(&root, 10).await.unwrap();
        assert!(!g.is_empty());
        assert_eq!(g[0].kind, "routing_404");
        std::fs::remove_dir_all(root).ok();
    }
}
