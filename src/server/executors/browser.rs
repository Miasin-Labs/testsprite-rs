//! Frontend / E2E executor — TestSprite's primary surface.
//!
//! Generates a Playwright script (LLM, or a deterministic smoke template) and
//! runs it with Node. Chromium/Firefox run on the host via the locally-cached
//! browsers; webkit (and any browser when `TESTSPRITE_BROWSER_DOCKER` is set)
//! runs inside the official Playwright Docker image, which bundles every
//! browser with the system libs the host may lack. Captures pass/fail and
//! page/console errors. The target is a frontend URL.

use serde_json::Value;
use uuid::Uuid;

use super::{clip, ExecCtx, Executor, Outcome};

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
        let browser = ctx.browser.as_deref().unwrap_or("chromium");
        if use_docker(browser) {
            run_node_script_docker(&script).await
        } else {
            run_node_script(&script).await
        }
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
        let browser = ctx.browser.as_deref().unwrap_or("chromium");
        let shot_path = match &ctx.shots_dir {
            Some(dir) => match tokio::fs::create_dir_all(dir).await {
                Ok(()) => {
                    let id = case.get("id").and_then(Value::as_str).unwrap_or("case");
                    Some(dir.join(format!("{id}-{browser}.png")))
                }
                Err(e) => {
                    tracing::warn!("could not create shots dir {dir:?}: {e}");
                    None
                }
            },
            None => None,
        };
        Ok(smoke_template(&ctx.target, browser, shot_path.as_deref()))
    }
}

/// Deterministic Playwright smoke test: navigate, assert a 2xx-3xx response and
/// no uncaught page errors.
fn smoke_template(url: &str, browser: &str, shot_path: Option<&std::path::Path>) -> String {
    // Headless Chromium under this executor's sandboxless environment needs
    // `--no-sandbox` or `page.screenshot()` fails with a protocol error.
    let launch_args = if browser == "chromium" {
        "{ args: ['--no-sandbox', '--disable-gpu'] }"
    } else {
        "{}"
    };
    let screenshot_line = match shot_path {
        Some(p) => format!(
            "  await page.screenshot({{ path: {:?}, fullPage: true }});\n",
            p.display().to_string()
        ),
        None => String::new(),
    };
    format!(
        r#"const {{ {browser} }} = require('playwright');
(async () => {{
  const browser = await {browser}.launch({launch_args});
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', e => errors.push(String(e)));
  const resp = await page.goto({url:?}, {{ waitUntil: 'load', timeout: 20000 }});
  const status = resp ? resp.status() : 0;
{screenshot_line}  await browser.close();
  if (status >= 400) {{ console.error('bad status ' + status); process.exit(1); }}
  if (errors.length) {{ console.error('page errors: ' + errors.join('; ')); process.exit(1); }}
  console.log('ok ' + status);
}})().catch(e => {{ console.error(e); process.exit(1); }});
"#,
        browser = browser,
        launch_args = launch_args,
        url = url,
        screenshot_line = screenshot_line,
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
            let err = clip(&String::from_utf8_lossy(&o.stderr), 2000);
            Outcome::fail(err, script.to_string())
        }
        Err(e) => Outcome::fail(format!("node failed to launch: {e}"), script.to_string()),
    }
}

/// Should this browser run inside the Playwright Docker image instead of on the
/// host? `TESTSPRITE_BROWSER_DOCKER` forces the answer (`1/true/yes` → docker,
/// `0/false/no` → host). By default only webkit uses Docker — it needs system
/// libs the host frequently lacks, whereas chromium/firefox run fine natively.
fn use_docker(browser: &str) -> bool {
    match std::env::var("TESTSPRITE_BROWSER_DOCKER") {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => browser == "webkit",
    }
}

/// Pinned Playwright release + the official base tag we layer the npm package
/// onto. The versioned base guarantees its bundled browsers match this release,
/// so `npm i -g playwright@<ver>` resolves the browsers already present.
const PW_VERSION: &str = "1.60.0";
const PW_BASE_IMAGE: &str = "mcr.microsoft.com/playwright:v1.60.0-noble";

/// Ensure the derived Playwright image exists, building it once if missing. The
/// official base ships browsers but not the `playwright` npm package, so we layer
/// it on (NODE_PATH baked so `require('playwright')` resolves). No-op when the
/// image is present or the caller pointed at a self-managed image.
async fn ensure_playwright_image(image: &str) -> Result<(), String> {
    let present = tokio::process::Command::new("docker")
        .args(["image", "inspect", image])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if present {
        return Ok(());
    }
    let derived = format!("testsprite-rs-playwright:{PW_VERSION}");
    if image != derived.as_str() {
        return Err(format!(
            "Playwright image `{image}` not found; pull/build it or unset TESTSPRITE_PLAYWRIGHT_IMAGE to auto-build the default"
        ));
    }
    let dir = std::env::temp_dir().join(format!("tsrs_pw_build_{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("could not create build dir: {e}"))?;
    let dockerfile = format!(
        "FROM {PW_BASE_IMAGE}\nRUN npm install -g playwright@{PW_VERSION}\nENV NODE_PATH=/usr/lib/node_modules\n"
    );
    let result = build_playwright_image(&dir, &dockerfile, image).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    result
}

async fn build_playwright_image(
    dir: &std::path::Path,
    dockerfile: &str,
    image: &str,
) -> Result<(), String> {
    tokio::fs::write(dir.join("Dockerfile"), dockerfile)
        .await
        .map_err(|e| format!("could not write Dockerfile: {e}"))?;
    tracing::info!("building Playwright Docker image {image} (one-time, ~1 min)…");
    let dpath = dir.to_string_lossy().into_owned();
    let out = tokio::process::Command::new("docker")
        .args(["build", "-t", image, dpath.as_str()])
        .output()
        .await
        .map_err(|e| format!("docker build failed to launch: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "building {image} failed: {}",
            clip(&String::from_utf8_lossy(&out.stderr), 1500)
        ))
    }
}

/// Run the Playwright script inside the official Playwright Docker image, which
/// bundles Node + Playwright + every browser (incl. webkit) with all system
/// deps — so browsers the host can't launch still run. `--network host` lets the
/// container reach the target URL (localhost included, on Linux).
async fn run_node_script_docker(script: &str) -> Outcome {
    let dir = std::env::temp_dir().join(format!("ts_pw_{}", Uuid::new_v4()));
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return Outcome::fail(format!("could not create work dir: {e}"), script.to_string());
    }
    let file = dir.join("script.js");
    if let Err(e) = tokio::fs::write(&file, script).await {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Outcome::fail(format!("could not write script: {e}"), script.to_string());
    }
    let image = crate::envs::playwright_image();
    if let Err(e) = ensure_playwright_image(&image).await {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Outcome::fail(e, script.to_string());
    }
    let mount = format!("{}:/work", dir.display());
    let out = tokio::process::Command::new("docker")
        .args([
            "run", "--rm", "--network", "host", "-v", mount.as_str(), "-w", "/work",
            image.as_str(), "node", "/work/script.js",
        ])
        .output()
        .await;
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
        tracing::debug!("could not remove work dir {dir:?}: {e}");
    }
    match out {
        Ok(o) if o.status.success() => Outcome::pass(script.to_string()),
        Ok(o) => {
            let mut buf = String::from_utf8_lossy(&o.stdout).into_owned();
            buf.push_str(&String::from_utf8_lossy(&o.stderr));
            Outcome::fail(clip(&buf, 2000), script.to_string())
        }
        Err(e) => Outcome::fail(
            format!("docker failed to launch: {e} (is Docker installed and running?)"),
            script.to_string(),
        ),
    }
}

/// Where `node` should look for `require('playwright')`. Honours an explicit
/// `PLAYWRIGHT_NODE_PATH`/`NODE_PATH`, else probes common global install roots.
fn resolve_node_path() -> Option<String> {
    for var in ["PLAYWRIGHT_NODE_PATH", "NODE_PATH"] {
        if let Ok(p) = std::env::var(var)
            && !p.is_empty()
        {
            return Some(p);
        }
    }
    let home = std::env::var("HOME").ok()?;
    [
        format!("{home}/.npm-global/lib/node_modules"),
        "/usr/local/lib/node_modules".to_string(),
        "/usr/lib/node_modules".to_string(),
    ]
    .into_iter()
    .find(|root| std::path::Path::new(root).join("playwright").exists())
}
