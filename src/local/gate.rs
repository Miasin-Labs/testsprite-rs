//! `gate` — run the stored suite as a CI gate: emit JUnit XML + a JSON
//! summary, best-effort post a PR comment via the authenticated `gh` CLI,
//! and return a non-zero exit code if any test failed.

use std::path::Path;

use serde_json::Value;

use super::ts_dir;

/// Run every stored test, write `testsprite_tests/junit.xml` and
/// `testsprite_tests/gate-summary.json`, best-effort comment on the current
/// PR (via `gh`), and return `0` if every test passed, `1` otherwise. The
/// exit code depends only on test results — `gh` failures never propagate.
pub async fn gate(root: &Path, url_override: Option<&str>, model: &str) -> anyhow::Result<i32> {
    let results = crate::local::run::run_collect(root, &[], url_override, model, false, None, 1).await?;

    let total = results.len();
    let failed = results
        .iter()
        .filter(|r| !r.get("passed").and_then(Value::as_bool).unwrap_or(false))
        .count();
    let passed = total - failed;

    let dir = ts_dir(root);
    std::fs::create_dir_all(&dir)?;

    let junit = render_junit(&results, total, failed);
    std::fs::write(dir.join("junit.xml"), junit)?;

    let summary = serde_json::json!({
        "total": total,
        "passed": passed,
        "failed": failed,
        "results": results,
    });
    std::fs::write(dir.join("gate-summary.json"), serde_json::to_string_pretty(&summary)?)?;

    println!("gate: {passed}/{total} passed ({failed} failed) — junit.xml + gate-summary.json written");

    let body = comment_body(&results, total, passed, failed);
    try_gh_comment(root, &body);

    Ok(if failed == 0 { 0 } else { 1 })
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
        Ok(o) if o.status.success() => {
            String::from_utf8(o.stdout)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.get("number").and_then(Value::as_i64))
                .is_some()
        }
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
