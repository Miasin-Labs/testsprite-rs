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

use super::{ExecCtx, Executor, Outcome, clip};

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
    /// Build the Playwright JS for a case. The LLM (when available) writes only
    /// the test BODY; the deterministic path uses an empty body (the harness
    /// still checks the initial navigation + page errors). Either way the body
    /// runs inside [`wrap_script`], which owns the browser lifecycle and the
    /// screenshot — so a visual artifact is captured on the LLM path too, not
    /// only the deterministic one.
    async fn script_for(&self, case: &Value, ctx: &ExecCtx) -> Result<String, String> {
        let browser = ctx.browser.as_deref().unwrap_or("chromium");
        let shot_path = shot_path(case, ctx, browser).await;
        let video_path = video_path(case, ctx, browser).await;
        // Parallel-safe test data: this test's own fresh `${uuid}`/`${ts}`/etc.
        // (and any expanded test_data_strategy templates) for planStep
        // interpolation, so concurrent frontend flows never reuse the same
        // generated account/record.
        let mut vars = ctx.variables.clone();
        crate::server::store::seed_dynamic(&mut vars);
        if let Some(body) = plan_steps_body(case, &vars, shot_path.as_deref()) {
            return Ok(wrap_script(
                &ctx.target,
                browser,
                shot_path.as_deref(),
                video_path.as_deref(),
                &body,
            ));
        }
        let body = match ctx.llm.as_ref() {
            Some(llm) => match llm.generate_playwright(case, &ctx.prd, &ctx.target).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("LLM playwright gen failed ({e}); using smoke check");
                    String::new()
                }
            },
            None => String::new(),
        };
        Ok(wrap_script(
            &ctx.target,
            browser,
            shot_path.as_deref(),
            video_path.as_deref(),
            &body,
        ))
    }
}

/// Per-case screenshot destination under `ctx.shots_dir`, or `None` when no
/// shots dir is configured or it cannot be created.
async fn shot_path(case: &Value, ctx: &ExecCtx, browser: &str) -> Option<std::path::PathBuf> {
    let dir = ctx.shots_dir.as_ref()?;
    if let Err(e) = tokio::fs::create_dir_all(dir).await {
        tracing::warn!("could not create shots dir {dir:?}: {e}");
        return None;
    }
    let id = case.get("id").and_then(Value::as_str).unwrap_or("case");
    Some(dir.join(format!("{id}-{browser}.png")))
}

/// Per-case video destination under `testsprite_tests/videos`, sibling to shots.
async fn video_path(case: &Value, ctx: &ExecCtx, browser: &str) -> Option<std::path::PathBuf> {
    let shots = ctx.shots_dir.as_ref()?;
    let root = shots.parent()?;
    let dir = root.join("videos");
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!("could not create videos dir {dir:?}: {e}");
        return None;
    }
    let id = case.get("id").and_then(Value::as_str).unwrap_or("case");
    Some(dir.join(format!("{id}-{browser}.webm")))
}

/// Compile frontend `planSteps` into deterministic Playwright statements.
///
/// This is the local equivalent of TestSprite's visible step list ("Input
/// Email", "Input Password", "Click Sign In", "Navigate to Dashboard"): store
/// the steps once, then run the same browser actions without per-run LLM code.
fn plan_steps_body(
    case: &Value,
    vars: &std::collections::HashMap<String, String>,
    shot_path: Option<&std::path::Path>,
) -> Option<String> {
    let steps = case
        .get("planSteps")
        .or_else(|| case.get("steps"))?
        .as_array()?;
    if steps.is_empty() {
        return None;
    }
    let mut out = String::from(
        r#"
async function fillFirst(selectors, value) {
  for (const s of selectors) {
    const loc = page.locator(s).first();
    if (await loc.count()) { await loc.fill(value); return; }
  }
  throw new Error('no fill target for ' + selectors.join(', '));
}
async function clickFirst(selectors) {
  for (const s of selectors) {
    const loc = page.locator(s).first();
    if (await loc.count()) { await loc.click(); return; }
  }
  throw new Error('no click target for ' + selectors.join(', '));
}
async function clickByText(text) {
  const escaped = text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const button = page.getByRole('button', { name: new RegExp(escaped, 'i') }).first();
  if (await button.count()) { await button.click(); return; }
  await page.getByText(text, { exact: false }).first().click();
}
"#,
    );
    for (idx, step) in steps.iter().enumerate() {
        out.push_str(&compile_plan_step(step, idx + 1, vars, shot_path));
    }
    Some(out)
}

fn compile_plan_step(
    step: &Value,
    idx: usize,
    vars: &std::collections::HashMap<String, String>,
    shot_path: Option<&std::path::Path>,
) -> String {
    let mut js = match step {
        Value::String(s) => compile_text_step(s, vars),
        Value::Object(o) => compile_object_step(o, vars),
        _ => format!("  console.log('skip unsupported step {idx}');\n"),
    };
    if let Some(path) = step_shot_path(shot_path, idx) {
        js.push_str(&format!(
            "  try {{ await page.screenshot({{ path: {}, fullPage: true }}); }} catch (_) {{}}\n",
            js_str(&path.display().to_string())
        ));
    }
    js
}

fn compile_text_step(s: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let lower = s.to_ascii_lowercase();
    let value = s.split_once(':').map(|(_, v)| interpolate(v.trim(), vars));
    if lower.contains("email") && (lower.contains("input") || lower.contains("enter")) {
        return fill_js(
            &[
                "input[type=email]",
                "input[name*=email i]",
                "input[placeholder*=email i]",
            ],
            value.as_deref().unwrap_or(""),
        );
    }
    if lower.contains("password") && (lower.contains("input") || lower.contains("enter")) {
        return fill_js(
            &["input[type=password]", "input[name*=password i]"],
            value.as_deref().unwrap_or(""),
        );
    }
    if let Some(label) = lower
        .strip_prefix("click ")
        .or_else(|| lower.strip_prefix("tap "))
    {
        let original = &s[s.len() - label.len()..];
        return format!("  await clickByText({});\n", js_str(original.trim()));
    }
    if lower.starts_with("navigate") || lower.starts_with("go to") || lower.starts_with("open ") {
        // Official steps read like "Navigate to /playground" — go straight to
        // the named path/URL when one is present; otherwise just settle the
        // page (a nav to a label like "Dashboard" has no deterministic target).
        if let Some(target) = s
            .split_whitespace()
            .find(|t| t.starts_with('/') || t.starts_with("http"))
        {
            return format!(
                "  await page.goto({}, {{ waitUntil: 'load', timeout: 20000 }});\n",
                js_str(&interpolate(target, vars))
            );
        }
        return "  await page.waitForLoadState('networkidle').catch(() => {});\n".to_string();
    }
    if lower.starts_with("verify ") || lower.starts_with("assert ") {
        let text = s
            .split_once(':')
            .map(|(_, v)| v)
            .or_else(|| s.split_once(' ').map(|(_, v)| v))
            .unwrap_or(s);
        return format!(
            "  await page.getByText({}, {{ exact: false }}).first().waitFor({{ timeout: 10000 }});\n",
            js_str(text.trim())
        );
    }
    format!("  console.log({});\n", js_str(&format!("manual step: {s}")))
}

fn compile_object_step(
    o: &serde_json::Map<String, Value>,
    vars: &std::collections::HashMap<String, String>,
) -> String {
    let action = o
        .get("action")
        .or_else(|| o.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let selectors = selector_list(o);
    let text = o.get("text").and_then(Value::as_str);
    let value = o
        .get("value")
        .and_then(Value::as_str)
        .map(|v| interpolate(v, vars))
        .unwrap_or_default();

    match action.as_str() {
        "fill" | "input" | "type" => {
            if selectors.is_empty() {
                fill_js(&["input:visible"], &value)
            } else {
                format!(
                    "  await fillFirst([{}], {});\n",
                    selectors
                        .iter()
                        .map(|s| js_str(s))
                        .collect::<Vec<_>>()
                        .join(", "),
                    js_str(&value)
                )
            }
        }
        "click" | "tap" => {
            if selectors.is_empty() {
                format!("  await clickByText({});\n", js_str(text.unwrap_or(&value)))
            } else {
                format!(
                    "  await clickFirst([{}]);\n",
                    selectors
                        .iter()
                        .map(|s| js_str(s))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
        "assert_text" | "expect_text" | "verify" | "assert" => format!(
            "  await page.getByText({}, {{ exact: false }}).first().waitFor({{ timeout: 10000 }});\n",
            js_str(text.unwrap_or(&value))
        ),
        "goto" | "navigate" => {
            if let Some(url) = o
                .get("url")
                .or_else(|| o.get("path"))
                .and_then(Value::as_str)
            {
                format!(
                    "  await page.goto({}, {{ waitUntil: 'load', timeout: 20000 }});\n",
                    js_str(&interpolate(url, vars))
                )
            } else {
                "  await page.waitForLoadState('networkidle').catch(() => {});\n".to_string()
            }
        }
        // The official frontend plan uses natural-language steps shaped
        // `{type:"action", description:"Navigate to /playground"}` — no
        // selector/value. When a structural action didn't match but there is a
        // `description` (or `text`), compile it as a natural-language step so
        // real official plans import and run instead of being skipped.
        _ => match o
            .get("description")
            .or_else(|| o.get("text"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(desc) => compile_text_step(desc, vars),
            None => format!("  console.log({});\n", js_str("unsupported plan step")),
        },
    }
}

fn selector_list(o: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(s) = o.get("selector").and_then(Value::as_str)
        && !s.is_empty()
    {
        out.push(s.to_string());
    }
    if let Some(arr) = o.get("selectors").and_then(Value::as_array) {
        for s in arr
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            if !out.iter().any(|existing| existing == s) {
                out.push(s.to_string());
            }
        }
    }
    out
}

fn fill_js(selectors: &[&str], value: &str) -> String {
    let selectors = selectors
        .iter()
        .map(|s| js_str(s))
        .collect::<Vec<_>>()
        .join(", ");
    format!("  await fillFirst([{selectors}], {});\n", js_str(value))
}

fn step_shot_path(shot_path: Option<&std::path::Path>, idx: usize) -> Option<std::path::PathBuf> {
    let p = shot_path?;
    let stem = p.file_stem()?.to_string_lossy();
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("png");
    Some(p.with_file_name(format!("{stem}-step{idx:02}.{ext}")))
}

fn interpolate(s: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = &after[..end];
        out.push_str(
            vars.get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
                .unwrap_or_default()
                .as_str(),
        );
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn js_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// Wrap a test `body` in a Playwright harness that OWNS the browser lifecycle
/// and the screenshot, so a visual artifact is captured deterministically on
/// every run — success or failure, LLM-authored or deterministic — instead of
/// depending on the generated script to remember `page.screenshot`.
///
/// `body` is JS statements operating on the harness's `page` (empty for the
/// deterministic smoke check). It runs inside an awaited async IIFE so generated
/// code can't skip the harness's cleanup: a stray `return` exits only the inner
/// function (not the whole run, which would leak the browser and the
/// screenshot), a `const page` shadows locally instead of colliding, and a
/// `throw` still propagates to the harness's try/catch. The screenshot runs
/// AFTER the try/catch, so a failing body still leaves an artifact.
fn wrap_script(
    url: &str,
    browser: &str,
    shot_path: Option<&std::path::Path>,
    video_path: Option<&std::path::Path>,
    body: &str,
) -> String {
    // Headless Chromium under this executor's sandboxless environment needs
    // `--no-sandbox` or `page.screenshot()` fails with a protocol error.
    let launch_args = if browser == "chromium" {
        "{ args: ['--no-sandbox', '--disable-gpu'] }"
    } else {
        "{}"
    };
    let screenshot_line = match shot_path {
        Some(p) => format!(
            "  try {{ await page.screenshot({{ path: {:?}, fullPage: true }}); }} catch (_) {{}}\n",
            p.display().to_string()
        ),
        None => String::new(),
    };
    let context_options = match video_path.and_then(|p| p.parent()) {
        Some(dir) => format!(
            "{{ recordVideo: {{ dir: {:?} }} }}",
            dir.display().to_string()
        ),
        None => "{}".to_string(),
    };
    let video_line = match video_path {
        Some(path) => format!(
            "  if (video) {{ try {{ const p = await video.path(); fs.renameSync(p, {:?}); }} catch (_) {{}} }}\n",
            path.display().to_string()
        ),
        None => String::new(),
    };
    format!(
        r#"const fs = require('fs');
const {{ {browser} }} = require('playwright');
(async () => {{
  const browser = await {browser}.launch({launch_args});
  const context = await browser.newContext({context_options});
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', e => errors.push(String(e)));
  let failed = null;
  try {{
    const resp = await page.goto({url:?}, {{ waitUntil: 'load', timeout: 20000 }});
    const status = resp ? resp.status() : 0;
    if (status >= 400) throw new Error('bad status ' + status);
    await (async () => {{
{body}
    }})();
    if (errors.length) throw new Error('page errors: ' + errors.join('; '));
  }} catch (e) {{ failed = e; }}
{screenshot_line}  const video = page.video();
  await context.close();
{video_line}  await browser.close();
  if (failed) {{ console.error(String(failed)); process.exit(1); }}
  console.log('ok');
}})().catch(e => {{ console.error(e); process.exit(1); }});
"#,
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
        return Outcome::fail(
            format!("could not create work dir: {e}"),
            script.to_string(),
        );
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
            "run",
            "--rm",
            "--network",
            "host",
            "-v",
            mount.as_str(),
            "-w",
            "/work",
            image.as_str(),
            "node",
            "/work/script.js",
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use serde_json::json;

    use super::{plan_steps_body, resolve_node_path, use_docker, wrap_script};
    use crate::testutil::env_guard;

    #[test]
    fn wrap_script_always_screenshots_when_a_path_is_given() {
        let s = wrap_script(
            "http://localhost:3000",
            "chromium",
            Some(Path::new("/tmp/shots/tc1-chromium.png")),
            Some(Path::new("/tmp/videos/tc1-chromium.webm")),
            "await page.click('#go');",
        );
        assert!(s.contains("page.screenshot("), "no screenshot in:\n{s}");
        assert!(s.contains("/tmp/shots/tc1-chromium.png"));
        assert!(s.contains("recordVideo"), "no video recording in:\n{s}");
        assert!(s.contains("/tmp/videos/tc1-chromium.webm"));
        // The generated body is embedded inside an isolating async IIFE, so a
        // stray `return`/redeclaration in it can't skip the harness cleanup.
        assert!(
            s.contains("await (async () => {"),
            "body not isolated:\n{s}"
        );
        assert!(s.contains("await page.click('#go');"));
        // The screenshot runs AFTER the try/catch, so a failing body still
        // leaves an artifact rather than skipping it.
        let shot = s.find("page.screenshot(").unwrap();
        let catch = s.find("catch (e) {").unwrap();
        assert!(shot > catch, "screenshot must run after the try/catch");
    }

    #[test]
    fn wrap_script_omits_screenshot_without_a_path() {
        let s = wrap_script("http://localhost:3000", "chromium", None, None, "");
        assert!(!s.contains("page.screenshot("));
        // The harness still guards the initial navigation.
        assert!(s.contains("bad status"));
    }

    #[test]
    fn plan_steps_compile_login_style_actions_and_step_shots() {
        let mut vars = HashMap::new();
        vars.insert("EMAIL".to_string(), "gym@example.com".to_string());
        vars.insert("PASSWORD".to_string(), "12345gym".to_string());
        let case = json!({
            "id": "login-flow",
            "planSteps": [
                "Input Email: ${EMAIL}",
                "Input Password: ${PASSWORD}",
                "Click Sign In",
                {"action":"assert_text","text":"Welcome Back"}
            ]
        });
        let body = plan_steps_body(
            &case,
            &vars,
            Some(Path::new("/tmp/shots/login-flow-chromium.png")),
        )
        .unwrap();
        assert!(body.contains("input[type=email]"), "{body}");
        assert!(body.contains("gym@example.com"), "{body}");
        assert!(body.contains("input[type=password]"), "{body}");
        assert!(body.contains("12345gym"), "{body}");
        assert!(body.contains("clickByText(\"Sign In\")"), "{body}");
        assert!(body.contains("Welcome Back"), "{body}");
        assert!(body.contains("login-flow-chromium-step01.png"), "{body}");
    }

    #[test]
    fn object_plan_steps_compile_selector_actions() {
        let case = json!({
            "planSteps": [
                {"action":"fill","selector":"#stale","selectors":["#email"],"value":"a@example.com"},
                {"action":"click","selector":"#missing","selectors":["button[type=submit]"]},
                {"action":"verify","text":"Dashboard"}
            ]
        });
        let body = plan_steps_body(&case, &HashMap::new(), None).unwrap();
        assert!(
            body.contains("fillFirst([\"#stale\", \"#email\"]"),
            "{body}"
        );
        assert!(
            body.contains("clickFirst([\"#missing\", \"button[type=submit]\"]"),
            "{body}"
        );
        assert!(body.contains("button[type=submit]"), "{body}");
        assert!(body.contains("Dashboard"), "{body}");
    }

    #[test]
    fn official_natural_language_plan_steps_compile_to_real_playwright() {
        // The exact shape of the official testsprite_frontend_test_plan.json:
        // a bare array of {type:"action", description:"<natural language>"}.
        let case = json!({
            "steps": [
                {"type":"action","description":"Navigate to /playground"},
                {"type":"action","description":"Input Email: ${EMAIL}"},
                {"type":"action","description":"Click Sign In"},
                {"type":"action","description":"Verify Welcome Back"},
            ]
        });
        let mut vars = HashMap::new();
        vars.insert("EMAIL".to_string(), "user@x.com".to_string());
        let body = plan_steps_body(&case, &vars, None).unwrap();
        // "Navigate to /playground" → a real goto, not a bare wait.
        assert!(body.contains("page.goto(\"/playground\""), "{body}");
        // "Input Email: ..." → fill the email field with the interpolated value.
        assert!(body.contains("input[type=email]"), "{body}");
        assert!(body.contains("user@x.com"), "{body}");
        // "Click Sign In" → clickByText.
        assert!(body.contains("clickByText(\"Sign In\")"), "{body}");
        // "Verify Welcome Back" → an assertion on visible text.
        assert!(body.contains("Welcome Back"), "{body}");
        // None of the official steps fell through to "unsupported".
        assert!(!body.contains("unsupported plan step"), "{body}");
    }

    #[test]
    fn plan_steps_compile_navigation_and_unsupported_steps() {
        let case = json!({
            "planSteps": [
                {"action":"goto","url":"/settings"},
                {"action":"mystery"},
                42,
                "Navigate to Dashboard"
            ]
        });
        let body = plan_steps_body(&case, &HashMap::new(), None).unwrap();
        assert!(body.contains("page.goto(\"/settings\""), "{body}");
        assert!(body.contains("unsupported plan step"), "{body}");
        assert!(body.contains("skip unsupported step 3"), "{body}");
        assert!(body.contains("waitForLoadState"), "{body}");
    }

    #[test]
    fn docker_routing_defaults_to_webkit_and_honors_force_env() {
        {
            let _guard = env_guard(&[("TESTSPRITE_BROWSER_DOCKER", None)]);
            assert!(use_docker("webkit"));
            assert!(!use_docker("chromium"));
        }
        {
            let _guard = env_guard(&[("TESTSPRITE_BROWSER_DOCKER", Some("yes"))]);
            assert!(use_docker("chromium"));
        }
        let _guard = env_guard(&[("TESTSPRITE_BROWSER_DOCKER", Some("no"))]);
        assert!(!use_docker("webkit"));
    }

    #[test]
    fn node_path_prefers_explicit_envs() {
        {
            let _guard = env_guard(&[
                ("PLAYWRIGHT_NODE_PATH", Some("/tmp/pw")),
                ("NODE_PATH", Some("/tmp/node")),
            ]);
            assert_eq!(resolve_node_path().as_deref(), Some("/tmp/pw"));
        }
        let _guard = env_guard(&[
            ("PLAYWRIGHT_NODE_PATH", None),
            ("NODE_PATH", Some("/tmp/node")),
        ]);
        assert_eq!(resolve_node_path().as_deref(), Some("/tmp/node"));
    }
}
