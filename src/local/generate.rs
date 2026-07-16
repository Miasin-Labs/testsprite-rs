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
use crate::server::llm::{LlmClient, Perspective};

/// Knobs shared by every generation entry point.
#[derive(Debug, Clone)]
pub struct GenOpts {
    /// Total LLM token cap for this command; generation stops (keeping what it
    /// already produced) once the ledger reaches it. `None` = unlimited.
    pub budget: Option<u64>,
    /// Acceptance-screen LLM-generated cases against the current baseline
    /// before they count as tests (deterministic doc/summary cases skip it —
    /// they carry no hallucinated oracle).
    pub gate: bool,
    /// Perspectives for unit-based generation (`--cover` / `--changed`).
    pub views: Vec<Perspective>,
    /// Coverage-feedback rounds for `--cover` (1 = single-shot, the default):
    /// after each round re-measure real coverage, keep only functions still
    /// uncovered, and regenerate for them until coverage stops improving or the
    /// cap is hit.
    pub iterate: usize,
}

impl Default for GenOpts {
    fn default() -> Self {
        Self {
            budget: None,
            gate: true,
            views: Perspective::ALL.to_vec(),
            iterate: 1,
        }
    }
}

/// Result of a generate run: the persisted PRD id (when a PRD was produced),
/// the stored test-case ids, and the subset the acceptance gate quarantined.
pub struct GenSummary {
    pub prd_id: Option<String>,
    pub test_ids: Vec<String>,
    pub quarantined: Vec<String>,
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
    opts: &GenOpts,
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
                quarantined: Vec::new(),
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
        let quarantined = screen_if(opts, root, &test_ids, model).await?;
        return Ok(GenSummary {
            prd_id: Some(prd_id),
            test_ids,
            quarantined,
        });
    }

    // Code summary with `api_endpoints` -> deterministic backend spec cases,
    // no LLM key needed. This is the local version of TestSprite's
    // "generate code summary -> generate test plan" path when the summary
    // already contains a runnable API surface.
    if let Some(p) = from.filter(|p| p.is_file()) {
        let summary = read_summary_value(p)?;
        let mut planned = crate::server::engine::plan_from_code_summary(&summary);
        if !planned.is_empty() {
            // Deterministic boundary probes ride along when the boundary view
            // is enabled (it is by default): edge path params + malformed
            // bodies, asserting "must not 5xx" — zero LLM spend.
            if opts.views.contains(&Perspective::Boundary) {
                planned.extend(crate::server::engine::boundary_cases(&summary));
            }
            let cases: Vec<Value> = planned
                .into_iter()
                .map(|case| {
                    json!({
                        "id": case.id,
                        "title": case.title,
                        "description": case.description,
                        "kind": "backend",
                        "spec": case.spec,
                    })
                })
                .collect();
            let prd = crate::server::engine::prd_from_code_summary(&summary);
            let source = format!("from:{} (deterministic endpoints)", p.display());
            let prd_id = persist_prd(root, &source, &prd, &cases, Some(TestKind::Backend)).await?;
            let test_ids = store_cases(root, cases, Some(TestKind::Backend), Some(&prd_id)).await?;
            return Ok(GenSummary {
                prd_id: Some(prd_id),
                test_ids,
                quarantined: Vec::new(),
            });
        }
    }

    // --from / --instruction -> LLM (needs a key).
    let Some(llm) = LlmClient::from_env(model) else {
        anyhow::bail!(
            "test generate needs an OpenAI key — set OPENAI_API_KEY or ~/.config/jfc/credentials.toml [openai].api_key"
        )
    };
    let (summary, source) = if let Some(p) = from.filter(|p| p.is_file()) {
        (read_summary_value(p)?, format!("from:{}", p.display()))
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
    let quarantined = screen_if(opts, root, &test_ids, model).await?;
    Ok(GenSummary {
        prd_id: Some(prd_id),
        test_ids,
        quarantined,
    })
}

/// Run the acceptance gate over freshly stored LLM-generated cases when the
/// opts ask for it; returns the quarantined ids.
async fn screen_if(
    opts: &GenOpts,
    root: &Path,
    ids: &[String],
    model: &str,
) -> anyhow::Result<Vec<String>> {
    if !opts.gate || ids.is_empty() {
        return Ok(Vec::new());
    }
    Ok(crate::local::accept::screen(root, ids, model)
        .await?
        .quarantined)
}

fn read_summary_value(p: &Path) -> anyhow::Result<Value> {
    let body =
        std::fs::read_to_string(p).map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?;
    let value: Value = serde_json::from_str(&body)
        .or_else(|_| serde_yaml::from_str(&body))
        .map_err(|e| anyhow::anyhow!("parsing {} as JSON/YAML: {e}", p.display()))?;
    if !value.is_object() {
        anyhow::bail!("{} does not contain a JSON object", p.display());
    }
    Ok(value)
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
pub async fn generate_cover(
    root: &Path,
    path: &Path,
    model: &str,
    opts: &GenOpts,
) -> anyhow::Result<GenSummary> {
    let units = crate::local::coverage::structural_surface(path)?;
    if units.is_empty() {
        anyhow::bail!("no functions found under {}", path.display());
    }
    if opts.iterate <= 1 {
        return generate_for_units(root, path, &units, model, "test generate --cover", opts).await;
    }
    generate_cover_iterate(root, path, model, opts).await
}

/// Coverage-feedback loop: generate, screen, re-measure real coverage, and
/// regenerate only for functions STILL uncovered — stopping when a round adds
/// no newly-covered function (a plateau) or the round cap / token budget is
/// hit. Re-measuring is what turns "one blind pass" into "close the gaps that
/// remain": a single pass predictably misses deep branches, and nothing else
/// verifies a generated test actually covered what it targeted.
async fn generate_cover_iterate(
    root: &Path,
    path: &Path,
    model: &str,
    opts: &GenOpts,
) -> anyhow::Result<GenSummary> {
    let mut all_ids = Vec::new();
    let mut all_quarantined = Vec::new();
    let mut prev_uncovered = usize::MAX;

    for round in 1..=opts.iterate {
        let gaps = crate::local::coverage::gaps(root, path).await?;
        let remaining = gaps.uncovered;
        if remaining.is_empty() {
            eprintln!("cover --iterate: round {round}: nothing uncovered — done");
            break;
        }
        // A round that closed nothing since last time is a plateau: the model
        // cannot reach the residual functions (unreachable/infeasible paths),
        // so keep spending is waste.
        if remaining.len() >= prev_uncovered {
            eprintln!(
                "cover --iterate: round {round}: {} still uncovered, no progress last round — stopping",
                remaining.len()
            );
            break;
        }
        prev_uncovered = remaining.len();

        let one_round = GenOpts {
            iterate: 1,
            ..opts.clone()
        };
        eprintln!(
            "cover --iterate: round {round}/{}: generating for {} uncovered function(s)",
            opts.iterate,
            remaining.len()
        );
        let summary = generate_for_units(
            root,
            path,
            &remaining,
            model,
            "test generate --cover",
            &one_round,
        )
        .await?;
        all_ids.extend(summary.test_ids);
        all_quarantined.extend(summary.quarantined);
    }

    Ok(GenSummary {
        prd_id: None,
        test_ids: all_ids,
        quarantined: all_quarantined,
    })
}

/// Code Diff Mode: generate a test for each function CHANGED since `since` that
/// no stored test already covers. No PRD (functions → cases directly).
pub async fn generate_changed(
    root: &Path,
    since: &str,
    model: &str,
    opts: &GenOpts,
) -> anyhow::Result<GenSummary> {
    let changed = crate::local::changed::changed_surface(root, since)?;
    let targets = crate::local::changed::uncovered_changed_units(root, &changed).await?;
    if targets.is_empty() {
        return Ok(GenSummary {
            prd_id: None,
            test_ids: Vec::new(),
            quarantined: Vec::new(),
        });
    }
    generate_for_units(root, root, &targets, model, "test generate --changed", opts).await
}

/// [`generate_changed`] plus a cross-version fault-check: each generated
/// `rust`/`command` case is run against a scratch worktree of the base
/// revision AND the current tree. A case is kept only if it FAILS on the base
/// and PASSES on HEAD (it genuinely detects the change). The others are
/// classified, not silently kept:
/// - pass-on-both → `suspect_oracle` (asserts nothing the change affected),
/// - fail-on-both → `residual_alignment` (encodes stale/pre-change semantics),
/// - unevaluable on the base (non-local target, worktree unavailable) → kept
///   as-is with a note, exactly like the acceptance gate's blocked path.
pub async fn generate_changed_fault_checked(
    root: &Path,
    since: &str,
    model: &str,
    opts: &GenOpts,
) -> anyhow::Result<GenSummary> {
    let summary = generate_changed(root, since, model, opts).await?;
    if summary.test_ids.is_empty() {
        return Ok(summary);
    }
    let base_rev = merge_base(root, since).unwrap_or_else(|| since.to_string());
    let quarantined =
        crate::local::accept::fault_check(root, &summary.test_ids, &base_rev, model).await?;
    Ok(GenSummary {
        quarantined,
        ..summary
    })
}

/// The merge-base of HEAD and `since` — the revision the change diverged from,
/// the honest "before" for a fault-check. Falls back to `None` (caller uses
/// `since` directly) when git can't compute it.
fn merge_base(root: &Path, since: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-base", "HEAD", since])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Ask the TestSprite LLM to adversarially generate high-signal QA cases from
/// the current code summary, stored suite, latest results, and coverage gaps.
pub async fn adversarial(
    root: &Path,
    scan: &Path,
    model: &str,
    store: bool,
    opts: &GenOpts,
) -> anyhow::Result<AuditSummary> {
    if crate::server::llm::resolve_key().is_none() {
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
    let mut cases = Vec::new();
    let mut errors = Vec::new();
    // Every per-model client shares one command-wide budget: track spend
    // across them by summing, since each client has its own ledger.
    let mut spent: u64 = 0;
    for model in model_list(model) {
        if opts.budget.is_some_and(|b| spent >= b) {
            eprintln!("audit: token budget reached ({spent} spent) — skipping model {model}");
            continue;
        }
        let Some(llm) = LlmClient::from_env(&model) else {
            continue;
        };
        match llm.generate_adversarial_tests(&context).await {
            Ok(mut proposed) => {
                for c in &mut proposed {
                    if c.is_object() {
                        c["modelSource"] = json!(model);
                    }
                }
                cases.extend(proposed);
            }
            Err(e) => errors.push(format!("{model}: {e}")),
        }
        spent += llm.usage().total_tokens();
    }
    // Merge with consensus votes across models, then drop cases already
    // covered by the stored suite (novelty guard).
    cases = dedupe_cases(cases);
    if cases.is_empty() && !errors.is_empty() {
        anyhow::bail!("adversarial planning failed: {}", errors.join("; "));
    }
    let test_ids = if store {
        let (fresh, dropped) = drop_known_duplicates(root, cases.clone()).await;
        if dropped > 0 {
            eprintln!("audit: dropped {dropped} case(s) duplicating existing stored tests");
        }
        let ids = store_cases(root, fresh, None, None).await?;
        let first_model = model_list(model)
            .into_iter()
            .next()
            .unwrap_or_else(|| model.to_string());
        screen_if(opts, root, &ids, &first_model).await?;
        ids
    } else {
        Vec::new()
    };
    Ok(AuditSummary { cases, test_ids })
}

fn model_list(model: &str) -> Vec<String> {
    model
        .split(',')
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>()
}

/// Merge cases proposed by (possibly) several models, keeping the first of
/// each title but recording a CONSENSUS signal: how many distinct models
/// proposed it, stamped as `modelVotes` (+ `consensus: true` at >= 2). When
/// independent models converge on the same adversarial case it is far more
/// likely a real defect vector than a single model's guess — the vote lets the
/// caller (and a reviewer) rank by agreement. Single-model runs are unaffected
/// (every case gets `modelVotes: 1`).
fn dedupe_cases(cases: Vec<Value>) -> Vec<Value> {
    use std::collections::BTreeMap;
    // Preserve first-seen order while tallying distinct model sources per key.
    let mut order: Vec<String> = Vec::new();
    let mut first: BTreeMap<String, Value> = BTreeMap::new();
    let mut voters: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for case in cases {
        let key = case
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| case.to_string());
        let model = case
            .get("modelSource")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !first.contains_key(&key) {
            order.push(key.clone());
            first.insert(key.clone(), case);
        }
        voters.entry(key).or_default().insert(model);
    }
    order
        .into_iter()
        .map(|key| {
            let mut case = first.remove(&key).unwrap();
            let votes = voters.get(&key).map(|v| v.len()).unwrap_or(1);
            if case.is_object() {
                case["modelVotes"] = json!(votes);
                if votes >= 2 {
                    case["consensus"] = json!(true);
                }
            }
            case
        })
        .collect()
}

/// Drop generated cases that merely reproduce an EXISTING stored test —
/// exact content-hash match, or a normalized-title near-duplicate above a
/// similarity threshold. Only ~10% of generated tests uniquely contribute, so
/// admitting regurgitated/memorized ones bloats the suite with redundant
/// low-value cases. Returns `(kept, dropped_count)`.
async fn drop_known_duplicates(root: &Path, cases: Vec<Value>) -> (Vec<Value>, usize) {
    let existing = store::export_all(root).await.unwrap_or_default();
    let existing_hashes: std::collections::BTreeSet<String> = existing
        .iter()
        .filter_map(|t| serde_json::to_string(t).ok())
        .map(|s| store::content_hash(&s))
        .collect();
    let existing_titles: Vec<String> = existing
        .iter()
        .filter_map(|t| t.get("title").and_then(Value::as_str))
        .map(normalize_title)
        .collect();

    let before = cases.len();
    let kept: Vec<Value> = cases
        .into_iter()
        .filter(|c| {
            if let Ok(s) = serde_json::to_string(c)
                && existing_hashes.contains(&store::content_hash(&s))
            {
                return false;
            }
            let title = c
                .get("title")
                .and_then(Value::as_str)
                .map(normalize_title)
                .unwrap_or_default();
            if title.is_empty() {
                return true;
            }
            // A title >85% similar to an existing test's is a near-duplicate.
            !existing_titles
                .iter()
                .any(|e| title_similarity(&title, e) > 0.85)
        })
        .collect();
    let dropped = before - kept.len();
    (kept, dropped)
}

fn normalize_title(t: &str) -> String {
    // Lowercase and split on any non-alphanumeric run, so punctuation and path
    // slashes don't make "GET /todos responds" and "get todos responds" look
    // like different tests.
    t.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Token-set Jaccard over two normalized titles — a cheap near-duplicate
/// signal that ignores word order and punctuation.
fn title_similarity(a: &str, b: &str) -> f64 {
    let sa: std::collections::BTreeSet<&str> = a.split_whitespace().collect();
    let sb: std::collections::BTreeSet<&str> = b.split_whitespace().collect();
    if sa.is_empty() && sb.is_empty() {
        return 1.0;
    }
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Shared: ask the LLM for one case per unit (capped at 40) per configured
/// perspective (normal/boundary/exception), merge + dedupe the views, store
/// them, and acceptance-screen the result.
async fn generate_for_units(
    root: &Path,
    scan: &Path,
    units: &[crate::local::coverage::Unit],
    model: &str,
    what: &str,
    opts: &GenOpts,
) -> anyhow::Result<GenSummary> {
    // Focal context: attach each function's real source (truncated) so the
    // model grounds inputs/outputs/branches in what the code actually does
    // instead of hallucinating from the bare name. Two caps bound the prompt:
    // per-function and total across the batch.
    const PER_FN_SOURCE_CAP: usize = 1_200;
    const TOTAL_SOURCE_CAP: usize = 48_000;
    let picked: Vec<_> = units.iter().take(40).collect();
    let picked_owned: Vec<crate::local::coverage::Unit> =
        picked.iter().map(|u| (*u).clone()).collect();
    let sources = crate::local::coverage::function_sources(scan, &picked_owned, PER_FN_SOURCE_CAP);
    // Path targeting: the explicit list of conditions each function branches on,
    // so the prompt can ask for inputs that make each one both true and false
    // (per-branch, not just per-function).
    let conditions = crate::local::coverage::function_branch_conditions(scan, &picked_owned);
    let mut source_budget = TOTAL_SOURCE_CAP;
    let functions = Value::Array(
        picked
            .iter()
            .map(|u| {
                let mut f = json!({"name": u.name, "file": u.file, "branches": u.branches});
                if let Some(src) = sources.get(&(u.file.clone(), u.line))
                    && src.len() <= source_budget
                {
                    source_budget -= src.len();
                    f["source"] = json!(src);
                }
                if let Some(conds) = conditions.get(&(u.file.clone(), u.line)) {
                    f["branch_conditions"] = json!(conds);
                }
                f
            })
            .collect(),
    );
    let Some(llm) = LlmClient::from_env(model) else {
        anyhow::bail!(
            "{what} needs an OpenAI key — set OPENAI_API_KEY or ~/.config/jfc/credentials.toml [openai].api_key"
        )
    };
    // Few-shot exemplars: the repo's own most-related stored tests, mined by
    // dependency-set overlap with the targets, so generation mirrors the
    // project's real setup/argument/return shapes instead of inventing them.
    let exemplars = {
        let vocab: Vec<String> = crate::local::coverage::structural_surface(scan)
            .unwrap_or_default()
            .into_iter()
            .map(|u| u.name)
            .collect();
        let targets: Vec<String> = picked.iter().map(|u| u.name.clone()).collect();
        let tests = store::list(root).await.unwrap_or_default();
        crate::local::retrieval::exemplars_for(&vocab, &targets, &tests, 3)
    };
    // Prevention corpus: this project's own recurring failures as do/don't
    // guidance, so generation stops re-making the mistakes triage already saw.
    let prevention = crate::local::guidelines::render_prompt_block(
        &crate::local::guidelines::mine(root, 8)
            .await
            .unwrap_or_default(),
    );
    let mut cases = Vec::new();
    let mut errors = Vec::new();
    for view in &opts.views {
        if llm.over_budget(opts.budget) {
            eprintln!(
                "{what}: token budget reached ({} spent) — skipping the '{}' view",
                llm.usage().total_tokens(),
                view.label()
            );
            break;
        }
        match llm
            .generate_from_functions(&functions, &exemplars, &prevention, *view)
            .await
        {
            Ok(mut proposed) => {
                for c in &mut proposed {
                    if c.is_object() {
                        c["perspective"] = json!(view.label());
                    }
                }
                cases.extend(proposed);
            }
            Err(e) => errors.push(format!("{} view: {e}", view.label())),
        }
    }
    if cases.is_empty() {
        anyhow::bail!(
            "{what}: no view produced cases{}",
            if errors.is_empty() {
                String::new()
            } else {
                format!(" ({})", errors.join("; "))
            }
        );
    }
    let cases = clear_duplicate_ids(dedupe_cases(cases));
    let (cases, dropped) = drop_known_duplicates(root, cases).await;
    if dropped > 0 {
        eprintln!("{what}: dropped {dropped} case(s) duplicating existing stored tests");
    }
    let test_ids = store_cases(root, cases, None, None).await?;
    let quarantined = screen_if(opts, root, &test_ids, model).await?;
    Ok(GenSummary {
        prd_id: None,
        test_ids,
        quarantined,
    })
}

/// The store upserts by id, so an id repeated across merged views would
/// silently overwrite an earlier case. Keep the first occurrence; later
/// repeats get their id cleared so the store assigns a fresh uuid.
fn clear_duplicate_ids(cases: Vec<Value>) -> Vec<Value> {
    let mut seen = std::collections::BTreeSet::new();
    cases
        .into_iter()
        .map(|mut c| {
            if let Some(id) = c.get("id").and_then(Value::as_str)
                && !id.is_empty()
                && !seen.insert(id.to_string())
            {
                c["id"] = json!("");
            }
            c
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn from_code_summary_with_endpoints_generates_deterministic_specs_without_llm() {
        let root = crate::local::tmp_root();
        let summary = root.join("code_summary.yaml");
        std::fs::write(
            &summary,
            r#"
project_name: demo
api_endpoints:
  - method: GET
    path: /health
    expect_status: 200
"#,
        )
        .unwrap();

        let out = generate(
            &root,
            Some(&summary),
            None,
            None,
            "no-such-model",
            None,
            &GenOpts::default(),
        )
        .await
        .unwrap();
        assert_eq!(out.test_ids.len(), 1);
        let case = crate::local::store::get_value(&root, &out.test_ids[0])
            .await
            .unwrap();
        assert_eq!(case["kind"], "backend");
        assert_eq!(case["spec"]["method"], "GET");
        assert_eq!(case["spec"]["path"], "/health");
        assert_eq!(case["spec"]["expect_status"], 200);
        assert!(case["prdId"].as_str().is_some());
        let prds = crate::local::store::list_prds(&root).await.unwrap();
        assert_eq!(prds[0]["cases"], 1);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn audit_model_list_splits_comma_separated_models() {
        assert_eq!(
            model_list("gpt-5.3-codex, gpt-5.5"),
            vec!["gpt-5.3-codex".to_string(), "gpt-5.5".to_string()]
        );
    }

    #[test]
    fn duplicate_ids_across_views_get_cleared_not_overwritten() {
        let out = clear_duplicate_ids(vec![
            json!({"id":"TC001","title":"normal"}),
            json!({"id":"TC001","title":"boundary variant"}),
            json!({"id":"BND001","title":"boundary"}),
            json!({"title":"no id at all"}),
        ]);
        assert_eq!(out[0]["id"], "TC001");
        // The repeat keeps its case but loses the colliding id (store will
        // assign a uuid instead of silently overwriting TC001).
        assert_eq!(out[1]["id"], "");
        assert_eq!(out[1]["title"], "boundary variant");
        assert_eq!(out[2]["id"], "BND001");
        assert!(out[3].get("id").is_none());
    }

    #[test]
    fn audit_dedupe_cases_by_title_records_consensus_votes() {
        let cases = vec![
            json!({"title":"same","modelSource":"a"}),
            json!({"title":"same","modelSource":"b"}),
            json!({"title":"other","modelSource":"a"}),
        ];
        let out = dedupe_cases(cases);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["modelSource"], "a");
        // Two distinct models proposed "same" → consensus with 2 votes.
        assert_eq!(out[0]["modelVotes"], 2);
        assert_eq!(out[0]["consensus"], true);
        // A single-model case gets 1 vote and no consensus flag.
        assert_eq!(out[1]["modelVotes"], 1);
        assert!(out[1].get("consensus").is_none());
    }

    #[test]
    fn title_similarity_is_order_insensitive_jaccard() {
        assert_eq!(
            title_similarity("get todos responds", "get todos responds"),
            1.0
        );
        assert!(title_similarity("get todos responds", "todos get responds") > 0.99);
        assert!(title_similarity("get todos", "delete users") < 0.2);
    }

    #[tokio::test]
    async fn novelty_guard_drops_cases_duplicating_stored_tests() {
        let root = crate::local::tmp_root();
        crate::local::store::add_value(
            &root,
            json!({"id":"e1","title":"GET /todos responds","kind":"backend",
                   "spec":{"method":"GET","path":"/todos"}}),
        )
        .await
        .unwrap();

        let cases = vec![
            // Near-duplicate title of the stored test → dropped.
            json!({"title":"get todos responds","kind":"backend"}),
            // Genuinely new → kept.
            json!({"title":"DELETE /users/{id} rejects unknown","kind":"backend"}),
        ];
        let (kept, dropped) = drop_known_duplicates(&root, cases).await;
        assert_eq!(dropped, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["title"], "DELETE /users/{id} rejects unknown");

        std::fs::remove_dir_all(root).ok();
    }
}
