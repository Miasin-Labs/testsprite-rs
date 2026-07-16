//! Generate local test cases with the LLM (PRD → plan → stored cases).
//! When the model can infer endpoints, generated cases may carry deterministic
//! `spec`/`steps` and run without per-run codegen; description-only cases still
//! fall back to LLM-generated code at execution time.
//!
//! The PRD and the test plan are persisted — the SQLite `prd` table plus the
//! `standard_prd.json` / `*_test_plan.json` files the original plugin writes —
//! so the whole flow (doc/summary → PRD → plan → cases) stays inspectable, not
//! just the leaf cases. Each generated case is stamped with the `prdId` it came
//! from.

use std::path::Path;

use serde_json::{Value, json};

use super::store;
use crate::server::executors::TestKind;
use crate::server::llm::LlmClient;

/// Result of a generate run: the persisted PRD id (when a PRD was produced) and
/// the stored test-case ids.
pub struct GenSummary {
    pub prd_id: Option<String>,
    pub test_ids: Vec<String>,
}

/// Result of adversarial QA planning: the cases the LLM proposed and the ids
/// stored when `--store` was set.
pub struct AuditSummary {
    pub cases: Vec<Value>,
    pub test_ids: Vec<String>,
}

/// Generate test cases from a code summary (`--from <file>`), a normalized PRD
/// distilled from an arbitrary doc (`--doc <file>`: README / notes / Jira ticket
/// / spec), or a plain instruction (`--instruction <text>`). Persists the PRD +
/// plan and the resulting cases (each stamped with its `prdId`).
pub async fn generate(
    root: &Path,
    from: Option<&Path>,
    instruction: Option<&str>,
    doc: Option<&Path>,
    model: &str,
    kind: Option<TestKind>,
) -> anyhow::Result<GenSummary> {
    // Structured API doc (Postman / OpenAPI / HAR) -> deterministic spec cases,
    // no LLM key needed. The fast, robust path: cases run via execute_spec
    // (reqwest) against the live target, not per-run LLM codegen.
    if let Some(dp) = doc {
        let text = read_doc(dp).await?;
        if let Some(ex) = crate::local::apidoc::extract(&text) {
            eprintln!(
                "generate: parsed {} endpoint(s) from {} ({} format) — deterministic spec cases (no LLM)",
                ex.cases.len(),
                dp.display(),
                ex.format
            );
            let source = format!("doc:{} ({})", dp.display(), ex.format);
            let prd_id =
                persist_prd(root, &source, &ex.prd, &ex.cases, Some(TestKind::Backend)).await?;
            let test_ids =
                store_cases(root, ex.cases, Some(TestKind::Backend), Some(&prd_id)).await?;
            return Ok(GenSummary {
                prd_id: Some(prd_id),
                test_ids,
            });
        }
        // Unstructured doc -> LLM normalization (needs a key).
        let Some(llm) = LlmClient::from_env(model) else {
            anyhow::bail!(
                "{} isn't a recognized Postman/OpenAPI/HAR doc, and LLM fallback needs an OpenAI key (set OPENAI_API_KEY)",
                dp.display()
            )
        };
        let prd = llm.generate_prd_from_doc(&text).await?;
        let cases = llm.generate_plan(&prd).await?;
        let prd_id =
            persist_prd(root, &format!("doc:{}", dp.display()), &prd, &cases, kind).await?;
        let test_ids = store_cases(root, cases, kind, Some(&prd_id)).await?;
        return Ok(GenSummary {
            prd_id: Some(prd_id),
            test_ids,
        });
    }

    // --from / --instruction -> LLM (needs a key).
    let Some(llm) = LlmClient::from_env(model) else {
        anyhow::bail!(
            "test generate needs an OpenAI key — set OPENAI_API_KEY or ~/.config/jfc/credentials.toml [openai].api_key"
        )
    };
    let (summary, source) = if let Some(p) = from.filter(|p| p.is_file()) {
        let body = std::fs::read_to_string(p)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?;
        let value: Value = serde_json::from_str(&body)
            .or_else(|_| serde_yaml::from_str(&body))
            .map_err(|e| anyhow::anyhow!("parsing {} as JSON/YAML: {e}", p.display()))?;
        if !value.is_object() {
            anyhow::bail!("{} does not contain a JSON object", p.display());
        }
        (value, format!("from:{}", p.display()))
    } else if let Some(instruction) = instruction {
        (
            json!({ "project_name": "local", "description": instruction }),
            format!(
                "instruction:{}",
                instruction.chars().take(80).collect::<String>()
            ),
        )
    } else if let Some(p) = from {
        anyhow::bail!(
            "--from expects a code-summary JSON file, not a directory ({}); pass --instruction instead",
            p.display()
        )
    } else {
        anyhow::bail!("pass --from <code_summary.json>, --doc <file>, or --instruction <text>")
    };
    let prd = llm.generate_prd(&summary).await?;
    let cases = llm.generate_plan(&prd).await?;
    let prd_id = persist_prd(root, &source, &prd, &cases, kind).await?;
    let test_ids = store_cases(root, cases, kind, Some(&prd_id)).await?;
    Ok(GenSummary {
        prd_id: Some(prd_id),
        test_ids,
    })
}

/// Persist the PRD + plan to SQLite and mirror them to the on-disk artifact
/// files (`standard_prd.json`, `testsprite_{backend,frontend}_test_plan.json`)
/// the original plugin writes. Returns the new prd id.
async fn persist_prd(
    root: &Path,
    source: &str,
    prd: &Value,
    plan: &[Value],
    kind: Option<TestKind>,
) -> anyhow::Result<String> {
    let id = store::save_prd(root, source, prd, plan).await?;
    // Best-effort file mirror (never fail generation over a file write).
    let paths = crate::paths::Paths::new(root);
    let _ = std::fs::create_dir_all(paths.dir());
    let _ = std::fs::write(
        paths.standard_prd(),
        serde_json::to_string_pretty(prd).unwrap_or_default(),
    );
    let plan_path = match kind {
        Some(TestKind::Frontend) => paths.frontend_test_plan(),
        _ => paths.backend_test_plan(),
    };
    let _ = std::fs::write(
        plan_path,
        serde_json::to_string_pretty(&Value::Array(plan.to_vec())).unwrap_or_default(),
    );
    Ok(id)
}

/// Generate a test case per function found by the structural coverage surface
/// under `path`. No PRD is produced (functions → cases directly).
pub async fn generate_cover(root: &Path, path: &Path, model: &str) -> anyhow::Result<GenSummary> {
    let units = crate::local::coverage::structural_surface(path)?;
    if units.is_empty() {
        anyhow::bail!("no functions found under {}", path.display());
    }
    generate_for_units(root, &units, model, "test generate --cover").await
}

/// Code Diff Mode: generate a test for each function CHANGED since `since` that
/// no stored test already covers. No PRD (functions → cases directly).
pub async fn generate_changed(root: &Path, since: &str, model: &str) -> anyhow::Result<GenSummary> {
    let changed = crate::local::changed::changed_surface(root, since)?;
    let targets = crate::local::changed::uncovered_changed_units(root, &changed).await?;
    if targets.is_empty() {
        return Ok(GenSummary {
            prd_id: None,
            test_ids: Vec::new(),
        });
    }
    generate_for_units(root, &targets, model, "test generate --changed").await
}

/// Ask the TestSprite LLM to adversarially generate high-signal QA cases from
/// the current code summary, stored suite, latest results, and coverage gaps.
pub async fn adversarial(
    root: &Path,
    scan: &Path,
    model: &str,
    store: bool,
) -> anyhow::Result<AuditSummary> {
    let Some(llm) = LlmClient::from_env(model) else {
        anyhow::bail!(
            "test audit needs an OpenAI key — set OPENAI_API_KEY or ~/.config/jfc/credentials.toml [openai].api_key"
        )
    };
    let summary = crate::local::summary::generate(root).unwrap_or_else(|_| json!({}));
    let stored_tests = store::export_all(root).await.unwrap_or_default();
    let latest_results = store::latest_results(root).await.unwrap_or_default();
    let coverage = crate::local::coverage::gaps(root, scan)
        .await
        .ok()
        .and_then(|g| serde_json::to_value(g).ok())
        .unwrap_or(Value::Null);
    let context = json!({
        "code_summary": summary,
        "stored_tests": stored_tests,
        "latest_results": latest_results,
        "coverage_gaps": coverage,
    });
    let cases = llm.generate_adversarial_tests(&context).await?;
    let test_ids = if store {
        store_cases(root, cases.clone(), None, None).await?
    } else {
        Vec::new()
    };
    Ok(AuditSummary { cases, test_ids })
}

/// Shared: ask the LLM for one case per unit (capped at 40) and store them.
async fn generate_for_units(
    root: &Path,
    units: &[crate::local::coverage::Unit],
    model: &str,
    what: &str,
) -> anyhow::Result<GenSummary> {
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
    let test_ids = store_cases(root, cases, None, None).await?;
    Ok(GenSummary {
        prd_id: None,
        test_ids,
    })
}

/// Store generated cases, tagging `kind` and (when set) the originating
/// `prd_id` so each case links back to the PRD it came from. Returns the ids.
async fn store_cases(
    root: &Path,
    cases: Vec<Value>,
    kind: Option<TestKind>,
    prd_id: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let mut ids = Vec::new();
    for mut case in cases {
        if !case.is_object() {
            continue;
        }
        if let Some(k) = kind {
            case["kind"] = serde_json::to_value(k)?;
        }
        if let Some(pid) = prd_id {
            case["prdId"] = json!(pid);
        }
        let id = store::add_value(root, case).await?;
        ids.push(id);
    }
    Ok(ids)
}

/// Read a `--doc` source: fetch it when it's an `http(s)` URL (e.g. a utoipa
/// app's served `/api-docs/openapi.json`), else read the local file.
async fn read_doc(dp: &Path) -> anyhow::Result<String> {
    let s = dp.to_string_lossy();
    if s.starts_with("http://") || s.starts_with("https://") {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()?;
        let resp = client
            .get(s.as_ref())
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("fetching {s}: {e}"))?;
        if !resp.status().is_success() {
            anyhow::bail!("{s} returned HTTP {}", resp.status());
        }
        return Ok(resp.text().await?);
    }
    if !dp.is_file() {
        anyhow::bail!(
            "--doc expects a readable file or http(s) URL ({})",
            dp.display()
        );
    }
    std::fs::read_to_string(dp).map_err(|e| anyhow::anyhow!("reading {}: {e}", dp.display()))
}
