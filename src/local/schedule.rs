//! Local schedule store — name a `(group, cadence)` pair so it can be
//! re-run on demand (`crate::local::run`) or emitted as OS crontab lines,
//! the local analogue of V3's monitoring/cron re-verification.
//!
//! Persisted as a pretty JSON array at `testsprite_tests/schedules.json`.
//! This is a plain config file (not the sqlx store), so I/O here is
//! synchronous `std::fs`.

use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::ts_dir;

const SCHEDULES_FILE: &str = "schedules.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    pub name: String,
    pub group: String,
    pub cadence: String,
}

/// Map a cadence keyword to a 5-field cron expression. `None` if `cadence`
/// is not one of the 4 supported values.
pub fn cadence_cron(cadence: &str) -> Option<&'static str> {
    match cadence {
        "hourly" => Some("0 * * * *"),
        "daily" => Some("0 9 * * *"),
        "weekly" => Some("0 9 * * 1"),
        "monthly" => Some("0 9 1 * *"),
        _ => None,
    }
}

fn schedules_path(root: &Path) -> std::path::PathBuf {
    ts_dir(root).join(SCHEDULES_FILE)
}

/// Read all stored schedules; `[]` if the file does not exist yet.
fn read_all(root: &Path) -> Result<Vec<Schedule>> {
    let path = schedules_path(root);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("reading schedules file {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let schedules: Vec<Schedule> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing schedules file {}", path.display()))?;
    Ok(schedules)
}

/// Persist `schedules` as a pretty JSON array, creating `testsprite_tests/`
/// if needed.
fn write_all(root: &Path, schedules: &[Schedule]) -> Result<()> {
    let dir = ts_dir(root);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = schedules_path(root);
    let raw = serde_json::to_string_pretty(schedules).context("serializing schedules")?;
    fs::write(&path, raw).with_context(|| format!("writing schedules file {}", path.display()))?;
    Ok(())
}

/// Upsert a schedule by name; bails on an invalid cadence.
pub fn add(root: &Path, name: &str, group: &str, cadence: &str) -> Result<()> {
    if cadence_cron(cadence).is_none() {
        bail!(
            "invalid cadence {cadence:?}: expected one of hourly, daily, weekly, monthly"
        );
    }
    let mut schedules = read_all(root)?;
    let entry = Schedule {
        name: name.to_string(),
        group: group.to_string(),
        cadence: cadence.to_string(),
    };
    match schedules.iter_mut().find(|s| s.name == name) {
        Some(existing) => *existing = entry,
        None => schedules.push(entry),
    }
    write_all(root, &schedules)
}

/// All stored schedules, sorted by name.
pub fn list(root: &Path) -> Result<Vec<Schedule>> {
    let mut schedules = read_all(root)?;
    schedules.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(schedules)
}

/// The schedule named `name`, if any.
pub fn get(root: &Path, name: &str) -> Result<Option<Schedule>> {
    let schedules = read_all(root)?;
    Ok(schedules.into_iter().find(|s| s.name == name))
}

/// Remove the schedule named `name`. Returns `true` if one was removed.
pub fn remove(root: &Path, name: &str) -> Result<bool> {
    let mut schedules = read_all(root)?;
    let before = schedules.len();
    schedules.retain(|s| s.name != name);
    let removed = schedules.len() != before;
    if removed {
        write_all(root, &schedules)?;
    }
    Ok(removed)
}

/// Emit one crontab line per stored schedule:
/// `<cron> cd <root> && <bin> test run --group <group>  # testsprite-rs:<name>`
pub fn crontab(root: &Path, bin: &str) -> Result<String> {
    let schedules = list(root)?;
    let mut out = String::new();
    for s in &schedules {
        let cron = cadence_cron(&s.cadence)
            .with_context(|| format!("schedule {:?} has invalid cadence {:?}", s.name, s.cadence))?;
        out.push_str(&format!(
            "{cron} cd {} && {} test run --group {}  # testsprite-rs:{}\n",
            sh_quote(&root.display().to_string()),
            sh_quote(bin),
            sh_quote(&s.group),
            s.name
        ));
    }
    Ok(out)
}

/// POSIX single-quote a value so the emitted crontab line survives spaces and
/// shell metacharacters in the path, binary, or group name.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::tmp_root;

    #[test]
    fn cadence_cron_maps_known_values_and_rejects_garbage() {
        assert_eq!(cadence_cron("hourly"), Some("0 * * * *"));
        assert_eq!(cadence_cron("daily"), Some("0 9 * * *"));
        assert_eq!(cadence_cron("weekly"), Some("0 9 * * 1"));
        assert_eq!(cadence_cron("monthly"), Some("0 9 1 * *"));
        assert_eq!(cadence_cron("garbage"), None);
    }

    #[test]
    fn add_then_list_round_trips_and_upserts() {
        let root = tmp_root();
        add(&root, "nightly", "smoke", "daily").unwrap();
        let all = list(&root).unwrap();
        assert_eq!(all, vec![Schedule {
            name: "nightly".to_string(),
            group: "smoke".to_string(),
            cadence: "daily".to_string(),
        }]);

        // Same name, different group -> upsert, not duplicate.
        add(&root, "nightly", "regression", "weekly").unwrap();
        let all = list(&root).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].group, "regression");
        assert_eq!(all[0].cadence, "weekly");
    }

    #[test]
    fn add_rejects_invalid_cadence() {
        let root = tmp_root();
        let err = add(&root, "nightly", "smoke", "biweekly").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("hourly"));
        assert!(msg.contains("daily"));
        assert!(msg.contains("weekly"));
        assert!(msg.contains("monthly"));
    }

    #[test]
    fn remove_and_get_reflect_presence() {
        let root = tmp_root();
        add(&root, "nightly", "smoke", "daily").unwrap();
        assert!(get(&root, "nightly").unwrap().is_some());
        assert!(remove(&root, "nightly").unwrap());
        assert!(!remove(&root, "nightly").unwrap());
        assert!(get(&root, "nightly").unwrap().is_none());
    }

    #[test]
    fn crontab_emits_one_line_per_schedule() {
        let root = tmp_root();
        add(&root, "nightly", "smoke", "daily").unwrap();
        add(&root, "hourly-check", "core", "hourly").unwrap();

        let out = crontab(&root, "testsprite").unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);

        let nightly_line = lines
            .iter()
            .find(|l| l.contains("testsprite-rs:nightly"))
            .unwrap();
        assert!(nightly_line.starts_with("0 9 * * *"));
        assert!(nightly_line.contains("test run --group 'smoke'"));

        let hourly_line = lines
            .iter()
            .find(|l| l.contains("testsprite-rs:hourly-check"))
            .unwrap();
        assert!(hourly_line.starts_with("0 * * * *"));
        assert!(hourly_line.contains("test run --group 'core'"));
    }

    #[test]
    fn crontab_shell_quotes_group_with_spaces() {
        let root = tmp_root();
        add(&root, "sp", "my group", "daily").unwrap();
        let out = crontab(&root, "testsprite").unwrap();
        // A spaced group must stay a SINGLE argument via single-quoting.
        assert!(
            out.contains("test run --group 'my group'"),
            "spaced group not quoted: {out}"
        );
    }

    #[test]
    fn crontab_is_empty_string_with_no_schedules() {
        let root = tmp_root();
        assert_eq!(crontab(&root, "testsprite").unwrap(), "");
    }
}
