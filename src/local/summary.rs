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

fn detect_stack(root: &Path, files: &[PathBuf]) -> Vec<Value> {
    let mut stack = BTreeSet::new();
    for f in files {
        match f.to_string_lossy().as_ref() {
            "Cargo.toml" => {
                stack.insert("rust");
                if file_contains(root.join(f), "axum") {
                    stack.insert("axum");
                }
                if file_contains(root.join(f), "sqlx") {
                    stack.insert("sqlx");
                }
                if file_contains(root.join(f), "tokio") {
                    stack.insert("tokio");
                }
            }
            "package.json" => {
                stack.insert("node");
                if file_contains(root.join(f), "react") {
                    stack.insert("react");
                }
                if file_contains(root.join(f), "next") {
                    stack.insert("nextjs");
                }
                if file_contains(root.join(f), "playwright") {
                    stack.insert("playwright");
                }
            }
            "pyproject.toml" => {
                stack.insert("python");
            }
            "go.mod" => {
                stack.insert("go");
            }
            _ => match f.extension().and_then(|e| e.to_str()) {
                Some("rs") => {
                    stack.insert("rust");
                }
                Some("ts" | "tsx") => {
                    stack.insert("typescript");
                }
                Some("js" | "jsx") => {
                    stack.insert("javascript");
                }
                Some("py") => {
                    stack.insert("python");
                }
                Some("go") => {
                    stack.insert("go");
                }
                _ => {}
            },
        }
    }
    stack
        .into_iter()
        .map(|name| json!({"name": name}))
        .collect()
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
        .map(|(name, files)| json!({"name": name, "files": files}))
        .collect()
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
        ("function post", "POST"),
        ("function put", "PUT"),
        ("function get", "GET"),
        ("const delete", "DELETE"),
        ("const patch", "PATCH"),
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

fn file_contains(path: PathBuf, needle: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|s| s.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_detects_stack_features_and_endpoints() {
        let root = crate::local::tmp_root();
        std::fs::write(
            root.join("Cargo.toml"),
            "[dependencies]\naxum=\"1\"\ntokio=\"1\"",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src/routes")).unwrap();
        std::fs::write(
            root.join("src/routes/users.rs"),
            "router.route(\"/users\", post(create_user));\nrouter.route(\"/health\", get(health));",
        )
        .unwrap();

        let summary = generate(&root).unwrap();
        assert!(summary["tech_stack"].to_string().contains("rust"));
        assert!(summary["tech_stack"].to_string().contains("axum"));
        assert!(summary["features"].to_string().contains("users"));
        assert!(summary["api_endpoints"].to_string().contains("/users"));
        assert!(summary["api_endpoints"].to_string().contains("POST"));

        std::fs::remove_dir_all(&root).ok();
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
    }
}
