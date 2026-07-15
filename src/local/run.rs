//! Run local test cases through the existing Executor seam.

use std::path::Path;
use std::sync::Arc;

use crate::server::executors::ExecCtx;

use super::{project, store};

const DEFAULT_TARGET: &str = "http://127.0.0.1:8080";

/// Run the given test ids (all tests if `ids` is empty) against the local
/// project's target, printing pass/fail per test and a summary line.
/// Returns `0` if every test passed, `1` otherwise.
pub async fn run(root: &Path, ids: &[String], url_override: Option<&str>) -> anyhow::Result<i32> {
    let project = project::load(root)?;

    let target = url_override
        .map(str::to_string)
        .or_else(|| project.target_url.clone())
        .unwrap_or_else(|| DEFAULT_TARGET.to_string());

    let tests = if ids.is_empty() {
        store::list(root)?
    } else {
        ids.iter()
            .map(|id| store::load_one(root, id))
            .collect::<anyhow::Result<Vec<_>>>()?
    };

    if tests.is_empty() {
        println!("no tests found; run `testsprite-rs test add <file>` first");
        return Ok(0);
    }

    let ctx = ExecCtx {
        target: target.clone(),
        llm: None,
        prd: Arc::new(serde_json::json!({})),
    };

    let mut failed = 0;
    let total = tests.len();
    for t in &tests {
        let kind = t.kind.unwrap_or(project.kind);
        let ex = crate::server::executors::for_kind(kind);
        let case = serde_json::to_value(t)?;
        let outcome = ex.run(&case, &ctx).await;

        if outcome.passed {
            println!("PASS  {}  {}", t.id, t.title);
        } else {
            failed += 1;
            println!("FAIL  {}  {}", t.id, t.title);
            if !outcome.error.is_empty() {
                println!("      {}", outcome.error);
            }
        }

        store::write_result(root, &t.id, &outcome)?;
    }

    let passed = total - failed;
    println!("\n{passed}/{total} passed");

    Ok(if failed == 0 { 0 } else { 1 })
}
