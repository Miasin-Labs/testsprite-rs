//! `project` table lifecycle (single row, id=1): init, load, show.

use std::path::{Path, PathBuf};
use std::collections::HashMap;

use anyhow::{Context, anyhow};

use crate::server::executors::TestKind;

use super::Project;

/// Create the database (if needed) and upsert the single project row.
/// Returns the path to the SQLite database file.
pub async fn init(root: &Path, kind: TestKind, name: &str, url: Option<&str>) -> anyhow::Result<PathBuf> {
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
    let row: Option<(String, String, Option<String>)> =
        sqlx::query_as("SELECT name,kind,target_url FROM project WHERE id=1")
            .fetch_optional(&pool)
            .await?;

    match row {
        None => Err(anyhow!("no project — run `testsprite-rs project init` first")),
        Some((name, kind_str, target_url)) => Ok(Project {
            name,
            kind: TestKind::parse(&kind_str),
            target_url,
        }),
    }
}

/// Load and pretty-print the project row.
pub async fn show(root: &Path) -> anyhow::Result<()> {
    let project = load(root).await?;
    println!("{}", serde_json::to_string_pretty(&project)?);
    Ok(())
}

/// Read `testsprite_tests/variables.json` — a `{param: value}` map that seeds
/// `{param}` path segments in deterministic specs (so `{id}` can hit a real
/// record instead of the `1` probe). Empty when absent/unparseable: variables
/// are best-effort, never fatal.
pub fn load_variables(root: &Path) -> HashMap<String, String> {
    let path = super::ts_dir(root).join("variables.json");
    let Ok(body) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let raw: HashMap<String, serde_json::Value> = serde_json::from_str(&body).unwrap_or_default();
    raw.into_iter()
        .map(|(k, v)| {
            let s = match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            (k, s)
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn init_then_load_round_trips_with_url() {
        let root = crate::local::tmp_root();
        init(&root, TestKind::Backend, "my-app", Some("http://localhost:3000"))
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

    #[tokio::test]
    async fn init_twice_keeps_one_row() {
        let root = crate::local::tmp_root();
        init(&root, TestKind::Backend, "first", None).await.unwrap();
        init(&root, TestKind::Frontend, "second", Some("http://x")).await.unwrap();

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
