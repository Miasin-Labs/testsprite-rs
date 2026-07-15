//! SQLite-backed test case + run-result store (see [`super::db`]).

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use serde_json::Value;

use crate::server::executors::Outcome;

use super::LocalTest;

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

    let title = obj.get("title").and_then(Value::as_str).unwrap_or("").to_string();
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

/// Rename a stored test — update its `title` (column + the JSON body). Fixes the
/// "TC000 duplicate names with no way to rename" pain: the agent renames its own
/// generated tests to something meaningful.
pub async fn rename(root: &Path, id: &str, title: &str) -> anyhow::Result<()> {
    let mut test = load_one(root, id).await?;
    test.title = title.to_string();
    let body = serde_json::to_string(&test)?;

    let pool = crate::local::db::open(root).await?;
    let done = sqlx::query(
        "UPDATE tests SET title=?, body=?, updated_at=datetime('now') WHERE id=?",
    )
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
pub async fn write_result(
    root: &Path,
    id: &str,
    outcome: &Outcome,
    analysis: Option<&Value>,
) -> anyhow::Result<()> {
    let (v, fk) = crate::local::verdict::classify(outcome.passed, &outcome.error);
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

    Ok(())
}

/// The latest run result (if any) per stored test, joined to its title.
pub async fn latest_results(root: &Path) -> anyhow::Result<Vec<Value>> {
    let pool = crate::local::db::open(root).await?;
    let rows: Vec<(String, String, i64, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT t.id, t.title, r.passed, r.failure_kind, r.analysis \
         FROM tests t JOIN runs r ON r.run_id = (SELECT MAX(run_id) FROM runs WHERE test_id = t.id)",
    )
    .fetch_all(&pool)
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for (id, title, passed, failure_kind, analysis) in rows {
        let cause = analysis
            .as_deref()
            .and_then(|a| serde_json::from_str::<Value>(a).ok())
            .and_then(|v| v.get("cause").and_then(Value::as_str).map(str::to_string));
        out.push(serde_json::json!({
            "id": id,
            "title": title,
            "passed": passed != 0,
            "failureKind": failure_kind,
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

/// Load the most recent run result for `id`, if any.
pub async fn load_result(root: &Path, id: &str) -> anyhow::Result<Option<Value>> {
    let pool = crate::local::db::open(root).await?;
    let row: Option<(i64, String, String, Option<String>)> = sqlx::query_as(
        "SELECT passed,error,code,analysis FROM runs WHERE test_id=? ORDER BY run_id DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&pool)
    .await?;

    let Some((passed, error, code, analysis)) = row else {
        return Ok(None);
    };

    let mut record = serde_json::json!({
        "id": id,
        "passed": passed != 0,
        "error": error,
        "code": code,
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

/// Write an LLM-proposed fix recommendation to `fixes/<id>.md` for a coding
/// agent to pick up. `fix` is `{explanation, patch}`; returns the file path.
pub fn write_fix(
    root: &Path,
    id: &str,
    title: &str,
    analysis: Option<&Value>,
    fix: &Value,
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

    let mut body = format!("# Fix recommendation — {title}\n\n- test: `{id}`\n- verdict: **{verdict}**\n");
    let cause = field("cause");
    if !cause.is_empty() {
        body.push_str(&format!("- root cause: {cause}\n"));
    }
    body.push_str(&format!(
        "\n## What to change\n\n{explanation}\n\n## Proposed patch\n\n```diff\n{patch}\n```\n"
    ));

    let path = dir.join(format!("{id}.md"));
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Materialize a stored test's `code` into a repo file (e.g.
/// `crates/foo/tests/bar.rs`) so cargo/CI own it, instead of only running it
/// ephemerally out of SQLite.
pub async fn emit(root: &Path, id: &str, out: &Path) -> anyhow::Result<()> {
    let test = load_one(root, id).await?;
    let code = test.extra.get("code").and_then(Value::as_str).unwrap_or("");
    if code.is_empty() {
        bail!("test {id} has no `code` to emit");
    }
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(out, code).with_context(|| format!("writing {}", out.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        write_result(&root, "t1", &outcome, None).await.unwrap();

        let result = load_result(&root, "t1").await.unwrap().unwrap();
        assert_eq!(result["passed"], false);
        assert_eq!(result["error"], "AssertionError: boom");

        let ok = Outcome {
            passed: true,
            error: String::new(),
            code: String::new(),
        };
        write_result(&root, "t1", &ok, None).await.unwrap();

        let latest = load_result(&root, "t1").await.unwrap().unwrap();
        assert_eq!(latest["passed"], true);

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
        add_value(&root, serde_json::json!({"id":"e1","title":"T","code":"fn t() {}"}))
            .await
            .unwrap();

        let out = root.join("out").join("e1.rs");
        emit(&root, "e1", &out).await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "fn t() {}");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn emit_without_code_errors() {
        let root = crate::local::tmp_root();
        add_value(&root, serde_json::json!({"id":"e2","title":"T"}))
            .await
            .unwrap();

        let out = root.join("e2.rs");
        let err = emit(&root, "e2", &out).await.unwrap_err();
        assert!(err.to_string().contains("no `code` to emit"));

        std::fs::remove_dir_all(&root).unwrap();
    }

}
