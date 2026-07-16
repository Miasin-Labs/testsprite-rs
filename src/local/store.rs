//! SQLite-backed test case + run-result store (see [`super::db`]).

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use serde_json::Value;

use super::LocalTest;
use crate::server::executors::{Outcome, TestKind};

/// Read `file` as a JSON object, assign a uuid `id` if missing/empty, and
/// upsert it into the `tests` table. Returns the id.
pub async fn add(root: &Path, file: &Path) -> anyhow::Result<String> {
    let body =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let value: Value =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", file.display()))?;
    add_value(root, value).await
}

/// Assign a uuid `id` if missing/empty, and upsert `value` into the `tests`
/// table. Returns the id.
pub async fn add_value(root: &Path, value: Value) -> anyhow::Result<String> {
    let mut obj = match value {
        Value::Object(obj) => obj,
        _ => bail!("test case is not a JSON object"),
    };

    let id = match obj.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => uuid::Uuid::new_v4().to_string(),
    };
    obj.insert("id".to_string(), Value::String(id.clone()));

    let title = obj
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let kind = obj.get("kind").and_then(Value::as_str).map(str::to_string);
    let body = serde_json::to_string(&Value::Object(obj))?;

    let pool = crate::local::db::open(root).await?;
    sqlx::query(
        "INSERT INTO tests (id,title,kind,body,updated_at) VALUES (?,?,?,?,datetime('now')) \
         ON CONFLICT(id) DO UPDATE SET title=excluded.title, kind=excluded.kind, body=excluded.body, updated_at=datetime('now')",
    )
    .bind(&id)
    .bind(&title)
    .bind(kind.as_deref())
    .bind(&body)
    .execute(&pool)
    .await?;

    Ok(id)
}

/// Snapshot the CURRENT stored definition of `id` into `test_revisions` before
/// something overwrites it, so an automated rewrite is never the only copy of
/// what the test used to assert. No-op when `id` isn't stored yet.
pub async fn snapshot_revision(root: &Path, id: &str, reason: &str) -> anyhow::Result<()> {
    let pool = crate::local::db::open(root).await?;
    let existing: Option<(String,)> = sqlx::query_as("SELECT body FROM tests WHERE id=?")
        .bind(id)
        .fetch_optional(&pool)
        .await?;
    let Some((body,)) = existing else {
        return Ok(());
    };
    sqlx::query("INSERT INTO test_revisions (test_id, body, reason) VALUES (?,?,?)")
        .bind(id)
        .bind(&body)
        .bind(reason)
        .execute(&pool)
        .await?;
    Ok(())
}

/// Prior definitions of `id`, newest-first, as `{revId, body, reason, createdAt}`.
pub async fn revisions(root: &Path, id: &str) -> anyhow::Result<Vec<Value>> {
    let pool = crate::local::db::open(root).await?;
    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT rev_id, body, reason, created_at FROM test_revisions WHERE test_id=? \
         ORDER BY rev_id DESC",
    )
    .bind(id)
    .fetch_all(&pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(rev_id, body, reason, created_at)| {
            serde_json::json!({
                "revId": rev_id,
                "reason": reason,
                "createdAt": created_at,
                "body": serde_json::from_str::<Value>(&body).unwrap_or(Value::Null),
            })
        })
        .collect())
}

/// Persist a generated PRD + its test plan; returns the new prd id. The PRD is
/// the normalized "what this app should do" the cases were derived from — kept
/// so the flow (doc/summary -> PRD -> plan -> cases) stays inspectable, not just
/// the leaf cases.
pub async fn save_prd(
    root: &Path,
    source: &str,
    prd: &Value,
    plan: &[Value],
) -> anyhow::Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let pool = crate::local::db::open(root).await?;
    sqlx::query("INSERT INTO prd (id,source,prd_json,plan_json) VALUES (?,?,?,?)")
        .bind(&id)
        .bind(source)
        .bind(serde_json::to_string(prd)?)
        .bind(serde_json::to_string(&Value::Array(plan.to_vec()))?)
        .execute(&pool)
        .await?;
    Ok(id)
}

/// List stored PRDs newest-first as `{id, source, features, cases, createdAt}`.
pub async fn list_prds(root: &Path) -> anyhow::Result<Vec<Value>> {
    let pool = crate::local::db::open(root).await?;
    let rows: Vec<(String, String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, source, prd_json, plan_json, created_at, approved_at FROM prd ORDER BY created_at DESC, rowid DESC",
    )
    .fetch_all(&pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, source, prd_json, plan_json, created_at, approved_at)| {
            let prd: Value = serde_json::from_str(&prd_json).unwrap_or(Value::Null);
            let features = prd
                .get("features")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            let cases = serde_json::from_str::<Value>(&plan_json)
                .ok()
                .and_then(|p| p.as_array().map(|a| a.len()))
                .unwrap_or(0);
            serde_json::json!({ "id": id, "source": source, "features": features, "cases": cases, "createdAt": created_at, "approvedAt": approved_at })
        })
        .collect())
}

/// Load one PRD (its requirements + generated plan) by id.
pub async fn load_prd(root: &Path, id: &str) -> anyhow::Result<Option<Value>> {
    let pool = crate::local::db::open(root).await?;
    let row: Option<(String, String, String, String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, source, prd_json, plan_json, created_at, approved_at FROM prd WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&pool)
    .await?;
    Ok(row.map(
        |(id, source, prd_json, plan_json, created_at, approved_at)| {
            serde_json::json!({
                "id": id,
                "source": source,
                "createdAt": created_at,
                "approvedAt": approved_at,
                "prd": serde_json::from_str::<Value>(&prd_json).unwrap_or(Value::Null),
                "plan": serde_json::from_str::<Value>(&plan_json).unwrap_or(Value::Null),
            })
        },
    ))
}

/// Mark a PRD/test plan as reviewed and approved. Returns the timestamp.
pub async fn approve_prd(root: &Path, id: &str) -> anyhow::Result<String> {
    let pool = crate::local::db::open(root).await?;
    let done = sqlx::query("UPDATE prd SET approved_at=datetime('now') WHERE id=?")
        .bind(id)
        .execute(&pool)
        .await?;
    if done.rows_affected() == 0 {
        bail!("no PRD {id}");
    }
    let ts: String = sqlx::query_scalar("SELECT approved_at FROM prd WHERE id=?")
        .bind(id)
        .fetch_one(&pool)
        .await?;
    Ok(ts)
}

/// If any selected test is stamped with a `prdId`, require that PRD to have
/// been approved. `ids=[]` means all stored tests.
pub async fn assert_prds_approved(root: &Path, ids: &[String]) -> anyhow::Result<()> {
    let tests = if ids.is_empty() {
        list(root).await?
    } else {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(load_one(root, id).await?);
        }
        out
    };
    let mut missing = Vec::new();
    for t in tests {
        let Some(prd_id) = t.extra.get("prdId").and_then(Value::as_str) else {
            continue;
        };
        match load_prd(root, prd_id).await? {
            Some(prd) if prd.get("approvedAt").and_then(Value::as_str).is_some() => {}
            Some(_) => missing.push(format!("{} -> {prd_id}", t.id)),
            None => missing.push(format!("{} -> missing PRD {prd_id}", t.id)),
        }
    }
    if !missing.is_empty() {
        bail!(
            "PRD approval required before running generated tests: {}. Review with `testsprite-rs prd review <id>` then `testsprite-rs prd approve <id>`.",
            missing.join(", ")
        );
    }
    Ok(())
}

/// The most recent PRD id, if any.
pub async fn latest_prd_id(root: &Path) -> anyhow::Result<Option<String>> {
    let pool = crate::local::db::open(root).await?;
    let row: Option<(String,)> =
        sqlx::query_as("SELECT id FROM prd ORDER BY created_at DESC, rowid DESC LIMIT 1")
            .fetch_optional(&pool)
            .await?;
    Ok(row.map(|(id,)| id))
}

/// List every stored test case, sorted by id.
pub async fn list(root: &Path) -> anyhow::Result<Vec<LocalTest>> {
    let pool = crate::local::db::open(root).await?;
    let bodies: Vec<String> = sqlx::query_scalar("SELECT body FROM tests ORDER BY id")
        .fetch_all(&pool)
        .await?;

    let mut tests = Vec::with_capacity(bodies.len());
    for body in bodies {
        let test: LocalTest = serde_json::from_str(&body).context("parsing stored test body")?;
        tests.push(test);
    }
    Ok(tests)
}

/// Load one stored test case by id.
pub async fn load_one(root: &Path, id: &str) -> anyhow::Result<LocalTest> {
    let pool = crate::local::db::open(root).await?;
    let body: Option<String> = sqlx::query_scalar("SELECT body FROM tests WHERE id=?")
        .bind(id)
        .fetch_optional(&pool)
        .await?;

    match body {
        None => Err(anyhow!("no test {id}")),
        Some(body) => {
            let test: LocalTest =
                serde_json::from_str(&body).context("parsing stored test body")?;
            Ok(test)
        }
    }
}

/// Load one stored test as the exact top-level JSON shape executors see.
pub async fn get_value(root: &Path, id: &str) -> anyhow::Result<Value> {
    Ok(load_one(root, id).await?.to_case_value())
}

/// Replace a frontend test's `planSteps` with a JSON array.
pub async fn put_plan_steps(root: &Path, id: &str, steps: Value) -> anyhow::Result<()> {
    let mut test = load_one(root, id).await?;
    let Value::Array(_) = steps else {
        bail!("plan steps must be a JSON array");
    };
    test.extra.insert("planSteps".to_string(), steps);
    add_value(root, test.to_case_value()).await?;
    Ok(())
}

/// Rename a stored test — update its `title` (column + the JSON body). Fixes the
/// "TC000 duplicate names with no way to rename" pain: the agent renames its own
/// generated tests to something meaningful.
pub async fn rename(root: &Path, id: &str, title: &str) -> anyhow::Result<()> {
    let mut test = load_one(root, id).await?;
    test.title = title.to_string();
    let body = serde_json::to_string(&test)?;

    let pool = crate::local::db::open(root).await?;
    let done =
        sqlx::query("UPDATE tests SET title=?, body=?, updated_at=datetime('now') WHERE id=?")
            .bind(title)
            .bind(&body)
            .bind(id)
            .execute(&pool)
            .await?;
    if done.rows_affected() == 0 {
        bail!("no test {id}");
    }
    Ok(())
}

/// Append the outcome of running a test case to the `runs` table.
///
/// `kind` is the executor that produced `outcome`; the verdict cannot be
/// derived from the error text without it (see [`crate::local::verdict`]).
pub async fn write_result(
    root: &Path,
    id: &str,
    outcome: &Outcome,
    analysis: Option<&Value>,
    kind: TestKind,
) -> anyhow::Result<()> {
    let (v, fk) = crate::local::verdict::classify(outcome.passed, &outcome.error, kind);
    let analysis_str = analysis.map(serde_json::to_string).transpose()?;

    let pool = crate::local::db::open(root).await?;
    sqlx::query(
        "INSERT INTO runs (test_id,passed,verdict,failure_kind,error,code,analysis) VALUES (?,?,?,?,?,?,?)",
    )
    .bind(id)
    .bind(outcome.passed as i64)
    .bind(v.as_str())
    .bind(fk)
    .bind(&outcome.error)
    .bind(&outcome.code)
    .bind(analysis_str)
    .execute(&pool)
    .await?;

    // Auto-prune: bound run history per test so scheduled/frequent runs don't
    // bloat testsprite.db (0 = unlimited).
    let keep = crate::envs::run_history_keep();
    if keep > 0 {
        sqlx::query(
            "DELETE FROM runs WHERE test_id = ? AND run_id NOT IN \
             (SELECT run_id FROM runs WHERE test_id = ? ORDER BY run_id DESC LIMIT ?)",
        )
        .bind(id)
        .bind(id)
        .bind(keep as i64)
        .execute(&pool)
        .await?;
    }

    Ok(())
}

/// Delete all but the latest `keep` runs for `id` (`0` = keep all). Returns the
/// number of rows deleted.
pub async fn prune_runs(root: &Path, id: &str, keep: usize) -> anyhow::Result<u64> {
    if keep == 0 {
        return Ok(0);
    }
    let pool = crate::local::db::open(root).await?;
    let res = sqlx::query(
        "DELETE FROM runs WHERE test_id = ? AND run_id NOT IN \
         (SELECT run_id FROM runs WHERE test_id = ? ORDER BY run_id DESC LIMIT ?)",
    )
    .bind(id)
    .bind(id)
    .bind(keep as i64)
    .execute(&pool)
    .await?;
    Ok(res.rows_affected())
}

/// Prune run history across ALL tests, keeping the latest `keep` per test
/// (`0` = keep all). Returns the total number of rows deleted.
pub async fn prune_all(root: &Path, keep: usize) -> anyhow::Result<u64> {
    if keep == 0 {
        return Ok(0);
    }
    let pool = crate::local::db::open(root).await?;
    let res = sqlx::query(
        "DELETE FROM runs WHERE run_id NOT IN (\
           SELECT run_id FROM (\
             SELECT run_id, ROW_NUMBER() OVER (PARTITION BY test_id ORDER BY run_id DESC) AS rn \
             FROM runs\
           ) WHERE rn <= ?)",
    )
    .bind(keep as i64)
    .execute(&pool)
    .await?;
    Ok(res.rows_affected())
}

/// The latest run result (if any) per stored test, joined to its title.
pub async fn latest_results(root: &Path) -> anyhow::Result<Vec<Value>> {
    let pool = crate::local::db::open(root).await?;
    let rows: Vec<(String, String, i64, Option<String>, String, Option<String>)> = sqlx::query_as(
        "SELECT t.id, t.title, r.passed, r.failure_kind, r.error, r.analysis \
         FROM tests t JOIN runs r ON r.run_id = (SELECT MAX(run_id) FROM runs WHERE test_id = t.id)",
    )
    .fetch_all(&pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for (id, title, passed, failure_kind, error, analysis) in rows {
        let cause = analysis
            .as_deref()
            .and_then(|a| serde_json::from_str::<Value>(a).ok())
            .and_then(|v| v.get("cause").and_then(Value::as_str).map(str::to_string));
        out.push(serde_json::json!({
            "id": id,
            "title": title,
            "passed": passed != 0,
            "verdict": if passed != 0 { "passed" } else { "failed" },
            "failureKind": failure_kind,
            "error": error,
            "cause": cause,
        }));
    }
    Ok(out)
}

/// Export all stored test definitions (raw JSON bodies), sorted by id.
pub async fn export_all(root: &Path) -> anyhow::Result<Vec<Value>> {
    let pool = crate::local::db::open(root).await?;
    let bodies: Vec<String> = sqlx::query_scalar("SELECT body FROM tests ORDER BY id")
        .fetch_all(&pool)
        .await?;

    let mut out = Vec::with_capacity(bodies.len());
    for body in bodies {
        out.push(serde_json::from_str(&body).context("parsing stored test body")?);
    }
    Ok(out)
}

/// Import test definitions (upserting each by id via [`add_value`]). Returns
/// the ids that were stored.
pub async fn import_values(root: &Path, tests: &[Value]) -> anyhow::Result<Vec<String>> {
    let mut ids = Vec::with_capacity(tests.len());
    for value in tests {
        ids.push(add_value(root, value.clone()).await?);
    }
    Ok(ids)
}

/// One `runs` row as selected below:
/// `(passed, error, code, analysis, verdict, failure_kind)`.
type RunRow = (
    i64,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Load the most recent run result for `id`, if any.
///
/// Includes the `verdict`/`failureKind` recorded at run time. They are stored
/// rather than re-derived because only the run knew which executor produced the
/// error text, and that determines what the text means.
pub async fn load_result(root: &Path, id: &str) -> anyhow::Result<Option<Value>> {
    let pool = crate::local::db::open(root).await?;
    let row: Option<RunRow> = sqlx::query_as(
        "SELECT passed,error,code,analysis,verdict,failure_kind FROM runs WHERE test_id=? \
             ORDER BY run_id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&pool)
    .await?;

    let Some((passed, error, code, analysis, verdict, failure_kind)) = row else {
        return Ok(None);
    };

    let mut record = serde_json::json!({
        "id": id,
        "passed": passed != 0,
        "error": error,
        "code": code,
        "verdict": verdict,
        "failureKind": failure_kind,
    });
    if let Some(analysis) = analysis {
        record["analysis"] = serde_json::from_str(&analysis).context("parsing stored analysis")?;
    }
    Ok(Some(record))
}

/// Full append-only run history for `id`, newest first. Surfaces the `runs`
/// table the executor appends to on every run — the local analogue of the
/// official CLI's `test result --history`. The data was already being
/// collected; this just exposes it.
pub async fn run_history(root: &Path, id: &str) -> anyhow::Result<Vec<Value>> {
    let pool = crate::local::db::open(root).await?;
    let rows: Vec<(i64, i64, Option<String>, Option<String>, String, String)> = sqlx::query_as(
        "SELECT run_id,passed,verdict,failure_kind,error,created_at \
         FROM runs WHERE test_id=? ORDER BY run_id DESC",
    )
    .bind(id)
    .fetch_all(&pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(run_id, passed, verdict, failure_kind, error, created_at)| {
                serde_json::json!({
                    "run_id": run_id,
                    "passed": passed != 0,
                    "verdict": verdict,
                    "failureKind": failure_kind,
                    "error": error,
                    "created_at": created_at,
                })
            },
        )
        .collect())
}

/// Ids of tests whose MOST RECENT run failed (`passed = 0`), sorted by id.
/// Empty when nothing has run or every latest run passed. Powers
/// `test rerun --failed` — replay just the reds (the local analogue of the
/// V3 `retryFailedAgents` flow).
pub async fn last_failed_ids(root: &Path) -> anyhow::Result<Vec<String>> {
    let pool = crate::local::db::open(root).await?;
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT t.id FROM tests t \
         JOIN runs r ON r.run_id = (SELECT MAX(run_id) FROM runs WHERE test_id = t.id) \
         WHERE r.passed = 0 ORDER BY t.id",
    )
    .fetch_all(&pool)
    .await?;
    Ok(ids)
}

/// Write an LLM-proposed fix recommendation to `fixes/<id>.md` for a coding
/// agent to pick up. `fix` is `{explanation, patch}`; returns the file path.
///
/// `appliable` is set by the caller only when the patch was grounded in the
/// repository's real source AND passed `git apply --check` — it controls whether
/// the patch is presented as a verified, ready-to-apply diff or an illustrative
/// sketch, so the two are never confused.
pub fn write_fix(
    root: &Path,
    id: &str,
    title: &str,
    analysis: Option<&Value>,
    fix: &Value,
    appliable: bool,
) -> anyhow::Result<std::path::PathBuf> {
    let dir = super::fixes_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let explanation = fix.get("explanation").and_then(Value::as_str).unwrap_or("");
    let patch = fix.get("patch").and_then(Value::as_str).unwrap_or("");
    let field = |key: &str| {
        analysis
            .and_then(|a| a.get(key))
            .and_then(Value::as_str)
            .unwrap_or("")
    };
    let verdict = {
        let v = field("verdict");
        if v.is_empty() { "unknown" } else { v }
    };

    let mut body =
        format!("# Fix recommendation — {title}\n\n- test: `{id}`\n- verdict: **{verdict}**\n");
    let cause = field("cause");
    if !cause.is_empty() {
        body.push_str(&format!("- root cause: {cause}\n"));
    }
    body.push_str(&format!("\n## What to change\n\n{explanation}\n"));
    if !patch.trim().is_empty() {
        if appliable {
            // Grounded in the repo's real source and passed `git apply --check`.
            body.push_str(
                "\n## Verified patch (grounded in your source; passed `git apply --check`)\n\n\
                 The fix engine was given the actual source at the failure's referenced lines and \
                 its output applies cleanly to your working tree. Review, then apply with \
                 `git apply`:\n\n",
            );
            body.push_str(&format!("```diff\n{patch}\n```\n"));
        } else {
            // The engine could not ground the change in real source, so the file
            // paths/line numbers are its reconstruction — a sketch, not a patch.
            body.push_str(
                "\n## Suggested change (illustrative — NOT generated from your source)\n\n\
                 The fix engine could not read (or ground the change in) the repository, so treat \
                 the paths and line numbers below as a sketch of the change to make by hand, not \
                 an appliable patch.\n\n",
            );
            body.push_str(&format!("```\n{patch}\n```\n"));
        }
    }

    let path = dir.join(format!("{id}.md"));
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Delete a stored test and its run history. Returns false if `id` wasn't
/// stored.
///
/// `add_value` is an upsert, so without this the `tests` table only ever grows:
/// auto-generated `TC000`-style duplicates accumulate with no way to prune them
/// short of opening the SQLite file by hand.
pub async fn delete(root: &Path, id: &str) -> anyhow::Result<bool> {
    let pool = crate::local::db::open(root).await?;
    let deleted = sqlx::query("DELETE FROM tests WHERE id=?")
        .bind(id)
        .execute(&pool)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Ok(false);
    }
    for table in ["runs", "test_revisions"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE test_id=?"))
            .bind(id)
            .execute(&pool)
            .await?;
    }
    Ok(true)
}

/// Render a stored test into repo-native source text.
///
/// A `spec` case carries no `code` of its own — it is executed by reqwest from
/// its `{method, path, expect_status}`. Rendering it through the same
/// `python_for` the run records as its artifact is what lets the flagship
/// deterministic cases leave SQLite at all.
fn emit_source(test: &LocalTest) -> anyhow::Result<String> {
    if let Some(code) = test.extra.get("code").and_then(Value::as_str)
        && !code.is_empty()
    {
        // A `command` test's "code" is a shell line (`cargo test -p foo`).
        // Writing that to `tests/bar.rs` produces a file that cannot compile.
        if test.kind == Some(crate::server::executors::TestKind::Command) {
            bail!(
                "test {} is a `command` test: its code is a shell line, not source. \
                 Run it via `testsprite-rs test run`, or put the command in your CI \
                 config — emitting it to a source file would not compile.",
                test.id
            );
        }
        return Ok(code.to_string());
    }

    if let Some(spec) = &test.spec {
        let spec: crate::server::engine::EndpointSpec = serde_json::from_value(spec.clone())
            .with_context(|| format!("test {} has an unparseable spec", test.id))?;
        // Emit against the placeholder target; the reader retargets it. Variables
        // are intentionally not substituted — an emitted file should not bake in
        // one machine's local ids.
        return Ok(crate::server::engine::python_for(
            &spec,
            "http://127.0.0.1:8080",
            &std::collections::HashMap::new(),
        ));
    }

    bail!("test {} has neither `code` nor `spec` to emit", test.id)
}

/// Materialize a stored test into a repo file (e.g. `crates/foo/tests/bar.rs`,
/// or `tests/test_api.py` for a spec case) so cargo/CI own it, instead of only
/// running it ephemerally out of SQLite.
pub async fn emit(root: &Path, id: &str, out: &Path) -> anyhow::Result<()> {
    let test = load_one(root, id).await?;
    let source = emit_source(&test)?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(out, source).with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

/// Write stored tests into `testsprite_tests/TC001_Title.ext`-style files, like
/// the official plugin's generated test-code folder. This is intentionally dumb:
/// it mirrors the runnable artifact shape already present in each stored case.
pub async fn materialize(
    root: &Path,
    ids: &[String],
    out_dir: Option<&Path>,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    let tests = if ids.is_empty() {
        list(root).await?
    } else {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(load_one(root, id).await?);
        }
        out
    };
    let dir = out_dir
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| super::ts_dir(root));
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut paths = Vec::new();
    for test in tests {
        let Some((body, ext)) = materialized_body(&test)? else {
            continue;
        };
        let path = dir.join(format!(
            "{}_{}.{}",
            safe_file_part(&test.id),
            safe_file_part(&test.title),
            ext
        ));
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        paths.push(path);
    }
    Ok(paths)
}

fn materialized_body(test: &LocalTest) -> anyhow::Result<Option<(String, &'static str)>> {
    let kind = test.kind.unwrap_or_default();
    if let Some(code) = test.extra.get("code").and_then(Value::as_str)
        && !code.trim().is_empty()
    {
        return Ok(Some((
            code.to_string(),
            match kind {
                TestKind::Frontend => "js",
                TestKind::Mcp => "json",
                TestKind::Rust => "rs",
                TestKind::Command => "sh",
                TestKind::Backend => "py",
            },
        )));
    }
    if matches!(kind, TestKind::Backend) && test.spec.is_some() {
        return Ok(Some((emit_source(test)?, "py")));
    }
    if let Some(steps) = test.extra.get("planSteps").or_else(|| test.extra.get("steps")) {
        return Ok(Some((serde_json::to_string_pretty(steps)?, "json")));
    }
    Ok(None)
}

fn safe_file_part(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    out.trim_matches('_').chars().take(80).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn delete_removes_the_test_and_its_history() {
        let root = crate::local::tmp_root();
        let id = add_value(&root, serde_json::json!({"title": "t"}))
            .await
            .unwrap();
        write_result(
            &root,
            &id,
            &Outcome::fail("boom", String::new()),
            None,
            TestKind::Backend,
        )
        .await
        .unwrap();
        snapshot_revision(&root, &id, "test").await.unwrap();

        assert!(delete(&root, &id).await.unwrap());
        assert!(list(&root).await.unwrap().is_empty());
        assert!(load_result(&root, &id).await.unwrap().is_none());
        assert!(revisions(&root, &id).await.unwrap().is_empty());

        // Deleting an unknown id is reported, not silently "successful".
        assert!(!delete(&root, "nope").await.unwrap());

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn emit_renders_a_spec_case_instead_of_dead_ending() {
        // The flagship deterministic cases carry a `spec` and no `code`. emit()
        // used to bail on exactly those — the cases it most needed to support.
        let root = crate::local::tmp_root();
        let id = add_value(
            &root,
            serde_json::json!({
                "title": "GET /todos responds",
                "kind": "backend",
                "spec": {"method": "GET", "path": "/todos", "expect_status": 200},
            }),
        )
        .await
        .unwrap();

        let out = root.join("tests/test_api.py");
        emit(&root, &id, &out).await.unwrap();
        let src = std::fs::read_to_string(&out).unwrap();
        assert!(src.contains("import requests"), "{src}");
        assert!(src.contains("/todos"), "{src}");
        assert!(src.contains("== 200"), "{src}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn emit_refuses_to_write_a_shell_command_into_a_source_file() {
        let root = crate::local::tmp_root();
        let id = add_value(
            &root,
            serde_json::json!({
                "title": "cargo tests",
                "kind": "command",
                "code": "cargo test -p mycrate --lib",
            }),
        )
        .await
        .unwrap();

        let err = emit(&root, &id, &root.join("tests/bar.rs"))
            .await
            .expect_err("a shell line is not Rust source");
        assert!(err.to_string().contains("shell line"), "{err}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn materialize_writes_official_style_test_files() {
        let root = crate::local::tmp_root();
        add_value(
            &root,
            serde_json::json!({
                "id": "TC001",
                "title": "GET /health responds",
                "kind": "backend",
                "spec": {"method": "GET", "path": "/health", "expect_status": 200}
            }),
        )
        .await
        .unwrap();
        add_value(
            &root,
            serde_json::json!({
                "id": "TC002",
                "title": "command gate",
                "kind": "command",
                "code": "cargo test --quiet"
            }),
        )
        .await
        .unwrap();
        add_value(
            &root,
            serde_json::json!({
                "id": "TC003",
                "title": "login flow",
                "kind": "frontend",
                "planSteps": [{"action": "goto", "url": "/login"}]
            }),
        )
        .await
        .unwrap();

        let paths = materialize(&root, &[], None).await.unwrap();
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(names.iter().any(|n| n == "TC001_GET__health_responds.py"), "{names:?}");
        assert!(names.iter().any(|n| n == "TC002_command_gate.sh"), "{names:?}");
        assert!(names.iter().any(|n| n == "TC003_login_flow.json"), "{names:?}");

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn add_with_explicit_id_stores_under_that_id() {
        let root = crate::local::tmp_root();
        let src = root.join("case.json");
        std::fs::write(
            &src,
            r#"{"id":"my-id","title":"t1","spec":{"method":"GET","path":"/"}}"#,
        )
        .unwrap();

        let id = add(&root, &src).await.unwrap();
        assert_eq!(id, "my-id");

        let loaded = load_one(&root, "my-id").await.unwrap();
        assert_eq!(loaded.id, "my-id");
        assert_eq!(loaded.title, "t1");
        assert_eq!(
            loaded.spec,
            Some(serde_json::json!({"method":"GET","path":"/"}))
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn add_without_id_assigns_uuid() {
        let root = crate::local::tmp_root();
        let src = root.join("case.json");
        std::fs::write(&src, r#"{"title":"no id here"}"#).unwrap();

        let id = add(&root, &src).await.unwrap();
        assert!(!id.is_empty());
        assert!(uuid::Uuid::parse_str(&id).is_ok());

        let loaded = load_one(&root, &id).await.unwrap();
        assert_eq!(loaded.id, id);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn list_returns_all_added_tests() {
        let root = crate::local::tmp_root();

        let src_a = root.join("a.json");
        std::fs::write(&src_a, r#"{"id":"a","title":"A"}"#).unwrap();
        let src_b = root.join("b.json");
        std::fs::write(&src_b, r#"{"id":"b","title":"B"}"#).unwrap();

        add(&root, &src_a).await.unwrap();
        add(&root, &src_b).await.unwrap();

        let tests = list(&root).await.unwrap();
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0].id, "a");
        assert_eq!(tests[1].id, "b");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn add_value_without_id_assigns_uuid() {
        let root = crate::local::tmp_root();

        let id = add_value(&root, serde_json::json!({"title": "generated"}))
            .await
            .unwrap();
        assert!(!id.is_empty());
        assert!(uuid::Uuid::parse_str(&id).is_ok());

        let loaded = load_one(&root, &id).await.unwrap();
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.title, "generated");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn add_value_with_explicit_id_upserts() {
        let root = crate::local::tmp_root();

        let id = add_value(&root, serde_json::json!({"id":"dup","title":"first"}))
            .await
            .unwrap();
        add_value(&root, serde_json::json!({"id":"dup","title":"second"}))
            .await
            .unwrap();

        let tests = list(&root).await.unwrap();
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].id, id);
        assert_eq!(tests[0].title, "second");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn put_plan_steps_replaces_only_the_steps() {
        let root = crate::local::tmp_root();
        add_value(
            &root,
            serde_json::json!({"id":"front","title":"F","kind":"frontend","planSteps":["old"]}),
        )
        .await
        .unwrap();
        put_plan_steps(
            &root,
            "front",
            serde_json::json!(["new", {"action":"click","text":"Go"}]),
        )
        .await
        .unwrap();
        let loaded = get_value(&root, "front").await.unwrap();
        assert_eq!(loaded["title"], "F");
        assert_eq!(loaded["planSteps"][0], "new");
        assert_eq!(loaded["planSteps"][1]["text"], "Go");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn write_result_then_load_result_returns_latest() {
        let root = crate::local::tmp_root();
        add_value(&root, serde_json::json!({"id":"t1","title":"T"}))
            .await
            .unwrap();

        let outcome = Outcome {
            passed: false,
            error: "AssertionError: boom".to_string(),
            code: "print(1)".to_string(),
        };
        write_result(&root, "t1", &outcome, None, TestKind::Backend)
            .await
            .unwrap();

        let result = load_result(&root, "t1").await.unwrap().unwrap();
        assert_eq!(result["passed"], false);
        assert_eq!(result["error"], "AssertionError: boom");

        let ok = Outcome {
            passed: true,
            error: String::new(),
            code: String::new(),
        };
        write_result(&root, "t1", &ok, None, TestKind::Backend)
            .await
            .unwrap();

        let latest = load_result(&root, "t1").await.unwrap().unwrap();
        assert_eq!(latest["passed"], true);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn last_failed_ids_returns_only_latest_failures() {
        let root = crate::local::tmp_root();
        add_value(&root, serde_json::json!({"id":"pass","title":"P"}))
            .await
            .unwrap();
        add_value(&root, serde_json::json!({"id":"fail","title":"F"}))
            .await
            .unwrap();

        let ok = Outcome {
            passed: true,
            error: String::new(),
            code: String::new(),
        };
        let bad = Outcome {
            passed: false,
            error: "boom".to_string(),
            code: String::new(),
        };
        write_result(&root, "pass", &ok, None, TestKind::Backend)
            .await
            .unwrap();
        write_result(&root, "fail", &bad, None, TestKind::Backend)
            .await
            .unwrap();

        let reds = last_failed_ids(&root).await.unwrap();
        assert_eq!(reds, vec!["fail".to_string()]);

        // A later PASS on `fail` clears it — only the LATEST run counts.
        write_result(&root, "fail", &ok, None, TestKind::Backend)
            .await
            .unwrap();
        let reds = last_failed_ids(&root).await.unwrap();
        assert!(reds.is_empty(), "latest pass clears the red: {reds:?}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn load_result_missing_returns_none() {
        let root = crate::local::tmp_root();
        let result = load_result(&root, "nope").await.unwrap();
        assert!(result.is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }
    #[tokio::test]
    async fn emit_writes_stored_code_to_file() {
        let root = crate::local::tmp_root();
        add_value(
            &root,
            serde_json::json!({"id":"e1","title":"T","code":"fn t() {}"}),
        )
        .await
        .unwrap();

        let out = root.join("out").join("e1.rs");
        emit(&root, "e1", &out).await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "fn t() {}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn emit_without_code_errors() {
        // Neither `code` nor `spec`: there is genuinely nothing to render.
        let root = crate::local::tmp_root();
        add_value(&root, serde_json::json!({"id":"e2","title":"T"}))
            .await
            .unwrap();

        let out = root.join("e2.rs");
        let err = emit(&root, "e2", &out).await.unwrap_err();
        assert!(
            err.to_string().contains("neither `code` nor `spec`"),
            "{err}"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn save_prd_then_load_and_list_round_trips() {
        let root = crate::local::tmp_root();
        let prd = serde_json::json!({
            "product_overview": "a todo api",
            "features": [{"name": "Create"}, {"name": "List"}],
        });
        let plan = vec![
            serde_json::json!({"id": "TC001", "title": "create"}),
            serde_json::json!({"id": "TC002", "title": "list"}),
        ];
        let id = save_prd(&root, "instruction:todo", &prd, &plan)
            .await
            .unwrap();

        let listed = list_prds(&root).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], id);
        assert_eq!(listed[0]["features"], 2);
        assert_eq!(listed[0]["cases"], 2);
        assert!(listed[0]["approvedAt"].is_null());

        let loaded = load_prd(&root, &id).await.unwrap().unwrap();
        assert_eq!(loaded["prd"]["product_overview"], "a todo api");
        assert_eq!(loaded["plan"].as_array().unwrap().len(), 2);
        assert_eq!(
            latest_prd_id(&root).await.unwrap().as_deref(),
            Some(id.as_str())
        );
        let approved_at = approve_prd(&root, &id).await.unwrap();
        assert!(!approved_at.is_empty());
        let approved = load_prd(&root, &id).await.unwrap().unwrap();
        assert_eq!(approved["approvedAt"], approved_at);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn assert_prds_approved_blocks_unapproved_generated_tests() {
        let root = crate::local::tmp_root();
        let prd = serde_json::json!({"features":[{"name":"A"}]});
        let plan = vec![serde_json::json!({"id":"TC001","title":"T"})];
        let prd_id = save_prd(&root, "instruction:a", &prd, &plan).await.unwrap();
        add_value(
            &root,
            serde_json::json!({"id":"generated","title":"G","prdId": prd_id}),
        )
        .await
        .unwrap();

        let err = assert_prds_approved(&root, &["generated".to_string()])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("PRD approval required"), "{err}");

        approve_prd(&root, &prd_id).await.unwrap();
        assert_prds_approved(&root, &["generated".to_string()])
            .await
            .unwrap();

        std::fs::remove_dir_all(&root).unwrap();
    }
}
