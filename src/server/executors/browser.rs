//! Frontend / E2E executor — TestSprite's primary surface.
//!
//! Generates a Playwright script (LLM, or a deterministic smoke template) and
//! runs it with Node using the locally-cached Chromium. Captures pass/fail and
//! page/console errors. The target is a frontend URL.

use serde_json::Value;
use uuid::Uuid;

use super::{ExecCtx, Executor, Outcome};

pub struct BrowserExecutor;

#[async_trait::async_trait]
impl Executor for BrowserExecutor {
    fn label(&self) -> &'static str {
        "browser"
    }

    async fn run(&self, case: &Value, ctx: &ExecCtx) -> Outcome {
        let script = match self.script_for(case, ctx).await {
            Ok(s) => s,
            Err(e) => return Outcome::fail(e, String::new()),
        };
        run_node_script(&script).await
    }
}

impl BrowserExecutor {
    /// Build the Playwright JS for a case: LLM-authored when available, else a
    /// deterministic smoke test (load the page, fail on console/page errors).
    async fn script_for(&self, case: &Value, ctx: &ExecCtx) -> Result<String, String> {
        if let Some(llm) = ctx.llm.as_ref() {
            match llm.generate_playwright(case, &ctx.prd, &ctx.target).await {
                Ok(s) => return Ok(s),
                Err(e) => tracing::warn!("LLM playwright gen failed ({e}); using smoke template"),
            }
        }
        Ok(smoke_template(&ctx.target))
    }
}

/// Deterministic Playwright smoke test: navigate, assert a 2xx-3xx response and
/// no uncaught page errors.
fn smoke_template(url: &str) -> String {
    format!(
        r#"const {{ chromium }} = require('playwright');
(async () => {{
  const browser = await chromium.launch();
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', e => errors.push(String(e)));
  const resp = await page.goto({url:?}, {{ waitUntil: 'load', timeout: 20000 }});
  const status = resp ? resp.status() : 0;
  await browser.close();
  if (status >= 400) {{ console.error('bad status ' + status); process.exit(1); }}
  if (errors.length) {{ console.error('page errors: ' + errors.join('; ')); process.exit(1); }}
  console.log('ok ' + status);
}})().catch(e => {{ console.error(e); process.exit(1); }});
"#,
        url = url
    )
}

/// Write the script to a temp file and run it with Node. Playwright resolves
/// the cached Chromium automatically via PLAYWRIGHT_BROWSERS_PATH / its default.
///
/// `node` resolves `require('playwright')` via `NODE_PATH`: if the caller set it
/// we honour it; otherwise we auto-detect a global install (handles the common
/// case where Playwright is only installed in a global `node_modules`).
async fn run_node_script(script: &str) -> Outcome {
    let dir = std::env::temp_dir();
    let file = dir.join(format!("ts_pw_{}.js", Uuid::new_v4()));
    if let Err(e) = tokio::fs::write(&file, script).await {
        return Outcome::fail(format!("could not write script: {e}"), script.to_string());
    }
    let mut cmd = tokio::process::Command::new("node");
    cmd.arg(&file);
    if let Some(node_path) = resolve_node_path() {
        cmd.env("NODE_PATH", node_path);
    }
    let out = cmd.output().await;
    if let Err(e) = tokio::fs::remove_file(&file).await {
        tracing::debug!("could not remove temp script {file:?}: {e}");
    }
    match out {
        Ok(o) if o.status.success() => Outcome::pass(script.to_string()),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Outcome::fail(err, script.to_string())
        }
        Err(e) => Outcome::fail(format!("node failed to launch: {e}"), script.to_string()),
    }
}

/// Where `node` should look for `require('playwright')`. Honours an explicit
/// `PLAYWRIGHT_NODE_PATH`/`NODE_PATH`, else probes common global install roots.
fn resolve_node_path() -> Option<String> {
    if let Ok(p) = std::env::var("PLAYWRIGHT_NODE_PATH") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    if let Ok(p) = std::env::var("NODE_PATH") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").ok()?;
    for root in [
        format!("{home}/.npm-global/lib/node_modules"),
        "/usr/local/lib/node_modules".to_string(),
        "/usr/lib/node_modules".to_string(),
    ] {
        if std::path::Path::new(&root).join("playwright").exists() {
            return Some(root);
        }
    }
    None
}
