//! Throwaway git worktree at a past revision — the base tree a cross-version
//! fault-check runs against.
//!
//! A generated regression test only earns its place if it FAILS on the code
//! before the change and PASSES after: a test green on both revisions asserts
//! nothing the change touched, and coverage is a near-useless proxy for this
//! (studies find >99% of naive "regression" tests pass on the pre-change tree
//! while still executing the changed lines). Checking that needs the old
//! source, which is what this provides.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A `git worktree` checked out at a revision, removed on drop.
pub struct ScratchWorktree {
    root: PathBuf,
    path: PathBuf,
}

impl ScratchWorktree {
    /// Create a detached worktree of `root` at `rev` under a unique temp path.
    /// Errors if `root` is not a git repo or `rev` is unknown.
    pub fn create(root: &Path, rev: &str) -> anyhow::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "testsprite-rs-base-{}-{}",
            std::process::id(),
            sanitize(rev),
        ));
        // A stale path from a crashed prior run would make `add` fail.
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "remove", "--force"])
            .arg(&path)
            .output();
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["worktree", "add", "--detach", "--force"])
            .arg(&path)
            .arg(rev)
            .output()
            .map_err(|e| anyhow::anyhow!("git worktree add failed to launch: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "git worktree add {rev} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Self {
            root: root.to_path_buf(),
            path,
        })
    }

    /// The checked-out base tree.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchWorktree {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output();
        // Belt-and-suspenders: if git kept the registration, still drop files.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Sanitize a revision string into a filesystem-safe suffix.
fn sanitize(rev: &str) -> String {
    let s: String = rev
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    s.trim_matches('_').chars().take(40).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_revision_into_a_path_safe_suffix() {
        assert_eq!(sanitize("HEAD~1"), "HEAD_1");
        assert_eq!(sanitize("origin/main"), "origin_main");
        assert_eq!(sanitize("abc123"), "abc123");
    }

    #[test]
    fn creates_and_removes_a_base_worktree() {
        // Build a throwaway repo with two commits so a base revision exists.
        let repo = crate::local::tmp_root();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            std::fs::remove_dir_all(&repo).ok();
            return; // no git available — nothing to test
        }
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "v1\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "one"]);
        std::fs::write(repo.join("f.txt"), "v2\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "two"]);

        let base_path;
        {
            let wt = ScratchWorktree::create(&repo, "HEAD~1").unwrap();
            base_path = wt.path().to_path_buf();
            // The base worktree carries the OLD file contents.
            assert_eq!(
                std::fs::read_to_string(wt.path().join("f.txt")).unwrap(),
                "v1\n"
            );
            assert!(base_path.exists());
        }
        // Dropped → removed.
        assert!(!base_path.exists(), "worktree should be cleaned on drop");

        std::fs::remove_dir_all(&repo).ok();
    }
}
