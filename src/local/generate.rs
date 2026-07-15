//! Generate local test cases with the LLM (PRD → plan → stored cases).
//! Generated cases have no `spec`, so `test run` LLM-generates their code.

use std::path::Path;

use crate::server::executors::TestKind;

use super::store;

/// Generate test cases from a code summary (`--from <file>`) or a plain
/// instruction (`--instruction <text>`), store them, and return their ids.
pub async fn generate(
    root: &Path,
    from: Option<&Path>,
    instruction: Option<&str>,
    model: &str,
    kind: Option<TestKind>,
) -> anyhow::Result<Vec<String>> {
    let Some(llm) = crate::server::llm::LlmClient::from_env(model) else {
        anyhow::bail!(
            "test generate needs an OpenAI key — set OPENAI_API_KEY or ~/.config/jfc/credentials.toml [openai].api_key"
        )
    };

    let summary = if let Some(from) = from {
        let body = std::fs::read_to_string(from)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", from.display()))?;
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", from.display()))?;
        if !value.is_object() {
            anyhow::bail!("{} does not contain a JSON object", from.display());
        }
        value
    } else if let Some(instruction) = instruction {
        serde_json::json!({ "project_name": "local", "description": instruction })
    } else {
        anyhow::bail!("pass --from <code_summary.json> or --instruction <text>")
    };

    let prd = llm.generate_prd(&summary).await?;
    let cases = llm.generate_plan(&prd).await?;

    let mut ids = Vec::new();
    for mut case in cases {
        if !case.is_object() {
            continue;
        }
        if let Some(k) = kind {
            case["kind"] = serde_json::to_value(k)?;
        }
        let id = store::add_value(root, case)?;
        ids.push(id);
    }
    Ok(ids)
}
