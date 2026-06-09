//! Markdown report generation. Mirrors `generateMcpTestReport` in backendClient.ts.

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

/// Render one test case's markdown section.
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
    format!(
        "\n#### {title}\n- **Test Code:** [{stem}.py](./{stem}.py){err}\n- **Test Visualization and Result:** {viz}\n- **Status:** {status}\n- **Analysis / Findings:** {{{{TODO:AI_ANALYSIS}}}}.\n---\n"
    )
}

/// Build the `raw_report.md` content from finished test entities. The narrative
/// analysis sections are intentionally left as placeholders for the host LLM to
/// fill in (matching the original plugin's `llm.generate` next_action design).
pub fn build_raw_report(project_name: &str, test_type: TestType, results: &[TestEntity]) -> String {
    let total = results.len().max(1);
    let passed = results.iter().filter(|r| r.passed()).count();
    let pass_rate = (passed as f64 / total as f64) * 100.0;

    let sections: String = results.iter().map(render_case).collect();

    format!(
        "\n# TestSprite AI Testing Report (MCP)\n\n---\n\n## 1️⃣ Document Metadata\n\
         - **Project Name:** {project_name}\n- **Test Type:** {test_type}\n- **Prepared by:** TestSprite AI Team (testsprite-rs)\n\n---\n\n\
         ## 2️⃣ Requirement Validation Summary\n{sections}\n\n\
         ## 3️⃣ Coverage & Matching Metrics\n\n- **{pass_rate:.2}%** of tests passed\n\n---\n\n\
         ## 4️⃣ Key Gaps / Risks\n{{AI_GENERATED_KEY_GAPS_AND_RISKS}}\n---\n"
    )
}
