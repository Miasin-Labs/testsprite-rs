//! Local code-summary generation — the first TestSprite MCP step.
//!
//! The official flow asks the IDE agent to scan the repo and write
//! `code_summary.yaml` (tech stack, features/files, endpoints). This local
//! implementation does the boring deterministic pass itself so PRD/plan
//! generation has real project context without requiring the host agent to
//! invent it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Value, json};

const SKIP_DIRS: &[&str] = &[
    ".git",
    ".omx",
    "target",
    "node_modules",
    "dist",
    "build",
    "testsprite_tests",
];

/// Generate a code-summary JSON object for `root`.
pub fn generate(root: &Path) -> anyhow::Result<Value> {
    let files = collect_files(root);
    let tech_stack = detect_stack(root, &files);
    let api_endpoints = detect_endpoints(root, &files);
    let features = group_features(&files);
    let file_rows: Vec<Value> = files
        .iter()
        .map(|p| json!({"path": p.to_string_lossy().replace('\\', "/"), "kind": file_kind(p)}))
        .collect();
    let name = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();
    Ok(json!({
        "project_name": name,
        "tech_stack": tech_stack,
        "features": features,
        "files": file_rows,
        "api_endpoints": api_endpoints,
    }))
}

/// Write the summary to `out` or the official `testsprite_tests/tmp/code_summary.yaml`.
pub fn write(root: &Path, out: Option<&Path>) -> anyhow::Result<(PathBuf, Value)> {
    let summary = generate(root)?;
    let path = out
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::paths::Paths::new(root).code_summary());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let body = match path.extension().and_then(|e| e.to_str()) {
        Some("json") => serde_json::to_string_pretty(&summary)?,
        _ => serde_yaml::to_string(&summary)?,
    };
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok((path, summary))
}

fn collect_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_str().unwrap_or("");
        !SKIP_DIRS.contains(&name) && !(name.starts_with('.') && name != ".env.example")
    }) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_path_buf();
        if interesting_file(&rel) {
            out.push(rel);
        }
    }
    out.sort();
    out
}

fn interesting_file(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if matches!(
        s.as_ref(),
        "Cargo.toml" | "package.json" | "pyproject.toml" | "go.mod"
    ) {
        return true;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "json" | "yaml" | "yml")
    )
}

/// Detect the tech stack as flat, human-readable strings — the shape real
/// TestSprite emits (`["TypeScript", "Next.js 14", "Tailwind CSS 4"]`). Base
/// languages come from file extensions; framework entries carry the declared
/// version pulled from `package.json` / `Cargo.toml` when available.
fn detect_stack(root: &Path, files: &[PathBuf]) -> Vec<String> {
    let mut stack: BTreeSet<String> = BTreeSet::new();
    for f in files {
        match f.extension().and_then(|e| e.to_str()) {
            Some("rs") => {
                stack.insert("Rust".into());
            }
            Some("ts" | "tsx") => {
                stack.insert("TypeScript".into());
            }
            Some("js" | "jsx") => {
                stack.insert("JavaScript".into());
            }
            Some("py") => {
                stack.insert("Python".into());
            }
            Some("go") => {
                stack.insert("Go".into());
            }
            _ => {}
        }
    }
    for f in files {
        match f.to_string_lossy().as_ref() {
            "package.json" => collect_node_stack(&root.join(f), &mut stack),
            "Cargo.toml" => collect_cargo_stack(&root.join(f), &mut stack),
            "pyproject.toml" => {
                stack.insert("Python".into());
            }
            "go.mod" => {
                stack.insert("Go".into());
            }
            _ => {}
        }
    }
    stack.into_iter().collect()
}

/// npm dependency name -> display label. Only frameworks worth surfacing in a
/// stack summary; utility packages are ignored.
const NODE_FRAMEWORKS: &[(&str, &str)] = &[
    ("next", "Next.js"),
    ("react", "React"),
    ("vue", "Vue"),
    ("svelte", "Svelte"),
    ("@angular/core", "Angular"),
    ("express", "Express"),
    ("@nestjs/core", "NestJS"),
    ("tailwindcss", "Tailwind CSS"),
    ("three", "Three.js"),
    ("vite", "Vite"),
    ("@playwright/test", "Playwright"),
    ("playwright", "Playwright"),
    ("typescript", "TypeScript"),
];

fn collect_node_stack(path: &Path, stack: &mut BTreeSet<String>) {
    stack.insert("Node.js".into());
    let Ok(body) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(pkg) = serde_json::from_str::<Value>(&body) else {
        return;
    };
    for section in ["dependencies", "devDependencies"] {
        let Some(deps) = pkg.get(section).and_then(Value::as_object) else {
            continue;
        };
        for (name, label) in NODE_FRAMEWORKS {
            if let Some(ver) = deps.get(*name).and_then(Value::as_str) {
                stack.insert(with_version(label, ver));
            }
        }
    }
}

/// Cargo crate name -> display label.
const CARGO_CRATES: &[(&str, &str)] = &[
    ("axum", "Axum"),
    ("actix-web", "Actix Web"),
    ("rocket", "Rocket"),
    ("warp", "Warp"),
    ("tokio", "Tokio"),
    ("sqlx", "SQLx"),
    ("diesel", "Diesel"),
    ("serde", "Serde"),
    ("reqwest", "reqwest"),
];

fn collect_cargo_stack(path: &Path, stack: &mut BTreeSet<String>) {
    stack.insert("Rust".into());
    let Ok(body) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(manifest) = toml::from_str::<toml::Value>(&body) else {
        return;
    };
    let Some(deps) = manifest.get("dependencies").and_then(|v| v.as_table()) else {
        return;
    };
    for (name, label) in CARGO_CRATES {
        let Some(dep) = deps.get(*name) else { continue };
        // A dependency value is either a version string or a table with a
        // `version` field (`{ version = "1", features = [...] }`).
        let ver = dep
            .as_str()
            .or_else(|| dep.get("version").and_then(|v| v.as_str()));
        match ver {
            Some(v) => stack.insert(with_version(label, v)),
            None => stack.insert((*label).to_string()),
        };
    }
}

/// Format `"Label major"` from a semver-ish requirement, dropping range
/// operators and pre-release/patch noise (`"^14.2.1"` -> `"Next.js 14"`). A
/// version we can't parse a leading number from yields just the label.
fn with_version(label: &str, req: &str) -> String {
    let digits: String = req
        .trim_start_matches(['^', '~', '>', '=', '<', ' ', 'v'])
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        label.to_string()
    } else {
        format!("{label} {digits}")
    }
}

fn group_features(files: &[PathBuf]) -> Vec<Value> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for f in files {
        let rel = f.to_string_lossy().replace('\\', "/");
        let feature = feature_name(&rel);
        groups.entry(feature).or_default().push(rel);
    }
    groups
        .into_iter()
        .map(|(name, files)| {
            json!({
                "name": name,
                "description": feature_description(&name, &files),
                "files": files,
            })
        })
        .collect()
}

/// A short, honest description for a deterministically-grouped feature: what it
/// is and how big. Matches real code_summary's `{name, description, files}`
/// shape without pretending to semantic knowledge we don't have.
fn feature_description(name: &str, files: &[String]) -> String {
    let n = files.len();
    let unit = if n == 1 { "file" } else { "files" };
    format!("{name} module spanning {n} {unit}.")
}

fn feature_name(rel: &str) -> String {
    let parts: Vec<&str> = rel.split('/').collect();
    let skip = [
        "src",
        "crates",
        "app",
        "pages",
        "routes",
        "components",
        "lib",
    ];
    parts
        .iter()
        .copied()
        .find(|p| !skip.contains(p) && !p.contains('.'))
        .unwrap_or_else(|| {
            rel.rsplit('/')
                .next()
                .unwrap_or(rel)
                .split('.')
                .next()
                .unwrap_or(rel)
        })
        .replace(['_', '-'], " ")
}

fn detect_endpoints(root: &Path, files: &[PathBuf]) -> Vec<Value> {
    let mut endpoints = Vec::new();
    let mut seen = BTreeSet::new();
    for f in files {
        let rel = f.to_string_lossy().replace('\\', "/");
        let ext = f.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go") {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(root.join(f)) else {
            continue;
        };
        for (idx, line) in src.lines().enumerate() {
            for hit in endpoint_hits(line, &rel) {
                let key = format!("{} {}", hit.method, hit.path);
                if !seen.insert(key) {
                    continue;
                }
                endpoints.push(json!({
                    "method": hit.method,
                    "path": hit.path,
                    "file": rel,
                    "line": idx + 1,
                }));
            }
        }
    }
    endpoints
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EndpointHit {
    method: &'static str,
    path: String,
}

fn endpoint_hits(line: &str, rel: &str) -> Vec<EndpointHit> {
    let mut hits = Vec::new();
    if let Some(path) = file_route_path(rel)
        && let Some(method) = exported_http_method(line)
    {
        hits.push(EndpointHit { method, path });
    }

    for path in quoted_paths(line) {
        hits.push(EndpointHit {
            method: method_hint(line),
            path,
        });
    }
    hits
}

fn quoted_paths(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for quote in ['"', '\''] {
        let mut rest = line;
        while let Some(start) = rest.find(quote) {
            let after = &rest[start + quote.len_utf8()..];
            let Some(end) = after.find(quote) else { break };
            let s = &after[..end];
            if is_route_path(s) {
                out.push(s.to_string());
            }
            rest = &after[end + quote.len_utf8()..];
        }
    }
    out
}

fn is_route_path(s: &str) -> bool {
    (s == "/" || (s.starts_with('/') && s.len() > 1))
        && !s.starts_with("//")
        && !s.chars().any(char::is_whitespace)
}

fn exported_http_method(line: &str) -> Option<&'static str> {
    let l = line.to_ascii_lowercase();
    for (needle, method) in [
        ("function delete", "DELETE"),
        ("function patch", "PATCH"),
        ("function query", "QUERY"),
        ("function post", "POST"),
        ("function put", "PUT"),
        ("function get", "GET"),
        ("const delete", "DELETE"),
        ("const patch", "PATCH"),
        ("const query", "QUERY"),
        ("const post", "POST"),
        ("const put", "PUT"),
        ("const get", "GET"),
    ] {
        if l.contains(needle) {
            return Some(method);
        }
    }
    None
}

fn file_route_path(rel: &str) -> Option<String> {
    if let Some(rest) = rel.strip_prefix("app/api/")
        && (rest.ends_with("/route.ts") || rest.ends_with("/route.js"))
    {
        return Some(format!(
            "/api/{}",
            rest.trim_end_matches("/route.ts")
                .trim_end_matches("/route.js")
        ));
    }
    if let Some(rest) = rel.strip_prefix("pages/api/") {
        let path = rest
            .trim_end_matches(".ts")
            .trim_end_matches(".tsx")
            .trim_end_matches(".js")
            .trim_end_matches(".jsx")
            .trim_end_matches("/index");
        return Some(format!("/api/{path}"));
    }
    None
}

fn method_hint(line: &str) -> &'static str {
    let l = line.to_ascii_lowercase();
    for (needle, method) in [
        ("delete", "DELETE"),
        ("patch", "PATCH"),
        ("query", "QUERY"),
        ("post", "POST"),
        ("put", "PUT"),
        ("get", "GET"),
    ] {
        if l.contains(needle) {
            return method;
        }
    }
    "GET"
}

fn file_kind(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go") => "source",
        Some("json" | "yaml" | "yml" | "toml") => "config",
        _ => "file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_detects_stack_features_and_endpoints() {
        let root = crate::local::tmp_root();
        std::fs::write(
            root.join("Cargo.toml"),
            "[dependencies]\naxum=\"1.2\"\ntokio={version=\"1\",features=[\"full\"]}",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src/routes")).unwrap();
        std::fs::write(
            root.join("src/routes/users.rs"),
            "router.route(\"/users\", post(create_user));\nrouter.route(\"/health\", get(health));",
        )
        .unwrap();

        let summary = generate(&root).unwrap();
        // Flat, versioned tech strings — the real code_summary shape. Versions
        // are pulled from Cargo.toml, including the `{version=...}` table form.
        let stack = summary["tech_stack"].as_array().unwrap();
        let stack: Vec<&str> = stack.iter().filter_map(Value::as_str).collect();
        assert!(stack.contains(&"Rust"));
        assert!(stack.contains(&"Axum 1"));
        assert!(stack.contains(&"Tokio 1"));
        // Features carry a name + description + files.
        assert!(summary["features"].to_string().contains("users"));
        assert!(summary["features"][0].get("description").is_some());
        assert!(summary["api_endpoints"].to_string().contains("/users"));
        assert!(summary["api_endpoints"].to_string().contains("POST"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn node_stack_pulls_framework_versions_from_package_json() {
        let root = crate::local::tmp_root();
        std::fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"next":"^14.2.1","react":"18.2.0","tailwindcss":"~4.0.0"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("app.tsx"), "export const x = 1;").unwrap();

        let summary = generate(&root).unwrap();
        let stack: Vec<String> = summary["tech_stack"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert!(stack.contains(&"TypeScript".to_string()));
        assert!(stack.contains(&"Node.js".to_string()));
        assert!(stack.contains(&"Next.js 14".to_string()));
        assert!(stack.contains(&"React 18".to_string()));
        assert!(stack.contains(&"Tailwind CSS 4".to_string()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn with_version_strips_range_operators() {
        assert_eq!(with_version("Next.js", "^14.2.1"), "Next.js 14");
        assert_eq!(with_version("React", "18.2.0"), "React 18");
        assert_eq!(with_version("Tailwind CSS", "~4.0"), "Tailwind CSS 4");
        assert_eq!(with_version("Vite", "latest"), "Vite");
    }

    #[test]
    fn endpoint_hits_cover_common_framework_route_shapes() {
        assert_eq!(
            endpoint_hits(
                "router.route(\"/users\", post(create_user));",
                "src/main.rs"
            ),
            vec![EndpointHit {
                method: "POST",
                path: "/users".to_string()
            }]
        );
        assert_eq!(
            endpoint_hits("app.get('/api/health', handler)", "server.js"),
            vec![EndpointHit {
                method: "GET",
                path: "/api/health".to_string()
            }]
        );
        assert_eq!(
            endpoint_hits("@app.patch('/items/{id}')", "main.py"),
            vec![EndpointHit {
                method: "PATCH",
                path: "/items/{id}".to_string()
            }]
        );
        assert_eq!(
            endpoint_hits("export async function GET() {}", "app/api/users/route.ts"),
            vec![EndpointHit {
                method: "GET",
                path: "/api/users".to_string()
            }]
        );
        assert_eq!(
            endpoint_hits("export const POST = async () => {}", "pages/api/login.ts"),
            vec![EndpointHit {
                method: "POST",
                path: "/api/login".to_string()
            }]
        );
        assert_eq!(
            endpoint_hits(
                "export async function QUERY() {}",
                "app/api/search/route.ts"
            ),
            vec![EndpointHit {
                method: "QUERY",
                path: "/api/search".to_string()
            }]
        );
        assert_eq!(
            endpoint_hits("app.query('/search', handler)", "server.js"),
            vec![EndpointHit {
                method: "QUERY",
                path: "/search".to_string()
            }]
        );
    }
}
