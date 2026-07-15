//! Rust executor — "TestSprite for this repo".
//!
//! The target is a path to a Rust crate directory. For each case the LLM
//! generates a `#[test]` (edge/boundary/error paths) which we drop into a temp
//! file inside the crate's `tests/` dir, run with `cargo test`, then remove.
//! Deterministic fallback: a smoke test asserting the crate compiles
//! (`cargo build`).

use serde_json::Value;
use uuid::Uuid;

use super::{ExecCtx, Executor, Outcome};

pub struct RustExecutor;

#[async_trait::async_trait]
impl Executor for RustExecutor {
    fn label(&self) -> &'static str {
        "rust"
    }

    async fn run(&self, case: &Value, ctx: &ExecCtx) -> Outcome {
        let crate_dir = ctx.target.clone();
        // Agent-provided code: compile + run it directly, no LLM needed.
        if let Some(code) = case
            .get("code")
            .and_then(|v| v.as_str())
            .filter(|c| !c.trim().is_empty())
        {
            return run_cargo_test(&crate_dir, code).await;
        }

        // No LLM → smoke test: does the crate compile?
        let Some(llm) = ctx.llm.as_ref() else {
            return cargo_build(&crate_dir).await;
        };

        match llm.generate_rust_test(case, &ctx.prd, &crate_dir).await {
            Ok(code) => run_cargo_test(&crate_dir, &code).await,
            Err(e) => Outcome::fail(format!("rust test generation failed: {e}"), String::new()),
        }
    }
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
        Ok(o) => Outcome::fail(tail(&String::from_utf8_lossy(&o.stderr), 1200), code),
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
            Outcome::fail(tail(&msg, 1500), code.to_string())
        }
        Err(e) => Outcome::fail(format!("cargo failed to launch: {e}"), code.to_string()),
    }
}

/// Keep the last `n` chars (compiler/test errors are most useful at the tail).
fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.trim().to_string()
    } else {
        format!("…{}", &s[s.len() - n..])
    }
}
