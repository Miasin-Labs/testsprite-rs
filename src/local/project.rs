//! `testsprite_tests/project.json` lifecycle: init, load, show.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};

use crate::server::executors::TestKind;

use super::{Project, project_json, ts_dir};

/// Create `testsprite_tests/` if needed and write `project.json`. Overwrites
/// any existing project.json. Returns the path to project.json.
pub fn init(root: &Path, kind: TestKind, name: &str, url: Option<&str>) -> anyhow::Result<PathBuf> {
    let dir = ts_dir(root);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let project = Project {
        name: name.to_string(),
        kind,
        target_url: url.map(|u| u.to_string()),
    };

    let path = project_json(root);
    let mut body = serde_json::to_string_pretty(&project)?;
    body.push('\n');
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Read and parse `project.json`.
pub fn load(root: &Path) -> anyhow::Result<Project> {
    let path = project_json(root);
    let body = match std::fs::read_to_string(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(anyhow!(
                "no project.json at {}; run `testsprite-rs project init` first",
                path.display()
            ));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let project: Project =
        serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))?;
    Ok(project)
}

/// Load and pretty-print `project.json`.
pub fn show(root: &Path) -> anyhow::Result<()> {
    let project = load(root)?;
    println!("{}", serde_json::to_string_pretty(&project)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_then_load_round_trips_with_url() {
        let root = crate::local::tmp_root();
        init(&root, TestKind::Backend, "my-app", Some("http://localhost:3000")).unwrap();

        let project = load(&root).unwrap();
        assert_eq!(project.name, "my-app");
        assert_eq!(project.kind, TestKind::Backend);
        assert_eq!(project.target_url.as_deref(), Some("http://localhost:3000"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn init_then_load_round_trips_without_url() {
        let root = crate::local::tmp_root();
        init(&root, TestKind::Rust, "unit-app", None).unwrap();

        let project = load(&root).unwrap();
        assert_eq!(project.name, "unit-app");
        assert_eq!(project.kind, TestKind::Rust);
        assert_eq!(project.target_url, None);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn load_missing_project_errors() {
        let root = crate::local::tmp_root();
        let err = load(&root).unwrap_err();
        assert!(err.to_string().contains("project init"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
