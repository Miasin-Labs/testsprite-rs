//! SQLite persistence (sqlx) for the local test lifecycle.
//!
//! One database file per project root at `testsprite_tests/testsprite.db`,
//! holding the project config, the stored test cases, and an append-only run
//! history. This replaces the former JSON-file store. The on-the-wire test
//! JSON is preserved verbatim in `tests.body` so [`super::LocalTest`] (with its
//! flattened `extra`) round-trips unchanged; only the storage medium changed.
//!
//! Every store/project function opens a pool via [`open`] and lets the pool
//! drop at the end of the call. SQLite file-open is sub-millisecond, so for a
//! per-command CLI this is simpler than threading a pool through every
//! signature, and WAL mode keeps concurrent readers unblocked.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use super::ts_dir;

/// Absolute path to the project's SQLite database file.
pub fn db_path(root: &Path) -> std::path::PathBuf {
    ts_dir(root).join("testsprite.db")
}

/// Open (creating if needed) the project's SQLite database and ensure the
/// schema exists. Cheap enough to call once per CLI command.
pub async fn open(root: &Path) -> Result<SqlitePool> {
    let dir = ts_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = db_path(root);

    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .with_context(|| format!("opening {}", path.display()))?;

    migrate(&pool).await?;
    Ok(pool)
}

/// Idempotent, additive-only schema creation. Safe to run on every [`open`].
async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(SCHEMA)
        .execute(pool)
        .await
        .context("applying schema")?;
    Ok(())
}

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS project (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    name       TEXT NOT NULL,
    kind       TEXT NOT NULL,
    target_url TEXT
);
CREATE TABLE IF NOT EXISTS tests (
    id         TEXT PRIMARY KEY,
    title      TEXT NOT NULL DEFAULT '',
    kind       TEXT,
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS runs (
    run_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    test_id      TEXT NOT NULL,
    passed       INTEGER NOT NULL,
    verdict      TEXT,
    failure_kind TEXT,
    error        TEXT NOT NULL DEFAULT '',
    code         TEXT NOT NULL DEFAULT '',
    analysis     TEXT,
    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS conversations (
    id         TEXT PRIMARY KEY,
    title      TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS messages (
    msg_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS messages_conv_idx ON messages (conversation_id, msg_id);
CREATE TABLE IF NOT EXISTS pending_actions (
    action_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT NOT NULL,
    kind            TEXT NOT NULL,
    args            TEXT NOT NULL DEFAULT '{}',
    summary         TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL DEFAULT 'pending',
    result          TEXT,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS prd (
    id         TEXT PRIMARY KEY,
    source     TEXT NOT NULL DEFAULT '',
    prd_json   TEXT NOT NULL,
    plan_json  TEXT NOT NULL DEFAULT '[]',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS pending_actions_conv_idx ON pending_actions (conversation_id, action_id);
CREATE INDEX IF NOT EXISTS runs_test_id_idx ON runs (test_id, run_id DESC);
";

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_is_idempotent_and_creates_schema() {
        let root = crate::local::tmp_root();
        // Two opens on the same root must both succeed (schema is IF NOT EXISTS).
        let p1 = open(&root).await.expect("first open");
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN \
             ('project','tests','runs','conversations','messages','pending_actions')",
        )
        .fetch_one(&p1)
        .await
        .expect("query tables");
        assert_eq!(count, 6, "all six tables should exist");
        drop(p1);
        open(&root).await.expect("second open must be idempotent");
        assert!(db_path(&root).exists(), "db file should exist on disk");
    }
}
