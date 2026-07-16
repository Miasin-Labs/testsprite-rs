//! Rust executor — "TestSprite for this repo".
//!
//! The target is a path to a Rust crate directory. For each case the LLM
//! generates a `#[test]` (edge/boundary/error paths) which we drop into a temp
//! file inside the crate's `tests/` dir, run with `cargo test`, then remove.
//! Deterministic fallback: a smoke test asserting the crate compiles
//! (`cargo build`).

use serde_json::Value;
use uuid::Uuid;

use super::{ExecCtx, Executor, Outcome, lead_with_first_error, repair};

/// Bounded LLM regeneration rounds after a compile failure.
const REPAIR_ROUNDS: usize = 2;

pub struct RustExecutor;

#[async_trait::async_trait]
impl Executor for RustExecutor {
    fn label(&self) -> &'static str {
        "rust"
    }

    async fn run(&self, case: &Value, ctx: &ExecCtx) -> Outcome {
        let crate_dir = ctx.target.clone();
        // Agent-provided code: compile + run it directly, no LLM needed. A
        // compile failure still gets stamped `build error:` — it's a broken
        // test file, not a product bug.
        if let Some(code) = case
            .get("code")
            .and_then(|v| v.as_str())
            .filter(|c| !c.trim().is_empty())
        {
            return stamp_build_error(run_cargo_test(&crate_dir, code).await);
        }

        // No LLM → smoke test: does the crate compile?
        let Some(llm) = ctx.llm.as_ref() else {
            return cargo_build(&crate_dir).await;
        };

        let code = match llm.generate_rust_test(case, &ctx.prd, &crate_dir).await {
            Ok(code) => repair::deterministic_fixups(&code),
            Err(e) => {
                return Outcome::fail(format!("rust test generation failed: {e}"), String::new());
            }
        };
        let mut outcome = run_cargo_test(&crate_dir, &code).await;

        // Compile-repair loop: feed the exact diagnostic back, bounded, and
        // never past the first attempt that reaches the test runner.
        let mut rounds = 0;
        while rounds < REPAIR_ROUNDS
            && !outcome.passed
            && repair::is_compile_failure(&outcome.error)
        {
            rounds += 1;
            match llm.repair_code("rust", &outcome.code, &outcome.error).await {
                Ok(fixed) => {
                    let fixed = repair::deterministic_fixups(&fixed);
                    tracing::info!("rust: compile-repair round {rounds}");
                    outcome = run_cargo_test(&crate_dir, &fixed).await;
                }
                Err(e) => {
                    tracing::warn!("rust: compile-repair round {rounds} failed: {e}");
                    break;
                }
            }
        }
        stamp_build_error(outcome)
    }
}

/// Re-label a compile failure as `build error:` so the verdict layer records
/// `build_error` (Blocked) instead of misreading rustc output as a product
/// assertion failure.
fn stamp_build_error(outcome: Outcome) -> Outcome {
    if !outcome.passed && repair::is_compile_failure(&outcome.error) {
        return Outcome::fail(
            format!("build error: {}", outcome.error),
            outcome.code.clone(),
        );
    }
    outcome
}

/// Deterministic fallback: `cargo build` the crate.
async fn cargo_build(crate_dir: &str) -> Outcome {
    let code = format!("// smoke: cargo build in {crate_dir}");
    let out = tokio::process::Command::new("cargo")
        .arg("build")
        .current_dir(crate_dir)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => Outcome::pass(code),
        Ok(o) => Outcome::fail(
            lead_with_first_error(&String::from_utf8_lossy(&o.stderr), 2000),
            code,
        ),
        Err(e) => Outcome::fail(format!("cargo failed to launch: {e}"), code),
    }
}

/// Write the generated test into `tests/<uuid>.rs`, run `cargo test` filtered to
/// that file's binary, then remove it.
async fn run_cargo_test(crate_dir: &str, code: &str) -> Outcome {
    let tests_dir = std::path::Path::new(crate_dir).join("tests");
    if let Err(e) = tokio::fs::create_dir_all(&tests_dir).await {
        return Outcome::fail(format!("could not create tests/: {e}"), code.to_string());
    }
    let stem = format!("ts_rs_gen_{}", Uuid::new_v4().simple());
    let file = tests_dir.join(format!("{stem}.rs"));
    if let Err(e) = tokio::fs::write(&file, code).await {
        return Outcome::fail(format!("could not write test file: {e}"), code.to_string());
    }

    let out = tokio::process::Command::new("cargo")
        .args(["test", "--test", &stem])
        .current_dir(crate_dir)
        .output()
        .await;

    if let Err(e) = tokio::fs::remove_file(&file).await {
        tracing::debug!("could not remove generated test {file:?}: {e}");
    }

    match out {
        Ok(o) if o.status.success() => Outcome::pass(code.to_string()),
        Ok(o) => {
            let mut msg = String::from_utf8_lossy(&o.stdout).to_string();
            msg.push_str(&String::from_utf8_lossy(&o.stderr));
            Outcome::fail(lead_with_first_error(&msg, 2000), code.to_string())
        }
        Err(e) => Outcome::fail(format!("cargo failed to launch: {e}"), code.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_failures_get_the_build_error_stamp_but_test_failures_do_not() {
        // A rustc compile failure → re-stamped, so verdict classifies it
        // build_error (Blocked), never a product assertion failure.
        let compile = Outcome::fail(
            "error[E0425]: cannot find value `x`\nerror: could not compile `demo`".to_string(),
            "code".to_string(),
        );
        let stamped = stamp_build_error(compile);
        assert!(
            stamped.error.starts_with("build error:"),
            "{}",
            stamped.error
        );
        let (v, fk) = crate::local::verdict::classify(
            false,
            &stamped.error,
            crate::server::executors::TestKind::Rust,
        );
        assert_eq!(v, crate::local::verdict::Verdict::Blocked);
        assert_eq!(fk, Some("build_error"));

        // A genuine test failure (the binary built, then a test failed) is
        // left untouched — it stays a real assertion failure.
        let test_fail = Outcome::fail(
            "running 1 test\ntest t ... FAILED\ntest result: FAILED. 0 passed; 1 failed"
                .to_string(),
            "code".to_string(),
        );
        let unstamped = stamp_build_error(test_fail);
        assert!(
            !unstamped.error.starts_with("build error:"),
            "{}",
            unstamped.error
        );

        // A pass is never touched.
        let ok = stamp_build_error(Outcome::pass("code".to_string()));
        assert!(ok.passed);
    }
}
