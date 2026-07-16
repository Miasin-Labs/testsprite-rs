//! `project` table lifecycle (single row, id=1): init, load, show.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};

use super::Project;
use crate::server::executors::TestKind;

/// Create the database (if needed) and upsert the single project row.
/// Returns the path to the SQLite database file.
pub async fn init(
    root: &Path,
    kind: TestKind,
    name: &str,
    url: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let pool = crate::local::db::open(root).await?;

    let kind_str = serde_json::to_value(kind)?
        .as_str()
        .context("TestKind did not serialize to a string")?
        .to_string();

    sqlx::query(
        "INSERT INTO project (id,name,kind,target_url) VALUES (1,?,?,?) \
         ON CONFLICT(id) DO UPDATE SET name=excluded.name, kind=excluded.kind, target_url=excluded.target_url",
    )
    .bind(name)
    .bind(&kind_str)
    .bind(url)
    .execute(&pool)
    .await?;

    Ok(crate::local::db::db_path(root))
}

/// Read the project row.
pub async fn load(root: &Path) -> anyhow::Result<Project> {
    let pool = crate::local::db::open(root).await?;
    let row: Option<(String, String, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT name,kind,target_url,start_command FROM project WHERE id=1")
            .fetch_optional(&pool)
            .await?;

    match row {
        None => Err(anyhow!(
            "no project — run `testsprite-rs project init` first"
        )),
        Some((name, kind_str, target_url, start_command)) => Ok(Project {
            name,
            kind: TestKind::parse(&kind_str),
            target_url,
            start_command,
        }),
    }
}

/// Load and pretty-print the project row.
pub async fn show(root: &Path) -> anyhow::Result<()> {
    let project = load(root).await?;
    println!("{}", serde_json::to_string_pretty(&project)?);
    Ok(())
}

/// Persist the command that starts the target app (for `test run --serve`).
pub async fn set_start(root: &Path, command: &str) -> anyhow::Result<()> {
    let pool = crate::local::db::open(root).await?;
    let n = sqlx::query("UPDATE project SET start_command=? WHERE id=1")
        .bind(command)
        .execute(&pool)
        .await?
        .rows_affected();
    if n == 0 {
        anyhow::bail!("no project — run `testsprite-rs project init` first");
    }
    Ok(())
}

/// Read local QA variables:
///
/// - `.testsprite.env` (gitignored secrets; KEY=value, comments allowed)
/// - `testsprite_tests/variables.json` (checked test data / path params)
///
/// `variables.json` wins on key collision. Missing/unparseable files are
/// best-effort, never fatal. `${KEY}` interpolation also falls back to the
/// process environment at execution time, so CI secrets do not need to be
/// copied into either file.
pub fn load_variables(root: &Path) -> HashMap<String, String> {
    let mut vars = load_env_file(&root.join(".testsprite.env"));
    let path = super::ts_dir(root).join("variables.json");
    let Ok(body) = std::fs::read_to_string(&path) else {
        return vars;
    };
    let raw: HashMap<String, serde_json::Value> = serde_json::from_str(&body).unwrap_or_default();
    for (k, v) in raw {
        let s = match v {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        };
        vars.insert(k, s);
    }
    vars
}

fn load_env_file(path: &Path) -> HashMap<String, String> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    parse_env(&body)
}

fn parse_env(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        if key.is_empty() {
            continue;
        }
        out.insert(key.to_string(), unquote_env(v.trim()));
    }
    out
}

fn unquote_env(v: &str) -> String {
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        return v[1..v.len() - 1].to_string();
    }
    v.to_string()
}

/// Upsert one `key=value` into `testsprite_tests/variables.json` (created if
/// needed). Returns the full updated map.
pub fn set_variable(
    root: &Path,
    key: &str,
    value: &str,
) -> anyhow::Result<HashMap<String, String>> {
    let mut vars = load_variables(root);
    vars.insert(key.to_string(), value.to_string());
    let dir = super::ts_dir(root);
    std::fs::create_dir_all(&dir)?;
    // BTreeMap for stable, diff-friendly key ordering on disk.
    let ordered: std::collections::BTreeMap<&String, &String> = vars.iter().collect();
    std::fs::write(
        dir.join("variables.json"),
        serde_json::to_string_pretty(&ordered)? + "\n",
    )?;
    Ok(vars)
}

/// Seed several `key=value` pairs into `variables.json` at once, WITHOUT
/// overwriting keys the user already set (in either `.testsprite.env` or
/// `variables.json`). Returns the keys actually written. Used to seed PRD
/// `testCredentials` / `test_environment` so a user's local overrides always
/// win. Serialization matches [`set_variable`] exactly (one shared writer).
pub fn seed_variables_missing(
    root: &Path,
    pairs: &[(String, String)],
) -> anyhow::Result<Vec<String>> {
    let existing = load_variables(root);
    let mut vars = existing.clone();
    let mut written = Vec::new();
    for (k, v) in pairs {
        if existing.contains_key(k) || v.is_empty() {
            continue;
        }
        vars.insert(k.clone(), v.clone());
        written.push(k.clone());
    }
    if written.is_empty() {
        return Ok(written);
    }
    let dir = super::ts_dir(root);
    std::fs::create_dir_all(&dir)?;
    let ordered: std::collections::BTreeMap<&String, &String> = vars.iter().collect();
    std::fs::write(
        dir.join("variables.json"),
        serde_json::to_string_pretty(&ordered)? + "\n",
    )?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_then_load_round_trips_with_url() {
        let root = crate::local::tmp_root();
        init(
            &root,
            TestKind::Backend,
            "my-app",
            Some("http://localhost:3000"),
        )
        .await
        .unwrap();

        let project = load(&root).await.unwrap();
        assert_eq!(project.name, "my-app");
        assert_eq!(project.kind, TestKind::Backend);
        assert_eq!(project.target_url.as_deref(), Some("http://localhost:3000"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn init_then_load_round_trips_without_url() {
        let root = crate::local::tmp_root();
        init(&root, TestKind::Rust, "unit-app", None).await.unwrap();

        let project = load(&root).await.unwrap();
        assert_eq!(project.name, "unit-app");
        assert_eq!(project.kind, TestKind::Rust);
        assert_eq!(project.target_url, None);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn load_missing_project_errors() {
        let root = crate::local::tmp_root();
        let err = load(&root).await.unwrap_err();
        assert!(err.to_string().contains("project init"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn variables_loads_gitignored_env_and_json_overrides() {
        let root = crate::local::tmp_root();
        std::fs::write(
            root.join(".testsprite.env"),
            "# local only\nexport ERS_EMAIL=cole@example.com\nTOKEN=\"secret token\"\nid=from-env\n",
        )
        .unwrap();
        std::fs::create_dir_all(crate::local::ts_dir(&root)).unwrap();
        std::fs::write(
            crate::local::ts_dir(&root).join("variables.json"),
            r#"{"id":"from-json","n":7}"#,
        )
        .unwrap();

        let vars = load_variables(&root);
        assert_eq!(vars["ERS_EMAIL"], "cole@example.com");
        assert_eq!(vars["TOKEN"], "secret token");
        assert_eq!(vars["id"], "from-json");
        assert_eq!(vars["n"], "7");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn seed_variables_missing_never_clobbers_user_values() {
        let root = crate::local::tmp_root();
        // User already set adminUser_password in variables.json.
        set_variable(&root, "adminUser_password", "user-secret").unwrap();

        let written = seed_variables_missing(
            &root,
            &[
                ("adminUser_username".to_string(), "admin".to_string()),
                (
                    "adminUser_password".to_string(),
                    "seeded-should-lose".to_string(),
                ),
                ("empty_skipped".to_string(), String::new()),
            ],
        )
        .unwrap();

        // Only the genuinely-new, non-empty key is written.
        assert_eq!(written, vec!["adminUser_username".to_string()]);
        let vars = load_variables(&root);
        assert_eq!(vars["adminUser_username"], "admin");
        // The user's pre-existing value survives; the seed did not overwrite it.
        assert_eq!(vars["adminUser_password"], "user-secret");
        assert!(!vars.contains_key("empty_skipped"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn init_twice_keeps_one_row() {
        let root = crate::local::tmp_root();
        init(&root, TestKind::Backend, "first", None).await.unwrap();
        init(&root, TestKind::Frontend, "second", Some("http://x"))
            .await
            .unwrap();

        let pool = crate::local::db::open(&root).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM project")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);

        let project = load(&root).await.unwrap();
        assert_eq!(project.name, "second");
        assert_eq!(project.kind, TestKind::Frontend);

        std::fs::remove_dir_all(&root).unwrap();
    }
}
