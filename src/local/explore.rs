//! Frontend exploratory QA: open pages, inventory controls, generate
//! deterministic `planSteps` candidates.
//!
//! This is the local, bounded version of TestSprite's "agent explores app / finds
//! what to test" loop. It crawls same-origin links up to a small depth/limit and
//! turns visible controls into runnable frontend tests the user can review/store.

use std::path::Path;

use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ExploreOpts<'a> {
    pub url: &'a str,
    pub store: bool,
    pub depth: usize,
    pub limit: usize,
    /// Opt-in stateful probing: click visible controls on a fresh page and turn
    /// observed URL/heading changes into candidate assertions. Off by default
    /// because clicks can mutate app state.
    pub interactions: bool,
}

/// Explore `url`, print/store candidate frontend tests, and return the report.
pub async fn explore(root: &Path, opts: ExploreOpts<'_>) -> anyhow::Result<Value> {
    let inventory = inventory_site(opts.url, opts.depth, opts.limit, opts.interactions).await?;
    let cases = cases_from_inventory(&inventory);
    let stored = if opts.store {
        crate::local::store::import_values(root, cases.as_array().unwrap_or(&Vec::new())).await?
    } else {
        Vec::new()
    };
    Ok(json!({
        "url": opts.url,
        "depth": opts.depth,
        "limit": opts.limit,
        "interactions": opts.interactions,
        "inventory": inventory,
        "cases": cases,
        "stored": stored,
    }))
}

async fn inventory_site(
    url: &str,
    depth: usize,
    limit: usize,
    interactions: bool,
) -> anyhow::Result<Value> {
    let script = inventory_script(url, depth.min(3), limit.clamp(1, 50), interactions);
    let out = run_node(&script).await?;
    Ok(serde_json::from_str(&out)?)
}

fn inventory_script(url: &str, depth: usize, limit: usize, interactions: bool) -> String {
    format!(
        r#"const {{ chromium }} = require('playwright');
(async () => {{
  const start = new URL({url:?});
  const maxDepth = {depth};
  const limit = {limit};
  const interactions = {interactions};
  const browser = await chromium.launch({{ args: ['--no-sandbox', '--disable-gpu'] }});
  const page = await browser.newPage();
  const seen = new Set();
  const queue = [{{ url: start.href, depth: 0 }}];
  const pages = [];

  function sameOrigin(href) {{
    try {{ const u = new URL(href, start.href); return u.origin === start.origin ? u.href.split('#')[0] : null; }} catch {{ return null; }}
  }}

  while (queue.length && pages.length < limit) {{
    const item = queue.shift();
    if (!item || seen.has(item.url)) continue;
    seen.add(item.url);
    try {{
      await page.goto(item.url, {{ waitUntil: 'load', timeout: 20000 }});
      const inv = await page.evaluate(() => {{
        const visible = el => {{
          const r = el.getBoundingClientRect();
          const s = getComputedStyle(el);
          return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none';
        }};
        const cssEsc = v => (window.CSS && CSS.escape) ? CSS.escape(v) : String(v).replace(/"/g, '\\"');
        const txt = el => (el.innerText || el.value || el.getAttribute('aria-label') || el.getAttribute('placeholder') || '').trim();
        const selector = el => {{
          if (el.id) return '#' + cssEsc(el.id);
          for (const a of ['data-testid','data-test','name','aria-label','placeholder']) {{
            const v = el.getAttribute(a);
            if (v) return `${{el.tagName.toLowerCase()}}[${{a}}="${{String(v).replace(/"/g, '\\"')}}"]`;
          }}
          return '';
        }};
        return {{
          url: location.href,
          title: document.title || '',
          headings: Array.from(document.querySelectorAll('h1,h2,h3')).filter(visible).slice(0, 12).map(txt).filter(Boolean),
          inputs: Array.from(document.querySelectorAll('input,textarea,select')).filter(visible).slice(0, 30).map(el => ({{
            tag: el.tagName.toLowerCase(), type: (el.getAttribute('type') || '').toLowerCase(),
            name: el.getAttribute('name') || '', id: el.id || '', placeholder: el.getAttribute('placeholder') || '',
            label: el.getAttribute('aria-label') || '', selector: selector(el)
          }})),
          buttons: Array.from(document.querySelectorAll('button,[role=button],input[type=submit],a[href]')).filter(visible).slice(0, 40).map(el => ({{
            tag: el.tagName.toLowerCase(), text: txt(el), href: el.href || '', selector: selector(el)
          }})).filter(x => x.text || x.href),
          links: Array.from(document.querySelectorAll('a[href]')).filter(visible).slice(0, 80).map(el => el.href).filter(Boolean),
        }};
      }});
      if (interactions) {{
        inv.probes = [];
        for (const b of (inv.buttons || []).filter(b => b.selector).slice(0, 8)) {{
          try {{
            await page.goto(item.url, {{ waitUntil: 'load', timeout: 20000 }});
            await page.locator(b.selector).first().click({{ timeout: 5000 }});
            await page.waitForLoadState('networkidle', {{ timeout: 2500 }}).catch(() => {{}});
            const after = await page.evaluate(() => ({{
              url: location.href,
              title: document.title || '',
              headings: Array.from(document.querySelectorAll('h1,h2,h3')).map(el => (el.innerText || '').trim()).filter(Boolean).slice(0, 8)
            }}));
            inv.probes.push({{ button: b, after }});
          }} catch (e) {{
            inv.probes.push({{ button: b, error: String(e) }});
          }}
        }}
      }}
      pages.push(inv);
      if (item.depth < maxDepth) {{
        for (const href of inv.links || []) {{
          const u = sameOrigin(href);
          if (u && !seen.has(u) && queue.length + pages.length < limit) queue.push({{ url: u, depth: item.depth + 1 }});
        }}
      }}
    }} catch (e) {{ pages.push({{ url: item.url, error: String(e) }}); }}
  }}
  await browser.close();
  console.log(JSON.stringify({{ start: start.href, pages }}));
}})().catch(async e => {{ console.error(String(e)); process.exit(1); }});
"#
    )
}

async fn run_node(script: &str) -> anyhow::Result<String> {
    let file = std::env::temp_dir().join(format!("tsrs_explore_{}.js", Uuid::new_v4()));
    tokio::fs::write(&file, script).await?;
    let mut cmd = tokio::process::Command::new("node");
    cmd.arg(&file);
    if let Some(node_path) = resolve_node_path() {
        cmd.env("NODE_PATH", node_path);
    }
    let out = cmd.output().await;
    let _ = tokio::fs::remove_file(&file).await;
    let out = out?;
    if !out.status.success() {
        anyhow::bail!("explore failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8(out.stdout)?)
}

fn resolve_node_path() -> Option<String> {
    if let Ok(v) = std::env::var("NODE_PATH")
        && !v.trim().is_empty()
    {
        return Some(v);
    }
    let home = std::env::var("HOME").unwrap_or_default();
    [
        format!("{home}/.npm-global/lib/node_modules"),
        "/usr/local/lib/node_modules".to_string(),
        "/usr/lib/node_modules".to_string(),
    ]
    .into_iter()
    .find(|root| std::path::Path::new(root).join("playwright").exists())
}

fn cases_from_inventory(inv: &Value) -> Value {
    let pages: Vec<&Value> = inv
        .get("pages")
        .and_then(Value::as_array)
        .map(|p| p.iter().collect())
        .unwrap_or_else(|| vec![inv]);
    let mut cases = Vec::new();
    for (page_idx, page) in pages.iter().enumerate() {
        cases.extend(cases_from_page(page, page_idx));
    }
    Value::Array(cases)
}

fn cases_from_page(inv: &Value, page_idx: usize) -> Vec<Value> {
    let mut cases = Vec::new();
    let title = inv.get("title").and_then(Value::as_str).unwrap_or("page");
    let url = inv.get("url").and_then(Value::as_str).unwrap_or("");
    let suffix = page_slug(url, page_idx);
    let heading = inv
        .get("headings")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find_map(Value::as_str));
    if let Some(h) = heading.filter(|h| !h.trim().is_empty()) {
        cases.push(json!({
            "id": format!("explore-page-smoke-{suffix}"),
            "title": format!("Page renders: {title}"),
            "kind": "frontend",
            "group": "explore",
            "planSteps": [format!("Verify: {h}")]
        }));
    }

    if looks_like_login(inv) {
        let email = find_input(inv, &["email", "user"]);
        let password = find_input(inv, &["password", "pass"]);
        let mut steps = Vec::new();
        steps.push(fill_step(email.as_ref(), "${TESTSPRITE_EMAIL}", "Email"));
        steps.push(fill_step(
            password.as_ref(),
            "${TESTSPRITE_PASSWORD}",
            "Password",
        ));
        let click = find_button(inv, &["sign in", "log in", "login", "submit"]);
        steps.push(click_step(click.as_ref(), "Sign In"));
        cases.push(json!({
            "id": format!("explore-login-flow-{suffix}"),
            "title": "Login flow discovered from visible form",
            "kind": "frontend",
            "group": "explore",
            "planSteps": steps,
        }));
    }

    for (i, b) in interesting_buttons(inv).into_iter().take(8).enumerate() {
        let text = b.text.clone();
        cases.push(json!({
            "id": format!("explore-action-{suffix}-{:02}", i + 1),
            "title": format!("Action is reachable: {text}"),
            "kind": "frontend",
            "group": "explore",
            "planSteps": [click_step(Some(&b), &text)],
        }));
    }

    if let Some(probes) = inv.get("probes").and_then(Value::as_array) {
        let before_headings: Vec<String> = inv
            .get("headings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_ascii_lowercase())
            .collect();
        for (i, probe) in probes.iter().enumerate().take(8) {
            let Some(button) = probe.get("button").and_then(control_from_button_value) else {
                continue;
            };
            let Some(after) = probe.get("after") else {
                continue;
            };
            let heading = after
                .get("headings")
                .and_then(Value::as_array)
                .and_then(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .find(|h| !before_headings.contains(&h.trim().to_ascii_lowercase()))
                        .or_else(|| a.iter().find_map(Value::as_str))
                })
                .filter(|h| !h.trim().is_empty());
            let mut steps = vec![click_step(Some(&button), &button.text)];
            if let Some(h) = heading {
                steps.push(Value::String(format!("Verify: {h}")));
            } else if let Some(url) = after.get("url").and_then(Value::as_str) {
                steps.push(Value::String(format!("Navigate to {url}")));
            }
            cases.push(json!({
                "id": format!("explore-probe-{suffix}-{:02}", i + 1),
                "title": format!("Click {} reaches an observable state", button.text),
                "kind": "frontend",
                "group": "explore",
                "planSteps": steps,
            }));
        }
    }
    cases
}

#[derive(Debug, Clone)]
struct Control {
    text: String,
    selector: Option<String>,
}

fn fill_step(input: Option<&Control>, value: &str, fallback_label: &str) -> Value {
    match input.and_then(|i| i.selector.as_deref()) {
        Some(selector) => {
            let mut selectors = vec![selector.to_string()];
            let label = fallback_label.to_ascii_lowercase();
            if label.contains("email") {
                selectors.push("input[type=email]".to_string());
                selectors.push("input[placeholder*=email i]".to_string());
            } else if label.contains("password") {
                selectors.push("input[type=password]".to_string());
                selectors.push("input[name*=password i]".to_string());
            }
            json!({"action":"fill","selector":selector,"selectors":selectors,"value":value})
        }
        None => Value::String(format!("Input {fallback_label}: {value}")),
    }
}

fn click_step(button: Option<&Control>, fallback_text: &str) -> Value {
    match button.and_then(|b| b.selector.as_deref()) {
        Some(selector) => json!({
            "action":"click",
            "selector":selector,
            "selectors":[selector, format!("text={}", button.map(|b| b.text.as_str()).unwrap_or(fallback_text))]
        }),
        None => Value::String(format!("Click {fallback_text}")),
    }
}

fn looks_like_login(inv: &Value) -> bool {
    find_input(inv, &["email", "user"]).is_some()
        && find_input(inv, &["password", "pass"]).is_some()
}

fn find_input(inv: &Value, needles: &[&str]) -> Option<Control> {
    inv.get("inputs")
        .and_then(Value::as_array)?
        .iter()
        .find_map(|i| {
            let hay = ["type", "name", "id", "placeholder", "label"]
                .into_iter()
                .filter_map(|k| i.get(k).and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase();
            if needles.iter().any(|n| hay.contains(n)) {
                Some(Control {
                    text: hay,
                    selector: i
                        .get("selector")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                })
            } else {
                None
            }
        })
}

fn find_button(inv: &Value, needles: &[&str]) -> Option<Control> {
    interesting_buttons(inv).into_iter().find(|button| {
        let lower = button.text.to_ascii_lowercase();
        needles.iter().any(|n| lower.contains(n))
    })
}

fn control_from_button_value(v: &Value) -> Option<Control> {
    let text = v.get("text").and_then(Value::as_str)?.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(Control {
        text,
        selector: v
            .get("selector")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

fn interesting_buttons(inv: &Value) -> Vec<Control> {
    inv.get("buttons")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let text = b.get("text").and_then(Value::as_str)?.trim().to_string();
            if text.is_empty() || text.len() > 60 {
                return None;
            }
            Some(Control {
                text,
                selector: b
                    .get("selector")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            })
        })
        .collect()
}

fn page_slug(url: &str, idx: usize) -> String {
    let path = url
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/').map(|(_, path)| path))
        .unwrap_or("")
        .trim_matches('/');
    let base = if path.is_empty() { "home" } else { path };
    let slug: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{idx}-{}", slug.trim_matches('-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_generates_login_and_smoke_cases() {
        let inv = json!({
            "start": "http://x/",
            "pages": [{
                "url": "http://x/",
                "title": "Gym",
                "headings": ["Welcome Back"],
                "inputs": [
                    {"type":"email","placeholder":"Email","selector":"#email"},
                    {"type":"password","placeholder":"Password","selector":"#password"}
                ],
                "buttons": [{"text":"Sign In","selector":"#sign-in"}, {"text":"View Full Program"}]
            }]
        });
        let cases = cases_from_inventory(&inv);
        let s = cases.to_string();
        assert!(s.contains("explore-page-smoke"), "{s}");
        assert!(s.contains("explore-login-flow"), "{s}");
        assert!(s.contains("TESTSPRITE_EMAIL"), "{s}");
        assert!(s.contains("#email"), "{s}");
        assert!(s.contains("selectors"), "{s}");
        assert!(s.contains("#sign-in"), "{s}");
    }

    #[test]
    fn inventory_generates_cases_for_multiple_pages() {
        let inv = json!({
            "pages": [
                {"url":"http://x/","title":"Home","headings":["Home"],"buttons":[]},
                {"url":"http://x/settings","title":"Settings","headings":["Settings"],"buttons":[{"text":"Save","selector":"#save"}]}
            ]
        });
        let cases = cases_from_inventory(&inv);
        let s = cases.to_string();
        assert!(s.contains("Settings"), "{s}");
        assert!(s.contains("#save"), "{s}");
    }

    #[test]
    fn interaction_probes_generate_click_plus_assertion_cases() {
        let inv = json!({
            "pages": [{
                "url": "http://x/",
                "title": "Home",
                "headings": ["Home"],
                "buttons": [{"text":"Open Details","selector":"#details"}],
                "probes": [{
                    "button": {"text":"Open Details","selector":"#details"},
                    "after": {"url":"http://x/","headings":["Details"]}
                }]
            }]
        });
        let cases = cases_from_inventory(&inv);
        let s = cases.to_string();
        assert!(s.contains("explore-probe"), "{s}");
        assert!(s.contains("#details"), "{s}");
        assert!(s.contains("Verify: Details"), "{s}");
    }
}
