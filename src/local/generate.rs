//! Generate local test cases with the LLM (PRD → plan → stored cases).
//! Generated cases have no `spec`, so `test run` LLM-generates their code.

use std::path::Path;

use serde_json::{Value, json};

use crate::server::executors::TestKind;
use crate::server::llm::LlmClient;

use super::store;

/// Generate test cases from a code summary (`--from <file>`), a normalized PRD
/// distilled from an arbitrary doc (`--doc <file>`: README / notes / Jira ticket
/// / spec), or a plain instruction (`--instruction <text>`). Stores them and
/// returns their ids.
pub async fn generate(
    root: &Path,
    from: Option<&Path>,
    instruction: Option<&str>,
    doc: Option<&Path>,
    model: &str,
    kind: Option<TestKind>,
) -> anyhow::Result<Vec<String>> {
    let Some(llm) = LlmClient::from_env(model) else {
        anyhow::bail!(
            "test generate needs an OpenAI key — set OPENAI_API_KEY or ~/.config/jfc/credentials.toml [openai].api_key"
        )
    };

    let prd = if let Some(dp) = doc {
        if !dp.is_file() {
            anyhow::bail!("--doc expects a readable file ({})", dp.display());
        }
        let text = std::fs::read_to_string(dp)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", dp.display()))?;
        llm.generate_prd_from_doc(&text).await?
    } else {
        let summary = if let Some(p) = from.filter(|p| p.is_file()) {
            let body = std::fs::read_to_string(p)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?;
            let value: Value = serde_json::from_str(&body)
                .map_err(|e| anyhow::anyhow!("parsing {}: {e}", p.display()))?;
            if !value.is_object() {
                anyhow::bail!("{} does not contain a JSON object", p.display());
            }
            value
        } else if let Some(instruction) = instruction {
            json!({ "project_name": "local", "description": instruction })
        } else if let Some(p) = from {
            anyhow::bail!(
                "--from expects a code-summary JSON file, not a directory ({}); pass --instruction instead",
                p.display()
            )
        } else {
            anyhow::bail!("pass --from <code_summary.json>, --doc <file>, or --instruction <text>")
        };
        llm.generate_prd(&summary).await?
    };

    let cases = llm.generate_plan(&prd).await?;
    store_cases(root, cases, kind).await
}

/// Generate a test case per function found by the structural coverage surface
/// under `path`, targeting each function's inputs/outputs and control-flow
/// branches. Stores the cases and returns their ids.
pub async fn generate_cover(root: &Path, path: &Path, model: &str) -> anyhow::Result<Vec<String>> {
    let units = crate::local::coverage::structural_surface(path)?;
    if units.is_empty() {
        anyhow::bail!("no functions found under {}", path.display());
    }
    generate_for_units(root, &units, model, "test generate --cover").await
}

/// Code Diff Mode: generate a test for each function CHANGED since `since` (per
/// `git diff`) that no stored test already covers. Returns the new ids — empty
/// when nothing changed or every changed function is already covered.
pub async fn generate_changed(root: &Path, since: &str, model: &str) -> anyhow::Result<Vec<String>> {
    let changed = crate::local::changed::changed_surface(root, since)?;
    let targets = crate::local::changed::uncovered_changed_units(root, &changed).await?;
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    generate_for_units(root, &targets, model, "test generate --changed").await
}

/// Shared: ask the LLM for one case per unit (capped at 40) and store them.
async fn generate_for_units(
    root: &Path,
    units: &[crate::local::coverage::Unit],
    model: &str,
    what: &str,
) -> anyhow::Result<Vec<String>> {
    let functions = Value::Array(
        units
            .iter()
            .take(40)
            .map(|u| json!({"name": u.name, "file": u.file, "branches": u.branches}))
            .collect(),
    );
    let Some(llm) = LlmClient::from_env(model) else {
        anyhow::bail!(
            "{what} needs an OpenAI key — set OPENAI_API_KEY or ~/.config/jfc/credentials.toml [openai].api_key"
        )
    };
    let cases = llm.generate_from_functions(&functions).await?;
    store_cases(root, cases, None).await
}

/// Store generated cases, tagging `kind` when given. Returns the stored ids.
async fn store_cases(
    root: &Path,
    cases: Vec<Value>,
    kind: Option<TestKind>,
) -> anyhow::Result<Vec<String>> {
    let mut ids = Vec::new();
    for mut case in cases {
        if !case.is_object() {
            continue;
        }
        if let Some(k) = kind {
            case["kind"] = serde_json::to_value(k)?;
        }
        let id = store::add_value(root, case).await?;
        ids.push(id);
    }
    Ok(ids)
}
