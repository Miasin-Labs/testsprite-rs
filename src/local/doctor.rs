//! `testsprite-rs doctor` — offline environment diagnostics. Checks the LLM
//! key, cargo/coverage/node/gh/python3 toolchains, and Playwright browser
//! caches. Never prints secrets; only reports presence/absence.

use std::process::Command;

use serde_json::json;

/// Run one probe command and return its trimmed stdout on success, `None` on
/// spawn error or non-zero exit.
fn probe(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = if out.stdout.is_empty() {
        out.stderr
    } else {
        out.stdout
    };
    let text = String::from_utf8_lossy(&text).trim().to_string();
    Some(text)
}

enum Status {
    Ok(String),
    Warn(String),
    Fail(String),
}

impl Status {
    /// (`ok|warn|fail`, detail) — the on-the-wire status tag + human detail.
    fn parts(self) -> (&'static str, String) {
        match self {
            Status::Ok(d) => ("ok", d),
            Status::Warn(d) => ("warn", d),
            Status::Fail(d) => ("fail", d),
        }
    }
}

/// One diagnostic result, named and tagged `ok|warn|fail`.
struct Check {
    name: &'static str,
    status: &'static str,
    detail: String,
}

/// Gather every diagnostic (no I/O side effects beyond the probes themselves).
fn collect_checks() -> Vec<Check> {
    let mut checks = Vec::new();
    let mut add = |name: &'static str, status: Status| {
        let (status, detail) = status.parts();
        checks.push(Check {
            name,
            status,
            detail,
        });
    };

    add(
        "openai key",
        if crate::server::llm::resolve_key().is_some() {
            Status::Ok("configured, LLM features enabled".to_string())
        } else {
            Status::Warn("no key — deterministic mode only".to_string())
        },
    );
    add(
        "cargo",
        match probe("cargo", &["--version"]) {
            Some(v) => Status::Ok(v),
            None => Status::Fail("not found on PATH".to_string()),
        },
    );
    add(
        "cargo-llvm-cov",
        match probe("cargo", &["llvm-cov", "--version"]) {
            Some(v) => Status::Ok(v),
            None => Status::Warn("coverage --rust unavailable".to_string()),
        },
    );
    add(
        "node",
        match probe("node", &["--version"]) {
            Some(v) => Status::Ok(v),
            None => Status::Warn("frontend/Playwright unavailable".to_string()),
        },
    );
    add("playwright browsers", playwright_status());
    add(
        "docker",
        match probe("docker", &["--version"]) {
            Some(v) => Status::Ok(format!("{v} (webkit + any-browser via Docker)")),
            None => {
                Status::Warn("webkit executor unavailable (host browsers still work)".to_string())
            }
        },
    );
    add(
        "gh",
        match probe("gh", &["--version"]) {
            Some(v) => Status::Ok(v.lines().next().unwrap_or("").to_string()),
            None => Status::Warn("PR gating unavailable".to_string()),
        },
    );
    add(
        "python3",
        match probe("python3", &["--version"]) {
            Some(v) => Status::Ok(v),
            None => Status::Warn(
                "backend deterministic executor still works via reqwest; python only needed for LLM python tests"
                    .to_string(),
            ),
        },
    );
    checks
}

/// Run every diagnostic check. In text mode prints one `[ok]/[warn]/[fail]`
/// line each; with `json` emits a `DoctorReport` = `{checks:[{name,status,
/// detail}]}` (status ∈ ok|warn|fail). Returns exit code 1 if any check
/// failed, else 0.
pub fn doctor(json_out: bool) -> anyhow::Result<i32> {
    let checks = collect_checks();
    let any_fail = checks.iter().any(|c| c.status == "fail");
    print!("{}", render(&checks, json_out)?);
    Ok(if any_fail { 1 } else { 0 })
}

/// Render checks as `[status] name: detail` lines, or a `DoctorReport` JSON
/// (`{checks:[{name,status,detail}]}`). Pure — the unit-testable core of `doctor`.
fn render(checks: &[Check], json_out: bool) -> anyhow::Result<String> {
    if json_out {
        let report = json!({
            "checks": checks
                .iter()
                .map(|c| json!({ "name": c.name, "status": c.status, "detail": c.detail }))
                .collect::<Vec<_>>(),
        });
        Ok(format!("{}\n", serde_json::to_string_pretty(&report)?))
    } else {
        let mut out = String::new();
        for c in checks {
            out.push_str(&format!("[{}] {}: {}\n", c.status, c.name, c.detail));
        }
        Ok(out)
    }
}

fn playwright_status() -> Status {
    let Some(home) = dirs_home() else {
        return Status::Warn("no home directory to locate ~/.cache/ms-playwright".to_string());
    };
    let cache = home.join(".cache").join("ms-playwright");
    if !cache.exists() {
        return Status::Warn("~/.cache/ms-playwright not found".to_string());
    }
    let engines = ["chromium", "firefox", "webkit"];
    let found: Vec<&str> = engines
        .iter()
        .copied()
        .filter(|engine| {
            std::fs::read_dir(&cache)
                .map(|mut entries| {
                    entries.any(|e| {
                        e.ok()
                            .map(|e| {
                                e.file_name()
                                    .to_string_lossy()
                                    .to_ascii_lowercase()
                                    .starts_with(engine)
                            })
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
        .collect();
    if found.is_empty() {
        Status::Warn("no browsers found under ~/.cache/ms-playwright".to_string())
    } else {
        Status::Ok(found.join(", "))
    }
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &'static str, status: &'static str) -> Check {
        Check {
            name,
            status,
            detail: "d".to_string(),
        }
    }

    #[test]
    fn render_json_is_valid_doctor_report() {
        let out = render(&[check("x", "ok"), check("y", "fail")], true).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = v["checks"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(
            arr.iter()
                .all(|c| matches!(c["status"].as_str(), Some("ok" | "warn" | "fail")))
        );
        assert_eq!(arr[1]["name"], "y");
    }

    #[test]
    fn render_text_is_status_lines() {
        assert_eq!(
            render(&[check("x", "warn")], false).unwrap(),
            "[warn] x: d\n"
        );
    }

    #[test]
    fn status_parts_preserve_wire_tags() {
        assert_eq!(Status::Ok("yes".into()).parts(), ("ok", "yes".to_string()));
        assert_eq!(
            Status::Warn("careful".into()).parts(),
            ("warn", "careful".to_string())
        );
        assert_eq!(
            Status::Fail("no".into()).parts(),
            ("fail", "no".to_string())
        );
    }
}
