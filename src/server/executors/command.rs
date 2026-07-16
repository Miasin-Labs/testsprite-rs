//! Command executor — run the repo's OWN test command (e.g. `cargo test -p foo`)
//! deterministically: pass on exit 0. The agent hands over a shell command as the
//! case's `code`; we run it in the project root (ctx.root), no LLM.

use serde_json::Value;

use super::{ExecCtx, Executor, Outcome, lead_with_first_error};

pub struct CommandExecutor;

#[async_trait::async_trait]
impl Executor for CommandExecutor {
    fn label(&self) -> &'static str {
        "command"
    }

    async fn run(&self, case: &Value, ctx: &ExecCtx) -> Outcome {
        let Some(cmd) = case
            .get("code")
            .and_then(|v| v.as_str())
            .filter(|c| !c.trim().is_empty())
        else {
            return Outcome::fail(
                "command test needs a `code` field with the shell command to run",
                String::new(),
            );
        };

        let mut process = tokio::process::Command::new("sh");
        process.arg("-c").arg(cmd).current_dir(&ctx.root);
        // `.testsprite.env` / variables.json are loaded into ExecCtx.variables
        // for spec interpolation; command tests need the same environment so
        // repo helper scripts can mint OAuth tokens or read test credentials.
        // Parallel-safe test data: fresh per-invocation tokens + expanded
        // test_data_strategy templates so a wrapped Playwright/pytest run can
        // mint a UNIQUE user/record and never collide with a concurrent run.
        let mut env: std::collections::HashMap<String, String> = ctx.variables.clone();
        crate::server::store::seed_dynamic(&mut env);
        for (k, v) in &env {
            // Skip the internal template stash; expose only concrete values.
            if k.starts_with("__tsdata_tpl__") {
                continue;
            }
            process.env(k, v);
            // Also expose the dynamic tokens uppercased for shell ergonomics.
            if matches!(k.as_str(), "uuid" | "uuid8" | "ts" | "rand") {
                process.env(format!("TESTSPRITE_{}", k.to_uppercase()), v);
            }
        }
        let out = process.output().await;

        match out {
            Ok(o) if o.status.success() => Outcome::pass(cmd.to_string()),
            Ok(o) => {
                let mut buf = String::from_utf8_lossy(&o.stdout).into_owned();
                buf.push_str(&String::from_utf8_lossy(&o.stderr));
                Outcome::fail(
                    format!(
                        "command failed (exit {}): {}",
                        o.status.code().unwrap_or(-1),
                        lead_with_first_error(&buf, 2000)
                    ),
                    cmd.to_string(),
                )
            }
            Err(e) => Outcome::fail(format!("could not launch command: {e}"), cmd.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn ctx() -> ExecCtx {
        ExecCtx {
            target: String::new(),
            llm: None,
            prd: Arc::new(serde_json::json!({})),
            browser: None,
            shots_dir: None,
            root: std::env::current_dir().unwrap_or_default(),
            variables: std::collections::HashMap::new(),
        }
    }

    #[tokio::test]
    async fn passes_on_exit_zero() {
        let case = serde_json::json!({ "code": "true" });
        let outcome = CommandExecutor.run(&case, &ctx()).await;
        assert!(outcome.passed, "{}", outcome.error);
    }

    #[tokio::test]
    async fn injects_testsprite_variables_into_command_env() {
        let mut ctx = ctx();
        ctx.variables
            .insert("TESTSPRITE_COMMAND_SECRET".to_string(), "ok".to_string());
        let case = serde_json::json!({ "code": "test \"$TESTSPRITE_COMMAND_SECRET\" = ok" });
        let outcome = CommandExecutor.run(&case, &ctx).await;
        assert!(outcome.passed, "{}", outcome.error);
    }

    #[tokio::test]
    async fn fails_on_nonzero_exit() {
        let case = serde_json::json!({ "code": "false" });
        let outcome = CommandExecutor.run(&case, &ctx()).await;
        assert!(!outcome.passed);
        assert!(outcome.error.contains("exit"));
    }

    #[tokio::test]
    async fn fails_without_code() {
        let case = serde_json::json!({});
        let outcome = CommandExecutor.run(&case, &ctx()).await;
        assert!(!outcome.passed);
        assert!(outcome.error.contains("needs a `code`"));
    }
}
