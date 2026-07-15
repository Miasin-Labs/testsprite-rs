//! Backend HTTP executor. Runs either a deterministic HTTP assertion (when the
//! case embeds an endpoint `spec`) or LLM-generated Python (`requests`).

use serde_json::{Value, from_value};

use super::{ExecCtx, Executor, Outcome};
use crate::server::engine::EndpointSpec;
use crate::server::store;

pub struct HttpExecutor;

#[async_trait::async_trait]
impl Executor for HttpExecutor {
    fn label(&self) -> &'static str {
        "http"
    }

    async fn run(&self, case: &Value, ctx: &ExecCtx) -> Outcome {
        // Deterministic: the case embeds an executable endpoint spec.
        if let Some(spec) = case
            .get("spec")
            .and_then(|s| from_value::<EndpointSpec>(s.clone()).ok())
        {
            let (ok, err, code) = store::execute_spec(&spec, &ctx.target).await;
            return Outcome {
                passed: ok,
                error: err,
                code,
            };
        }
        // Agent-provided code: run it directly, no LLM needed.
        if let Some(code) = case
            .get("code")
            .and_then(|v| v.as_str())
            .filter(|c| !c.trim().is_empty())
        {
            let (ok, err, c) = store::execute_python(code).await;
            return Outcome {
                passed: ok,
                error: err,
                code: c,
            };
        }

        // LLM: generate Python for the case, then run it via python3.
        let Some(llm) = ctx.llm.as_ref() else {
            return Outcome::fail("no spec and no LLM available", String::new());
        };
        match llm.generate_test_code(case, &ctx.prd, &ctx.target).await {
            Ok(code) => {
                let (ok, err, code) = store::execute_python(&code).await;
                Outcome {
                    passed: ok,
                    error: err,
                    code,
                }
            }
            Err(e) => Outcome::fail(format!("code generation failed: {e}"), String::new()),
        }
    }
}
