//! End-to-end CLI tests: drive the compiled `testsprite-rs` binary through its
//! cheap, deterministic, offline subcommands and assert on stdout/stderr.
//!
//! Every invocation runs in a fresh, unique temp directory (so the per-project
//! SQLite store at `testsprite_tests/testsprite.db` and the schedules/project
//! files are isolated) and with the API-key env vars removed, so nothing here
//! can touch the network. Slow/LLM/tunnel subcommands (serve, backend, gate,
//! loop, coverage, visual, generate/run/audit/explore, discord) are never
//! invoked.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

/// Absolute path to the binary under test (built by `cargo test`).
const BIN: &str = env!("CARGO_BIN_EXE_testsprite-rs");

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A fresh, process-unique temp directory. `tests/cli.rs` cannot use crate
/// internals, so this reimplements the isolation locally with pid + a counter.
fn fresh_dir() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("tsrs_cli_{}_{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fresh temp dir");
    dir
}

/// Build a `Command` for the binary, scoped to `dir` and stripped of the
/// API-key env vars so no invocation can reach a real endpoint.
fn bin(dir: &Path) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(dir);
    c.env_remove("API_KEY");
    c.env_remove("TSMCP_API_KEY");
    c.env_remove("OPENAI_API_KEY");
    c
}

/// Run the binary with `args` in `dir` and return its captured output.
fn run(dir: &Path, args: &[&str]) -> Output {
    bin(dir)
        .args(args)
        .output()
        .expect("spawn testsprite-rs binary")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Write `{"id":"TC001","title":"T"}` into `dir` and `test add --file` it.
fn add_tc001(dir: &Path) {
    let case = dir.join("case.json");
    std::fs::write(&case, r#"{"id":"TC001","title":"T"}"#).unwrap();
    let out = run(dir, &["test", "add", "--file", case.to_str().unwrap()]);
    assert!(out.status.success(), "test add failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("TC001"),
        "add did not echo id: {}",
        stdout(&out)
    );
}

fn cleanup(dir: &Path) {
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn test_list_empty_then_add_and_all_output_formats() {
    let dir = fresh_dir();

    // Empty store: `test list` succeeds with no rows on stdout.
    let out = run(&dir, &["test", "list"]);
    assert!(out.status.success(), "empty list failed: {}", stderr(&out));
    assert!(
        stdout(&out).trim().is_empty(),
        "empty store should print nothing, got: {:?}",
        stdout(&out)
    );

    add_tc001(&dir);

    // text
    let out = run(&dir, &["test", "list"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("TC001"), "text list missing TC001");

    // json — contains the id and is a parseable JSON array
    let out = run(&dir, &["test", "list", "--output", "json"]);
    assert!(out.status.success());
    let s = stdout(&out);
    assert!(s.contains("TC001"), "json list missing TC001: {s}");
    let v: serde_json::Value = serde_json::from_str(&s).expect("json list parses");
    assert!(v.is_array(), "json list is an array");

    // csv
    let out = run(&dir, &["test", "list", "--output", "csv"]);
    assert!(out.status.success());
    let s = stdout(&out);
    assert!(s.contains("id,title,kind"), "csv header missing: {s}");
    assert!(s.contains("TC001"), "csv list missing TC001: {s}");

    // ndjson
    let out = run(&dir, &["test", "list", "--output", "ndjson"]);
    assert!(out.status.success());
    let s = stdout(&out);
    assert!(s.contains("TC001"), "ndjson list missing TC001: {s}");
    let first = s.lines().next().expect("at least one ndjson line");
    serde_json::from_str::<serde_json::Value>(first).expect("ndjson line parses");

    // bogus format => nonzero exit and a helpful stderr message.
    let out = run(&dir, &["test", "list", "--output", "bogus"]);
    assert!(!out.status.success(), "bogus --output should fail");
    assert!(
        stderr(&out).contains("invalid --output"),
        "expected 'invalid --output' on stderr, got: {}",
        stderr(&out)
    );

    cleanup(&dir);
}

#[test]
fn test_get_rename_delete_round_trip() {
    let dir = fresh_dir();
    add_tc001(&dir);

    // get => JSON containing the title.
    let out = run(&dir, &["test", "get", "TC001"]);
    assert!(out.status.success(), "get failed: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("get JSON parses");
    assert_eq!(v["id"], "TC001");
    assert_eq!(v["title"], "T");

    // rename => new title is reflected on the next get.
    let out = run(&dir, &["test", "rename", "TC001", "--title", "Renamed"]);
    assert!(out.status.success(), "rename failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("renamed"),
        "rename message: {}",
        stdout(&out)
    );

    let out = run(&dir, &["test", "get", "TC001"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("Renamed"), "renamed title not stored");

    // delete => gone; list is empty again.
    let out = run(&dir, &["test", "delete", "TC001"]);
    assert!(out.status.success(), "delete failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("deleted"),
        "delete message: {}",
        stdout(&out)
    );

    let out = run(&dir, &["test", "list"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).trim().is_empty(),
        "store should be empty after delete"
    );

    cleanup(&dir);
}

#[test]
fn test_export_then_import_round_trip() {
    let src = fresh_dir();
    add_tc001(&src);

    // export => a JSON array to stdout containing the stored case.
    let out = run(&src, &["test", "export"]);
    assert!(out.status.success(), "export failed: {}", stderr(&out));
    let s = stdout(&out);
    let arr: serde_json::Value = serde_json::from_str(&s).expect("export JSON parses");
    assert!(arr.is_array(), "export is a JSON array");
    assert_eq!(arr.as_array().unwrap().len(), 1);
    assert!(s.contains("TC001"), "export missing TC001");

    // import that export into a FRESH project => the case reappears there.
    let dst = fresh_dir();
    let export_file = dst.join("export.json");
    std::fs::write(&export_file, &s).unwrap();
    let out = run(&dst, &["test", "import", export_file.to_str().unwrap()]);
    assert!(out.status.success(), "import failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("imported 1 test"),
        "import summary: {}",
        stdout(&out)
    );

    let out = run(&dst, &["test", "list"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("TC001"), "imported case not listed");

    cleanup(&src);
    cleanup(&dst);
}

#[test]
fn test_scaffold_backend_json_and_lint_json_parse() {
    let dir = fresh_dir();

    let out = run(&dir, &["test", "scaffold", "--type", "backend", "--json"]);
    assert!(out.status.success(), "scaffold failed: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("scaffold --json parses");
    assert_eq!(v["type"], "backend");
    assert!(v["code"].is_string(), "scaffold carries code");

    // lint on an empty store => valid JSON report, exit 0 (no hard issues).
    let out = run(&dir, &["test", "lint", "--json"]);
    assert!(out.status.success(), "lint failed: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("lint --json parses");
    assert_eq!(v["checked"], 0);
    assert!(v["issues"].is_array(), "lint report has issues array");

    cleanup(&dir);
}

#[test]
fn prd_list_empty_text_and_json() {
    let dir = fresh_dir();

    let out = run(&dir, &["prd", "list"]);
    assert!(out.status.success(), "prd list failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("no PRDs yet"),
        "expected empty-PRD hint, got: {}",
        stdout(&out)
    );

    let out = run(&dir, &["prd", "list", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("prd list --json parses");
    assert_eq!(v, serde_json::json!([]), "no PRDs => empty JSON array");

    cleanup(&dir);
}

#[test]
fn project_init_then_show_contains_name() {
    let dir = fresh_dir();

    let out = run(
        &dir,
        &[
            "project",
            "init",
            "--name",
            "demo",
            "--url",
            "http://127.0.0.1:1",
        ],
    );
    assert!(
        out.status.success(),
        "project init failed: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("wrote"),
        "init message: {}",
        stdout(&out)
    );

    let out = run(&dir, &["project", "show"]);
    assert!(
        out.status.success(),
        "project show failed: {}",
        stderr(&out)
    );
    let s = stdout(&out);
    assert!(s.contains("demo"), "show missing project name: {s}");
    let v: serde_json::Value = serde_json::from_str(&s).expect("project show is JSON");
    assert_eq!(v["name"], "demo");

    cleanup(&dir);
}

#[test]
fn schedule_add_list_crontab_remove_and_run_missing() {
    let dir = fresh_dir();

    let out = run(
        &dir,
        &[
            "schedule",
            "add",
            "nightly",
            "--group",
            "g",
            "--cadence",
            "daily",
        ],
    );
    assert!(
        out.status.success(),
        "schedule add failed: {}",
        stderr(&out)
    );
    assert!(
        stdout(&out).contains("nightly"),
        "add message: {}",
        stdout(&out)
    );

    let out = run(&dir, &["schedule", "list"]);
    assert!(out.status.success());
    let s = stdout(&out);
    assert!(s.contains("nightly"), "list missing schedule: {s}");
    assert!(s.contains("group=g"), "list missing group: {s}");

    // crontab: one line per schedule, referencing this binary and `test run`.
    let out = run(&dir, &["schedule", "crontab"]);
    assert!(out.status.success(), "crontab failed: {}", stderr(&out));
    let s = stdout(&out);
    assert!(s.contains(BIN), "crontab should embed the binary path: {s}");
    assert!(
        s.contains("test run --group"),
        "crontab should invoke `test run --group`: {s}"
    );
    assert!(
        s.contains("testsprite-rs:nightly"),
        "crontab tag missing: {s}"
    );

    let out = run(&dir, &["schedule", "remove", "nightly"]);
    assert!(out.status.success(), "remove failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("removed"),
        "remove message: {}",
        stdout(&out)
    );

    // run a schedule that does not exist => nonzero exit + message on stderr.
    let out = run(&dir, &["schedule", "run", "does-not-exist"]);
    assert!(
        !out.status.success(),
        "running a missing schedule should fail"
    );
    assert!(
        stderr(&out).contains("no schedule named"),
        "expected 'no schedule named' on stderr, got: {}",
        stderr(&out)
    );

    cleanup(&dir);
}

#[test]
fn completions_bash_mentions_binary() {
    let dir = fresh_dir();
    let out = run(&dir, &["completions", "bash"]);
    assert!(out.status.success(), "completions failed: {}", stderr(&out));
    assert!(
        stdout(&out).contains("testsprite-rs"),
        "bash completions should mention the binary name"
    );
    cleanup(&dir);
}

#[test]
fn doctor_json_is_a_report_with_a_checks_array() {
    let dir = fresh_dir();
    // NB: do NOT assert the exit code — probed tools may be missing on CI.
    let out = run(&dir, &["doctor", "--json"]);
    let v: serde_json::Value =
        serde_json::from_str(&stdout(&out)).expect("doctor --json emits JSON");
    let checks = v["checks"]
        .as_array()
        .expect("doctor report has a checks array");
    assert!(
        !checks.is_empty(),
        "doctor should report at least one check"
    );
    for c in checks {
        assert!(c["name"].is_string(), "each check has a name");
        assert!(c["status"].is_string(), "each check has a status");
    }
    cleanup(&dir);
}

#[test]
fn account_without_key_reports_no_api_key_offline() {
    let dir = fresh_dir();
    // With the key env vars removed, `account` short-circuits to a "No API Key"
    // JSON object BEFORE any network call — the only safe way to exercise it.
    let out = run(&dir, &["account"]);
    assert!(out.status.success(), "account failed: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("account emits JSON");
    assert_eq!(v["status"], "No API Key");
    cleanup(&dir);
}

#[test]
fn console_execute_without_config_fails_fast() {
    let dir = fresh_dir();
    // `generate-code-and-execute` with no committed config reads no network:
    // it errors on the missing executionArgs immediately.
    let out = run(&dir, &["generate-code-and-execute"]);
    assert!(
        !out.status.success(),
        "console execute with no config should fail"
    );
    assert!(
        stderr(&out).contains("executionArgs missing"),
        "expected 'executionArgs missing' on stderr, got: {}",
        stderr(&out)
    );
    cleanup(&dir);
}
