//! On-disk layout under a project root. Mirrors `common/path.ts`.

use std::path::{Path, PathBuf};

pub const TESTSPRITE_DIR: &str = "testsprite_tests";

/// Resolved paths relative to a project root.
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new(project_path: impl AsRef<Path>) -> Self {
        Self {
            root: project_path.as_ref().to_path_buf(),
        }
    }

    fn ts(&self) -> PathBuf {
        self.root.join(TESTSPRITE_DIR)
    }
    fn tmp(&self) -> PathBuf {
        self.ts().join("tmp")
    }

    #[allow(dead_code)]
    pub fn dir(&self) -> PathBuf {
        self.ts()
    }
    #[allow(dead_code)]
    pub fn tmp_dir(&self) -> PathBuf {
        self.tmp()
    }
    pub fn config(&self) -> PathBuf {
        self.tmp().join("config.json")
    }
    pub fn code_summary(&self) -> PathBuf {
        self.tmp().join("code_summary.yaml")
    }
    pub fn raw_prd_dir(&self) -> PathBuf {
        self.tmp().join("prd_files")
    }
    pub fn standard_prd(&self) -> PathBuf {
        self.ts().join("standard_prd.json")
    }
    pub fn frontend_test_plan(&self) -> PathBuf {
        self.ts().join("testsprite_frontend_test_plan.json")
    }
    pub fn backend_test_plan(&self) -> PathBuf {
        self.ts().join("testsprite_backend_test_plan.json")
    }
    pub fn test_results(&self) -> PathBuf {
        self.tmp().join("test_results.json")
    }
    pub fn raw_report(&self) -> PathBuf {
        self.tmp().join("raw_report.md")
    }
    pub fn test_report(&self) -> PathBuf {
        self.ts().join("testsprite-mcp-test-report.md")
    }
    pub fn execution_lock(&self) -> PathBuf {
        self.tmp().join("execution.lock")
    }
    pub fn test_code_dir(&self) -> PathBuf {
        self.ts()
    }
}

/// The gitignore entry the original plugin auto-appends (the config holds creds).
pub const GITIGNORE_ENTRY: &str = "testsprite_tests/tmp/config.json";

#[cfg(test)]
mod tests {
    #[test]
    fn paths_match_testsprite_layout() {
        let p = super::Paths::new("/repo");
        assert_eq!(p.dir(), std::path::PathBuf::from("/repo/testsprite_tests"));
        assert_eq!(
            p.code_summary(),
            std::path::PathBuf::from("/repo/testsprite_tests/tmp/code_summary.yaml")
        );
        assert_eq!(
            p.standard_prd(),
            std::path::PathBuf::from("/repo/testsprite_tests/standard_prd.json")
        );
        assert_eq!(
            p.frontend_test_plan(),
            std::path::PathBuf::from("/repo/testsprite_tests/testsprite_frontend_test_plan.json")
        );
        assert_eq!(
            p.test_report(),
            std::path::PathBuf::from("/repo/testsprite_tests/testsprite-mcp-test-report.md")
        );
        assert_eq!(
            p.tmp_dir(),
            std::path::PathBuf::from("/repo/testsprite_tests/tmp")
        );
        assert_eq!(
            p.raw_prd_dir(),
            std::path::PathBuf::from("/repo/testsprite_tests/tmp/prd_files")
        );
        assert_eq!(
            p.test_results(),
            std::path::PathBuf::from("/repo/testsprite_tests/tmp/test_results.json")
        );
        assert_eq!(
            p.raw_report(),
            std::path::PathBuf::from("/repo/testsprite_tests/tmp/raw_report.md")
        );
        assert_eq!(
            p.execution_lock(),
            std::path::PathBuf::from("/repo/testsprite_tests/tmp/execution.lock")
        );
        assert_eq!(p.test_code_dir(), p.dir());
    }
}
