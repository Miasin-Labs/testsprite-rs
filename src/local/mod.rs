//! Local, JSON-driven test lifecycle — a thin CLI layer over the Executor seam.
//! `test run` calls `server::executors::for_kind(kind).run()` directly against a
//! local target; there is no server and no tunnel.

pub mod coverage;
pub mod gate;
pub mod generate;
pub mod project;
pub mod run;
pub mod rerun;
pub mod store;
pub mod visual;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::TESTSPRITE_DIR;
use crate::server::executors::TestKind;

/// `testsprite_tests/project.json` — the local project config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub name: String,
    pub kind: TestKind,
    #[serde(rename = "targetUrl", default, skip_serializing_if = "Option::is_none")]
    pub target_url: Option<String>,
}

/// A stored local test case at `testsprite_tests/tests/<id>.json`.
/// Serializes back to the `{id,title,description,kind?,spec?,...}` JSON the
/// executor consumes; unknown fields (e.g. `planSteps`) round-trip via `extra`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalTest {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TestKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

pub fn ts_dir(root: &Path) -> PathBuf {
    root.join(TESTSPRITE_DIR)
}
pub fn project_json(root: &Path) -> PathBuf {
    ts_dir(root).join("project.json")
}
pub fn tests_dir(root: &Path) -> PathBuf {
    ts_dir(root).join("tests")
}
pub fn results_dir(root: &Path) -> PathBuf {
    ts_dir(root).join("results")
}
pub fn fixes_dir(root: &Path) -> PathBuf {
    ts_dir(root).join("fixes")
}

/// Shared test helper: a fresh, unique temp dir under the OS temp dir.
#[cfg(test)]
pub(crate) fn tmp_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tsrs_local_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
