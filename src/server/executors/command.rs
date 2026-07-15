//! Command executor — run the repo's OWN test command (e.g. `cargo test -p foo`)
//! deterministically: pass on exit 0. The agent hands over a shell command as the
//! case's `code`; we run it in the project root (ctx.root), no LLM.

use serde_json::Value;

use super::{ExecCtx, Executor, Outcome};

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

        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&ctx.root)
            .output()
            .await;

        match out {
            Ok(o) if o.status.success() => Outcome::pass(cmd.to_string()),
            Ok(o) => {
                let mut buf = String::from_utf8_lossy(&o.stdout).into_owned();
                buf.push_str(&String::from_utf8_lossy(&o.stderr));
                let tail: String = buf.chars().rev().take(1500).collect::<Vec<_>>().into_iter().rev().collect();
                Outcome::fail(
                    format!("command failed (exit {}): {}", o.status.code().unwrap_or(-1), tail.trim()),
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
        }
    }

    #[tokio::test]
    async fn passes_on_exit_zero() {
        let case = serde_json::json!({ "code": "true" });
        let outcome = CommandExecutor.run(&case, &ctx()).await;
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
