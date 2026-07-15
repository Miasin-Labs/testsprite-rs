//! `project` table lifecycle (single row, id=1): init, load, show.

use std::path::{Path, PathBuf};

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
