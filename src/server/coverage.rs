//! Coverage Guard — the architectural-soundness gate, sibling to jfc's Slop
//! Guard.
//!
//! Slop Guard answers "is this code slop?"; Coverage Guard answers "did the run
//! actually exercise the surface you declared?" It takes the declared surface
//! (endpoints / tools / functions from the PRD or code summary) and the set of
//! executed test entities, and flags:
//!   * surface elements with NO test touching them ("declared but untested"),
//!   * a coverage percentage,
//!   * runs where every test passed but coverage is partial (false confidence).
//!
//! It observes the pipeline's output; it is not an execution stage.

use serde_json::Value;

/// A single coverage finding (mirrors the shape of jfc's `SlopFinding`).
#[derive(Debug, Clone)]
pub struct CoverageFinding {
    pub rule: String,
    pub message: String,
    /// The surface element this is about (endpoint / tool / function).
    pub target: Option<String>,
}

/// A coverage report over (declared surface, executed cases).
#[derive(Debug, Clone)]
pub struct CoverageReport {
    pub declared: usize,
    pub covered: usize,
    pub findings: Vec<CoverageFinding>,
}

impl CoverageReport {
    pub fn percent(&self) -> f64 {
        if self.declared == 0 {
            100.0
        } else {
            (self.covered as f64 / self.declared as f64) * 100.0
        }
    }

    pub fn has_findings(&self) -> bool {
        !self.findings.is_empty()
    }
}

/// Extract the declared surface (stable identifiers) from a code summary / PRD.
/// Backends declare `api_endpoints`; the surface id is `METHOD path`.
pub fn declared_surface(code_summary: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(eps) = code_summary.get("api_endpoints").and_then(|v| v.as_array()) {
        for ep in eps {
            let method = ep
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_uppercase();
            if let Some(path) = ep.get("path").and_then(|v| v.as_str()) {
                out.push(format!("{method} {path}"));
            }
        }
    }
    // MCP tool surfaces declare `tools`.
    if let Some(tools) = code_summary.get("tools").and_then(|v| v.as_array()) {
        for t in tools {
            if let Some(name) = t
                .as_str()
                .or_else(|| t.get("name").and_then(|v| v.as_str()))
            {
                out.push(format!("tool:{name}"));
            }
        }
    }
    out
}

/// Decide whether an executed case (its title/description/code) touches a given
/// surface element. Cheap substring match on the surface's distinctive token
/// (the path or tool name).
fn case_touches(surface_id: &str, case_text: &str) -> bool {
    let token = surface_id
        .strip_prefix("tool:")
        .map(|t| t.to_string())
        .or_else(|| surface_id.split_whitespace().nth(1).map(|p| p.to_string()))
        .unwrap_or_else(|| surface_id.to_string());
    !token.is_empty() && case_text.contains(&token)
}

/// Build a coverage report: which declared surface elements were exercised by
/// the executed cases. `case_texts` are the title/description/code blobs.
pub fn evaluate(declared: &[String], case_texts: &[String]) -> CoverageReport {
    let mut findings = Vec::new();
    let mut covered = 0usize;
    for surface_id in declared {
        let touched = case_texts.iter().any(|t| case_touches(surface_id, t));
        if touched {
            covered += 1;
        } else {
            findings.push(CoverageFinding {
                rule: "uncovered_surface".into(),
                message: format!("`{surface_id}` is declared but no test exercises it"),
                target: Some(surface_id.clone()),
            });
        }
    }

    CoverageReport {
        declared: declared.len(),
        covered,
        findings,
    }
}

/// Format the report as concise markdown (mirrors `slop_guard::format_report`).
pub fn format_report(report: &CoverageReport) -> String {
    let mut out = format!(
        "Coverage Guard: {:.0}% ({}/{} surface elements exercised)\n",
        report.percent(),
        report.covered,
        report.declared
    );
    for f in &report.findings {
        out.push_str(&format!("  • [{}] {}\n", f.rule, f.message));
    }
    out
}

/// Sanitize a surface id into a Mermaid-safe node id.
fn mermaid_node_id(idx: usize, _surface_id: &str) -> String {
    format!("n{idx}")
}

/// Render the coverage map as a Mermaid `graph` (a fenced ```mermaid block that
/// renders natively in GitHub/markdown — no browser needed). Covered surface is
/// green, uncovered red, so the architectural gap is visible at a glance.
///
/// This is the high-value use of mermaid here: the Coverage Guard *emits* a
/// diagram of declared-vs-exercised surface. (If a PNG is ever wanted, the
/// browser executor's Playwright/Chromium rail can rasterize the same block.)
pub fn mermaid_diagram(declared: &[String], case_texts: &[String]) -> String {
    let mut out = String::from("```mermaid\ngraph LR\n  SUT[System Under Test]\n");
    for (i, surface_id) in declared.iter().enumerate() {
        let node = mermaid_node_id(i, surface_id);
        let covered = case_texts.iter().any(|t| case_touches(surface_id, t));
        let label = surface_id.replace('"', "'");
        let mark = if covered { "✓" } else { "✗" };
        out.push_str(&format!("  SUT --> {node}[\"{mark} {label}\"]\n"));
        let cls = if covered { "covered" } else { "uncovered" };
        out.push_str(&format!("  class {node} {cls};\n"));
    }
    out.push_str("  classDef covered fill:#d4edda,stroke:#28a745;\n");
    out.push_str("  classDef uncovered fill:#f8d7da,stroke:#dc3545;\n");
    out.push_str("```\n");
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn declared_surface_reads_endpoints_normal() {
        let cs = json!({ "api_endpoints": [
            { "method": "get", "path": "/health" },
            { "method": "POST", "path": "/api/todos" },
        ]});
        let s = declared_surface(&cs);
        assert_eq!(
            s,
            vec!["GET /health".to_string(), "POST /api/todos".to_string()]
        );
    }

    #[test]
    fn evaluate_flags_untested_surface_robust() {
        let declared = vec!["GET /health".to_string(), "POST /api/todos".to_string()];
        // Only /health is exercised.
        let cases = vec!["test GET /health returns ok".to_string()];
        let report = evaluate(&declared, &cases);
        assert_eq!(report.declared, 2);
        assert_eq!(report.covered, 1);
        assert_eq!(report.percent() as u32, 50);
        assert_eq!(report.findings.len(), 1);
        assert_eq!(
            report.findings[0].target.as_deref(),
            Some("POST /api/todos")
        );
    }

    #[test]
    fn evaluate_full_coverage_no_findings_normal() {
        let declared = vec!["GET /health".to_string()];
        let cases = vec!["hit /health".to_string()];
        let report = evaluate(&declared, &cases);
        assert!(!report.has_findings());
        assert_eq!(report.percent() as u32, 100);
    }
}
