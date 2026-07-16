//! Markdown report generation. Mirrors `generateMcpTestReport` in backendClient.ts.
//!
//! Two renderers share the pure helpers here: [`build_raw_report`] (the cloud
//! `TestEntity` path, which lacks failure-kind/analysis so it only gets the
//! deterministic analysis fallback) and [`build_local_report`] (the local path,
//! which carries `cause`/`fix`/`failureKind` and so renders the full official
//! report — requirement-grouped sections, per-failure severity, and a
//! per-requirement coverage matrix).

use serde_json::Value;

use crate::types::{TestEntity, TestType};

/// Sanitize a test title into a filename stem (alnum/underscore).
pub fn sanitize_filename(title: &str) -> String {
    title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

/// Debuggability-weighted severity of a failure, matching the levels the real
/// reports use. A sharp, localized failure (a missing route, a failed
/// assertion, an auth wall, a build break) is HIGH — it points straight at a
/// fix; a diffuse env/network failure is LOW. Basis is the same kind ranking
/// [`crate::local::triage`] uses.
pub fn severity_for(failure_kind: Option<&str>) -> &'static str {
    match failure_kind {
        Some("routing_404" | "assertion" | "auth" | "build_error") => "HIGH",
        Some(
            "suspect_oracle" | "residual_alignment" | "network_timeout" | "timeout"
            | "browser_crash",
        ) => "MEDIUM",
        _ => "LOW",
    }
}

/// Real Analysis/Findings prose — never a placeholder. On a pass it states the
/// assertions held; on a failure it leads with the diagnosed cause (and the
/// recommended fix when present), falling back to the raw error, then to a
/// generic line only when nothing at all was captured.
pub fn analysis_prose(
    passed: bool,
    cause: Option<&str>,
    fix: Option<&str>,
    error: Option<&str>,
) -> String {
    fn nonempty(o: Option<&str>) -> Option<&str> {
        o.map(str::trim).filter(|s| !s.is_empty())
    }
    if passed {
        return match nonempty(cause) {
            Some(c) => c.to_string(),
            None => "Test passed. All assertions succeeded and the expected behavior was verified."
                .to_string(),
        };
    }
    match (nonempty(cause), nonempty(fix)) {
        (Some(c), Some(f)) => format!("{c} Recommended fix: {f}"),
        (Some(c), None) => c.to_string(),
        (None, _) => match nonempty(error) {
            Some(e) => {
                let clipped: String = e.chars().take(200).collect();
                format!("Test failed: {clipped}")
            }
            None => "Test failed; no diagnostic was captured.".to_string(),
        },
    }
}

/// Render one cloud-path (`TestEntity`) case section. Uses the deterministic
/// analysis fallback (TestEntity carries no cause/fix), never a placeholder.
fn render_case(r: &TestEntity) -> String {
    let status = match r.test_status.as_deref() {
        Some("PASSED") => "✅ Passed",
        Some("FAILED") => "❌ Failed",
        Some(other) => other,
        None => "UNKNOWN",
    };
    let title = r.title.clone().unwrap_or_default();
    let stem = sanitize_filename(&title);
    let viz = match (&r.project_id, &r.test_id) {
        (Some(p), Some(t)) => crate::backend::dashboard_url(p, t),
        _ => String::new(),
    };
    let err = r
        .test_error
        .as_deref()
        .filter(|e| !e.is_empty())
        .map(|e| format!("\n- **Test Error:** {e}"))
        .unwrap_or_default();
    let analysis = analysis_prose(r.passed(), None, None, r.test_error.as_deref());
    format!(
        "\n#### {title}\n- **Test Code:** [{stem}.py](./{stem}.py){err}\n- **Test Visualization and Result:** {viz}\n- **Status:** {status}\n- **Analysis / Findings:** {analysis}\n---\n"
    )
}

/// Build the `raw_report.md` content from finished test entities. Uses real
/// deterministic analysis prose (no `{{TODO}}` placeholder).
pub fn build_raw_report(project_name: &str, test_type: TestType, results: &[TestEntity]) -> String {
    let total = results.len().max(1);
    let passed = results.iter().filter(|r| r.passed()).count();
    let pass_rate = (passed as f64 / total as f64) * 100.0;

    let sections: String = results.iter().map(render_case).collect();
    let gaps: Vec<&TestEntity> = results.iter().filter(|r| !r.passed()).collect();
    let gaps_md = if gaps.is_empty() {
        "No failing tests.\n".to_string()
    } else {
        gaps.iter()
            .map(|r| format!("- {}\n", r.title.clone().unwrap_or_default()))
            .collect()
    };

    format!(
        "\n# TestSprite AI Testing Report (MCP)\n\n---\n\n## 1️⃣ Document Metadata\n\
         - **Project Name:** {project_name}\n- **Test Type:** {test_type}\n- **Prepared by:** TestSprite AI Team (testsprite-rs)\n\n---\n\n\
         ## 2️⃣ Requirement Validation Summary\n{sections}\n\n\
         ## 3️⃣ Coverage & Matching Metrics\n\n- **{pass_rate:.2}%** of tests passed\n\n---\n\n\
         ## 4️⃣ Key Gaps / Risks\n{gaps_md}\n---\n"
    )
}

/// One requirement bucket in the local report.
struct RequirementGroup {
    name: String,
    num: usize,
    tests: Vec<Value>,
}

/// Group latest-result rows by their `requirement` (fallback `category`, else
/// `Ungrouped`). Numbered `R001..` by first appearance over id-sorted results;
/// the `Ungrouped` bucket always renders last so numbering is stable.
fn group_by_requirement(results: &[Value]) -> Vec<RequirementGroup> {
    let mut sorted: Vec<&Value> = results.iter().collect();
    sorted.sort_by_key(|r| {
        r.get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    });

    let mut order: Vec<String> = Vec::new();
    let mut buckets: std::collections::HashMap<String, Vec<Value>> =
        std::collections::HashMap::new();
    for r in sorted {
        let name = r
            .get("requirement")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                r.get("category")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("Ungrouped")
            .to_string();
        if !buckets.contains_key(&name) {
            order.push(name.clone());
        }
        buckets.entry(name).or_default().push(r.clone());
    }
    // Ungrouped renders last; everything else keeps first-appearance order.
    if let Some(i) = order.iter().position(|n| n == "Ungrouped") {
        let u = order.remove(i);
        order.push(u);
    }

    order
        .into_iter()
        .enumerate()
        .map(|(i, name)| RequirementGroup {
            tests: buckets.remove(&name).unwrap_or_default(),
            name,
            num: i + 1,
        })
        .collect()
}

/// A case id: kept verbatim when it already looks like `TC001`, else a
/// synthesized `TC{index:03}` for uuid-only local ids.
fn tc_label(id: &str, index: usize) -> String {
    let looks_like_tc =
        id.starts_with("TC") && id.len() > 2 && id[2..].chars().all(|c| c.is_ascii_digit());
    if looks_like_tc {
        id.to_string()
    } else {
        format!("TC{:03}", index + 1)
    }
}

/// Render the full official-format local report from the `report()` JSON value
/// (which carries per-result `cause`/`fix`/`failureKind`). Pure and
/// deterministic — no wall-clock, stable ordering — so it golden-tests cleanly.
pub fn build_local_report(v: &Value) -> String {
    let total = v.get("total").and_then(Value::as_u64).unwrap_or(0);
    let passed = v.get("passed").and_then(Value::as_u64).unwrap_or(0);
    let failed = v.get("failed").and_then(Value::as_u64).unwrap_or(0);
    let pass_rate = if total > 0 {
        100.0 * passed as f64 / total as f64
    } else {
        0.0
    };
    let project = v
        .get("projectName")
        .and_then(Value::as_str)
        .unwrap_or("Local Project");
    let results: Vec<Value> = v
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut out = String::new();
    out.push_str("# TestSprite AI Testing Report (MCP)\n\n---\n\n");
    out.push_str(&format!(
        "## 1️⃣ Document Metadata\n- **Project Name:** {project}\n\
         - **Total Tests Executed:** {total}\n- **Pass Rate:** {pass_rate:.2}%\n\n---\n\n"
    ));

    let groups = group_by_requirement(&results);
    out.push_str("## 2️⃣ Requirement Validation Summary\n");
    for g in &groups {
        out.push_str(&format!("\n### Requirement R{:03}: {}\n", g.num, g.name));
        for (i, r) in g.tests.iter().enumerate() {
            let id = r.get("id").and_then(Value::as_str).unwrap_or("");
            let title = r.get("title").and_then(Value::as_str).unwrap_or("");
            let passed = r.get("passed").and_then(Value::as_bool).unwrap_or(false);
            let verdict = r.get("verdict").and_then(Value::as_str).unwrap_or("");
            let fk = r.get("failureKind").and_then(Value::as_str);
            let cause = r.get("cause").and_then(Value::as_str);
            let fix = r.get("fix").and_then(Value::as_str);
            let error = r.get("error").and_then(Value::as_str);
            let status = if passed {
                "✅ Passed"
            } else if verdict == "blocked" {
                "🚫 Blocked"
            } else {
                "❌ Failed"
            };
            let stem = sanitize_filename(title);
            out.push_str(&format!("\n#### Test {}\n", tc_label(id, i)));
            out.push_str(&format!("- **Test Name:** {title}\n"));
            out.push_str(&format!("- **Test Code:** [{stem}.py](./{stem}.py)\n"));
            if let Some(e) = error.filter(|e| !e.is_empty()) {
                out.push_str(&format!("- **Test Error:** {}\n", e.replace('\n', " ")));
            }
            out.push_str(&format!("- **Status:** {status}\n"));
            if !passed {
                out.push_str(&format!("- **Severity:** {}\n", severity_for(fk)));
            }
            out.push_str(&format!(
                "- **Analysis / Findings:** {}\n---\n",
                analysis_prose(passed, cause, fix, error)
            ));
        }
    }

    out.push_str("\n## 3️⃣ Coverage & Matching Metrics\n\n");
    out.push_str(&format!("- **{pass_rate:.2}%** of tests passed\n\n"));
    out.push_str("| Requirement | Total Tests | ✅ Passed | ❌ Failed |\n");
    out.push_str("|---|---|---|---|\n");
    for g in &groups {
        let t = g.tests.len();
        let p = g
            .tests
            .iter()
            .filter(|r| r.get("passed").and_then(Value::as_bool).unwrap_or(false))
            .count();
        out.push_str(&format!(
            "| R{:03}: {} | {} | {} | {} |\n",
            g.num,
            md_cell(&g.name),
            t,
            p,
            t - p
        ));
    }
    out.push_str(&format!("| **Total** | {total} | {passed} | {failed} |\n"));

    out.push_str("\n---\n\n## 4️⃣ Key Gaps / Risks\n");
    let fails: Vec<&Value> = results
        .iter()
        .filter(|r| !r.get("passed").and_then(Value::as_bool).unwrap_or(false))
        .collect();
    if fails.is_empty() {
        out.push_str("No failing tests.\n");
    } else {
        for r in fails {
            let title = r.get("title").and_then(Value::as_str).unwrap_or("");
            let fk = r
                .get("failureKind")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            out.push_str(&format!("- [{fk}] {title}\n"));
        }
    }
    out
}

/// Escape a markdown table cell.
fn md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn severity_maps_failure_kinds_to_levels() {
        for k in ["routing_404", "assertion", "auth", "build_error"] {
            assert_eq!(severity_for(Some(k)), "HIGH", "{k}");
        }
        for k in [
            "timeout",
            "network_timeout",
            "browser_crash",
            "suspect_oracle",
        ] {
            assert_eq!(severity_for(Some(k)), "MEDIUM", "{k}");
        }
        for k in ["network", "infra", "dependency", "unknown"] {
            assert_eq!(severity_for(Some(k)), "LOW", "{k}");
        }
        assert_eq!(severity_for(None), "LOW");
    }

    #[test]
    fn analysis_prose_uses_cause_and_fix_and_never_placeholders() {
        let p = analysis_prose(
            false,
            Some("the route is missing"),
            Some("add the handler"),
            None,
        );
        assert!(p.contains("the route is missing"));
        assert!(p.contains("Recommended fix: add the handler"));
        assert!(!p.contains("TODO") && !p.contains("AI_ANALYSIS"));
        // Fallbacks.
        assert!(analysis_prose(false, None, None, Some("boom")).contains("boom"));
        assert!(analysis_prose(true, None, None, None).contains("passed"));
        assert!(analysis_prose(false, None, None, None).contains("no diagnostic"));
    }

    #[test]
    fn build_local_report_groups_numbers_and_totals() {
        let v = json!({
            "total": 3, "passed": 1, "failed": 2, "projectName": "Demo",
            "results": [
                {"id":"TC001","title":"login ok","passed":true,"verdict":"passed","requirement":"Auth API"},
                {"id":"TC002","title":"login 404","passed":false,"verdict":"failed","failureKind":"routing_404","requirement":"Auth API","cause":"route missing","error":"got 404"},
                {"id":"TC003","title":"list projects","passed":false,"verdict":"failed","failureKind":"assertion","requirement":"Projects","error":"expected 3 got 0"},
            ],
        });
        let md = build_local_report(&v);
        // Requirement grouping + numbering.
        assert!(md.contains("### Requirement R001: Auth API"), "{md}");
        assert!(md.contains("### Requirement R002: Projects"), "{md}");
        // Severity only on failures.
        assert!(md.contains("- **Severity:** HIGH"));
        // Real analysis, no placeholder.
        assert!(md.contains("route missing"));
        assert!(!md.contains("TODO"));
        // Coverage matrix + Total row.
        assert!(md.contains("| Requirement | Total Tests | ✅ Passed | ❌ Failed |"));
        assert!(md.contains("| **Total** | 3 | 1 | 2 |"), "{md}");
        // Deterministic.
        assert_eq!(md, build_local_report(&v));
    }

    #[test]
    fn build_local_report_ungrouped_when_no_requirement() {
        let v = json!({
            "total": 1, "passed": 1, "failed": 0,
            "results": [{"id":"x","title":"t","passed":true,"verdict":"passed"}],
        });
        let md = build_local_report(&v);
        assert!(md.contains("Ungrouped"), "{md}");
        // A passing test carries no Severity line.
        assert!(!md.contains("Severity"));
        // uuid-ish id got a synthesized TC label.
        assert!(md.contains("#### Test TC001"), "{md}");
    }

    #[test]
    fn raw_report_no_longer_emits_todo_placeholder() {
        use crate::types::{TestEntity, TestType};
        let e = TestEntity {
            project_id: Some("p".into()),
            test_id: Some("t".into()),
            user_id: None,
            title: Some("smoke".into()),
            description: None,
            code: None,
            test_status: Some("FAILED".into()),
            test_error: Some("boom".into()),
            test_visualization: None,
            modified: None,
        };
        let md = build_raw_report("Demo", TestType::Backend, &[e]);
        assert!(!md.contains("TODO:AI_ANALYSIS"));
        assert!(!md.contains("AI_GENERATED_KEY_GAPS"));
        assert!(md.contains("Test failed: boom"));
    }
}
