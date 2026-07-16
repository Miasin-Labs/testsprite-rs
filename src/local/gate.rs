//! `gate` — run the stored suite as a CI gate: emit JUnit XML + a JSON
//! summary, best-effort post a PR comment via the authenticated `gh` CLI,
//! and return a non-zero exit code if any test failed.

use std::path::Path;

use serde_json::Value;

use super::ts_dir;

/// Knobs for one gate run.
pub struct GateOpts<'a> {
    pub url: Option<&'a str>,
    pub model: &'a str,
    /// Run a representative subset first; only escalate to the full suite if it
    /// passes.
    pub smoke: bool,
    /// Fail the gate below this mutation kill percent (0-100).
    pub min_mutation: Option<f64>,
}

/// Run every stored test, write `testsprite_tests/junit.xml` and
/// `testsprite_tests/gate-summary.json`, best-effort comment on the current
/// PR (via `gh`), and return `0` if every test passed, `1` otherwise. The
/// exit code depends only on test results and the optional mutation floor —
/// `gh` failures never propagate.
pub async fn gate(root: &Path, opts: GateOpts<'_>) -> anyhow::Result<i32> {
    // Fast pre-gate: a representative case per group / failure cluster. A cheap
    // red here short-circuits the full run — the inner CI loop stays sub-suite
    // fast when something obvious broke.
    if opts.smoke {
        let smoke_ids = smoke_subset(root).await?;
        if !smoke_ids.is_empty() {
            let smoke = crate::local::run::run_collect(
                root, &smoke_ids, opts.url, opts.model, false, None, 1, false,
            )
            .await?;
            let smoke_failed = smoke
                .iter()
                .filter(|r| !r.get("passed").and_then(Value::as_bool).unwrap_or(false))
                .count();
            if smoke_failed > 0 {
                println!(
                    "gate --smoke: {smoke_failed}/{} representative case(s) failed — \
                     skipping the full suite",
                    smoke.len()
                );
                write_artifacts(root, &smoke)?;
                return Ok(1);
            }
            println!(
                "gate --smoke: {} representative case(s) passed — running the full suite",
                smoke.len()
            );
        }
    }

    let results =
        crate::local::run::run_collect(root, &[], opts.url, opts.model, false, None, 1, false)
            .await?;

    let total = results.len();
    let failed = results
        .iter()
        .filter(|r| !r.get("passed").and_then(Value::as_bool).unwrap_or(false))
        .count();
    let passed = total - failed;

    write_artifacts(root, &results)?;

    println!(
        "gate: {passed}/{total} passed ({failed} failed) — junit.xml + gate-summary.json written"
    );

    let body = comment_body(&results, total, passed, failed);
    try_gh_comment(root, &body);

    // Oracle-strength floor: a green, high-coverage suite can still catch zero
    // bugs, so `--min-mutation` fails the gate on weak assertions, not just on
    // failing tests.
    let mut exit = if failed == 0 { 0 } else { 1 };
    if let Some(floor) = opts.min_mutation {
        let scan = root.to_path_buf();
        let report =
            tokio::task::spawn_blocking(move || crate::local::mutation::run_rust(&scan, 300))
                .await?;
        match report.kill_score {
            Some(score) if score < floor => {
                println!(
                    "gate --min-mutation: kill score {score:.1}% is below the {floor:.1}% floor \
                     ({} mutant(s) survived)",
                    report.missed
                );
                exit = 1;
            }
            Some(score) => {
                println!("gate --min-mutation: kill score {score:.1}% meets the {floor:.1}% floor");
            }
            None => {
                println!(
                    "gate --min-mutation: no mutation measurement available ({}), floor not enforced",
                    report.unavailable.as_deref().unwrap_or("unknown")
                );
            }
        }
    }

    Ok(exit)
}

/// One representative stored test per group (falling back to per-modality) —
/// the smoke tier. Deterministic: the first case (by id) in each bucket.
async fn smoke_subset(root: &Path) -> anyhow::Result<Vec<String>> {
    let mut tests = crate::local::run::runnable_tests(root).await?;
    tests.sort_by(|a, b| a.id.cmp(&b.id));
    let mut seen = std::collections::BTreeSet::new();
    let mut ids = Vec::new();
    for t in &tests {
        // Bucket by group when present, else by modality — one case each.
        let bucket = t
            .group()
            .map(str::to_string)
            .unwrap_or_else(|| format!("kind:{:?}", t.kind.unwrap_or_default()));
        if seen.insert(bucket) {
            ids.push(t.id.clone());
        }
    }
    Ok(ids)
}

/// Write the JUnit XML + JSON summary artifacts for a result set.
fn write_artifacts(root: &Path, results: &[Value]) -> anyhow::Result<()> {
    let total = results.len();
    let failed = results
        .iter()
        .filter(|r| !r.get("passed").and_then(Value::as_bool).unwrap_or(false))
        .count();
    let dir = ts_dir(root);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("junit.xml"), render_junit(results, total, failed))?;
    let summary = serde_json::json!({
        "total": total,
        "passed": total - failed,
        "failed": failed,
        "results": results,
    });
    std::fs::write(
        dir.join("gate-summary.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    Ok(())
}

/// Render a minimal but valid JUnit XML document for `results`.
fn render_junit(results: &[Value], total: usize, failed: usize) -> String {
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<testsuite name=\"testsprite\" tests=\"{total}\" failures=\"{failed}\">\n"
    ));
    for r in results {
        let id = r.get("id").and_then(Value::as_str).unwrap_or("");
        let title = r.get("title").and_then(Value::as_str).unwrap_or("");
        let passed = r.get("passed").and_then(Value::as_bool).unwrap_or(false);
        let error = r.get("error").and_then(Value::as_str).unwrap_or("");
        xml.push_str(&format!(
            "  <testcase name=\"{}\" classname=\"{}\">\n",
            xml_escape(title),
            xml_escape(id)
        ));
        if !passed {
            xml.push_str(&format!(
                "    <failure message=\"{}\">{}</failure>\n",
                xml_escape(error),
                xml_escape(error)
            ));
        }
        xml.push_str("  </testcase>\n");
    }
    xml.push_str("</testsuite>\n");
    xml
}

/// Escape `&`, `<`, `>`, `"`, and `'` for safe inclusion in XML text/attrs.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Compose a PR-comment body summarizing the gate run.
fn comment_body(results: &[Value], total: usize, passed: usize, failed: usize) -> String {
    let mut body = format!("**testsprite gate**: {passed}/{total} passed ({failed} failed)\n");
    if failed > 0 {
        body.push_str("\nFailed tests:\n");
        for r in results
            .iter()
            .filter(|r| !r.get("passed").and_then(Value::as_bool).unwrap_or(false))
            .take(10)
        {
            let title = r.get("title").and_then(Value::as_str).unwrap_or("");
            body.push_str(&format!("- {title}\n"));
        }
    }
    body
}

/// Best-effort: if `gh` is installed and `root` is on a PR, post `body` as a
/// PR comment. Any missing tool, non-repo, non-PR, or command failure is
/// swallowed with a printed note — never propagated to the caller.
fn try_gh_comment(root: &Path, body: &str) {
    let gh_ok = std::process::Command::new("gh")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !gh_ok {
        println!("gh: not on a PR (or gh unavailable) — skipping PR comment");
        return;
    }

    let pr_view = std::process::Command::new("gh")
        .args(["pr", "view", "--json", "number,url"])
        .current_dir(root)
        .output();
    let on_pr = match pr_view {
        Ok(o) if o.status.success() => String::from_utf8(o.stdout)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.get("number").and_then(Value::as_i64))
            .is_some(),
        _ => false,
    };
    if !on_pr {
        println!("gh: not on a PR (or gh unavailable) — skipping PR comment");
        return;
    }

    let commented = std::process::Command::new("gh")
        .args(["pr", "comment", "--body", body])
        .current_dir(root)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !commented {
        println!("gh: failed to post PR comment — skipping");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_reserved_xml_characters() {
        assert_eq!(
            xml_escape(r#"a & b < c > d " e ' f"#),
            "a &amp; b &lt; c &gt; d &quot; e &apos; f"
        );
    }

    #[test]
    fn junit_reports_failures_count_and_wraps_error() {
        let results = vec![
            serde_json::json!({"id":"t1","title":"ok test","passed":true,"error":null}),
            serde_json::json!({"id":"t2","title":"bad <test>","passed":false,"error":"boom & bust"}),
        ];
        let xml = render_junit(&results, 2, 1);
        assert!(xml.contains("tests=\"2\" failures=\"1\""));
        assert!(xml.contains("name=\"bad &lt;test&gt;\""));
        assert!(xml.contains("<failure message=\"boom &amp; bust\">boom &amp; bust</failure>"));
        assert_eq!(xml.matches("<failure").count(), 1);
    }

    #[tokio::test]
    async fn smoke_subset_picks_one_representative_per_group() {
        let root = crate::local::tmp_root();
        let add = |v: serde_json::Value| {
            let root = root.clone();
            async move { crate::local::store::add_value(&root, v).await.unwrap() }
        };
        // Two cases in group "a", one in group "b" → 2 buckets → 2 smoke ids.
        add(serde_json::json!({"id":"a1","title":"a one","kind":"command","code":"true","group":"a"}))
            .await;
        add(serde_json::json!({"id":"a2","title":"a two","kind":"command","code":"true","group":"a"}))
            .await;
        add(serde_json::json!({"id":"b1","title":"b one","kind":"command","code":"true","group":"b"}))
            .await;

        let ids = smoke_subset(&root).await.unwrap();
        assert_eq!(ids.len(), 2, "one representative per group: {ids:?}");
        // Deterministic: the lowest-id case in each bucket.
        assert!(ids.contains(&"a1".to_string()));
        assert!(ids.contains(&"b1".to_string()));

        // Quarantined cases never enter the smoke tier.
        crate::local::store::set_quarantine(&root, "b1", Some("suspect_oracle"))
            .await
            .unwrap();
        let ids = smoke_subset(&root).await.unwrap();
        assert!(
            !ids.contains(&"b1".to_string()),
            "quarantined excluded: {ids:?}"
        );

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn comment_body_lists_up_to_ten_failed_titles() {
        let mut results = Vec::new();
        for i in 0..12 {
            results.push(serde_json::json!({
                "id": format!("t{i}"),
                "title": format!("test {i}"),
                "passed": false,
                "error": "err",
            }));
        }
        let body = comment_body(&results, 12, 0, 12);
        assert_eq!(body.matches("- test").count(), 10);
    }
}
