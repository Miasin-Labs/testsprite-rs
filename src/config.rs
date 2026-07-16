//! Config read/save + "wait until committed". Mirrors `common/config.ts`.

use std::time::Duration;

use tokio::time::sleep;

use crate::paths::Paths;
use crate::types::Config;

pub async fn read_config(project_path: &str) -> Config {
    let p = Paths::new(project_path).config();
    match tokio::fs::read_to_string(&p).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::error!("readConfig parse error: {e}");
            Config {
                status: "init".into(),
                ..Default::default()
            }
        }),
        Err(e) => {
            tracing::error!("readConfig error: {e}");
            Config {
                status: "init".into(),
                ..Default::default()
            }
        }
    }
}

pub async fn save_config(project_path: &str, config: &Config) -> anyhow::Result<()> {
    let p = Paths::new(project_path).config();
    if let Some(dir) = p.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let json = serde_json::to_string_pretty(config)?;
    tokio::fs::write(&p, json).await?;
    Ok(())
}

/// Whether the user has committed the config (set in the original via the local
/// web UI). Part of the ported surface; the headless CLI seeds `status` directly.
#[allow(dead_code)]
pub async fn check_config_committed(project_path: &str) -> bool {
    read_config(project_path).await.status == "commited"
}

/// Poll until the user commits the config via the local web UI.
#[allow(dead_code)]
pub async fn wait_config_committed(project_path: &str) {
    loop {
        if check_config_committed(project_path).await {
            return;
        }
        sleep(Duration::from_millis(3000)).await;
    }
}

/// Auto-append the config path to `.gitignore` (the config can hold creds).
pub async fn ensure_gitignore_entry(project_path: &str) {
    let gi = std::path::Path::new(project_path).join(".gitignore");
    let entry = crate::paths::GITIGNORE_ENTRY;
    let write_result = match tokio::fs::read_to_string(&gi).await {
        Ok(content) if content.contains(entry) => return,
        Ok(content) => tokio::fs::write(&gi, format!("{content}\n{entry}\n")).await,
        Err(_) => tokio::fs::write(&gi, format!("{entry}\n")).await,
    };
    if let Err(e) = write_result {
        tracing::warn!("could not update .gitignore: {e}");
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn save_read_and_commit_status_round_trip() {
        let root = crate::local::tmp_root();
        let cfg = crate::types::Config {
            status: "commited".to_string(),
            project_name: Some("demo".to_string()),
            ..Default::default()
        };
        super::save_config(root.to_str().unwrap(), &cfg)
            .await
            .unwrap();
        let loaded = super::read_config(root.to_str().unwrap()).await;
        assert_eq!(loaded.status, "commited");
        assert_eq!(loaded.project_name.as_deref(), Some("demo"));
        assert!(super::check_config_committed(root.to_str().unwrap()).await);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn bad_or_missing_config_falls_back_to_init() {
        let root = crate::local::tmp_root();
        assert_eq!(
            super::read_config(root.to_str().unwrap()).await.status,
            "init"
        );
        let path = crate::paths::Paths::new(&root).config();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{not-json").unwrap();
        assert_eq!(
            super::read_config(root.to_str().unwrap()).await.status,
            "init"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn gitignore_entry_is_idempotent() {
        let root = crate::local::tmp_root();
        super::ensure_gitignore_entry(root.to_str().unwrap()).await;
        super::ensure_gitignore_entry(root.to_str().unwrap()).await;
        let body = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(body.matches(crate::paths::GITIGNORE_ENTRY).count(), 1);
        std::fs::remove_dir_all(root).ok();
    }
}
