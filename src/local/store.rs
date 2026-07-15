//! `testsprite_tests/tests/` and `testsprite_tests/results/` storage.

use std::path::Path;

use anyhow::{Context, anyhow, bail};
use serde_json::Value;

use crate::server::executors::Outcome;

use super::{LocalTest, results_dir, tests_dir};

fn test_path(root: &Path, id: &str) -> std::path::PathBuf {
    tests_dir(root).join(format!("{id}.json"))
}

/// Read `file` as a JSON object, assign a uuid `id` if missing/empty, and
/// store it under `tests/<id>.json`. Returns the id.
pub fn add(root: &Path, file: &Path) -> anyhow::Result<String> {
    let body =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let value: Value =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", file.display()))?;
    add_value(root, value)
}

/// Assign a uuid `id` if missing/empty, and store `value` under
/// `tests/<id>.json`. Returns the id.
pub fn add_value(root: &Path, value: Value) -> anyhow::Result<String> {
    let mut obj = match value {
        Value::Object(obj) => obj,
        _ => bail!("test case is not a JSON object"),
    };

    let id = match obj.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => uuid::Uuid::new_v4().to_string(),
    };
    obj.insert("id".to_string(), Value::String(id.clone()));

    let dir = tests_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let path = test_path(root, &id);
    let mut out = serde_json::to_string_pretty(&Value::Object(obj))?;
    out.push('\n');
    std::fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(id)
}

/// List every stored test case, sorted by id.
pub fn list(root: &Path) -> anyhow::Result<Vec<LocalTest>> {
    let dir = tests_dir(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut tests = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let body =
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let test: LocalTest =
            serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
        tests.push(test);
    }
    tests.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(tests)
}

/// Load one stored test case by id.
pub fn load_one(root: &Path, id: &str) -> anyhow::Result<LocalTest> {
    let path = test_path(root, id);
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!("no test {id} at {}", path.display()));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let test: LocalTest =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    Ok(test)
}

/// Write the outcome of running a test case to `results/<id>.json`.
pub fn write_result(
    root: &Path,
    id: &str,
    outcome: &Outcome,
    analysis: Option<&Value>,
) -> anyhow::Result<()> {
    let dir = results_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let mut record = serde_json::json!({
        "id": id,
        "passed": outcome.passed,
        "error": outcome.error,
        "code": outcome.code,
    });
    if let Some(analysis) = analysis {
        record["analysis"] = analysis.clone();
    }
    let path = dir.join(format!("{id}.json"));
    let mut body = serde_json::to_string_pretty(&record)?;
    body.push('\n');
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_with_explicit_id_stores_under_that_id() {
        let root = crate::local::tmp_root();
        let src = root.join("case.json");
        std::fs::write(
            &src,
            r#"{"id":"my-id","title":"t1","spec":{"method":"GET","path":"/"}}"#,
        )
        .unwrap();

        let id = add(&root, &src).unwrap();
        assert_eq!(id, "my-id");

        let loaded = load_one(&root, "my-id").unwrap();
        assert_eq!(loaded.id, "my-id");
        assert_eq!(loaded.title, "t1");
        assert_eq!(
            loaded.spec,
            Some(serde_json::json!({"method":"GET","path":"/"}))
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn add_without_id_assigns_uuid() {
        let root = crate::local::tmp_root();
        let src = root.join("case.json");
        std::fs::write(&src, r#"{"title":"no id here"}"#).unwrap();

        let id = add(&root, &src).unwrap();
        assert!(!id.is_empty());
        assert!(uuid::Uuid::parse_str(&id).is_ok());

        let loaded = load_one(&root, &id).unwrap();
        assert_eq!(loaded.id, id);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn list_returns_all_added_tests() {
        let root = crate::local::tmp_root();

        let src_a = root.join("a.json");
        std::fs::write(&src_a, r#"{"id":"a","title":"A"}"#).unwrap();
        let src_b = root.join("b.json");
        std::fs::write(&src_b, r#"{"id":"b","title":"B"}"#).unwrap();

        add(&root, &src_a).unwrap();
        add(&root, &src_b).unwrap();

        let tests = list(&root).unwrap();
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0].id, "a");
        assert_eq!(tests[1].id, "b");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn add_value_without_id_assigns_uuid() {
        let root = crate::local::tmp_root();

        let id = add_value(&root, serde_json::json!({"title": "generated"})).unwrap();
        assert!(!id.is_empty());
        assert!(uuid::Uuid::parse_str(&id).is_ok());

        let loaded = load_one(&root, &id).unwrap();
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.title, "generated");

        std::fs::remove_dir_all(&root).unwrap();
    }
}
