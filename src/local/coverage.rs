//! `coverage` — real Rust coverage via `cargo llvm-cov`, plus a
//! language-agnostic STRUCTURAL surface via tree-sitter: enumerate testable
//! units (functions/methods) and count control-flow branches per file, for
//! rust/python/javascript/typescript/go. Not just an API list — function
//! locations and branch counts, so callers can reason about what a test
//! suite actually needs to exercise.

use std::path::Path;

use serde::Serialize;
use tree_sitter::{Language, Node, Parser};

/// One discovered function/method definition.
#[derive(Debug, Clone, Serialize)]
pub struct Unit {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub branches: usize,
}

#[derive(Clone, Copy)]
enum Lang {
    Rust,
    Python,
    Go,
    JavaScript,
    TypeScript,
}

impl Lang {
    fn label(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
            Lang::Go => "go",
            Lang::JavaScript => "javascript",
            Lang::TypeScript => "typescript",
        }
    }

    fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Lang::Rust),
            "py" => Some(Lang::Python),
            "go" => Some(Lang::Go),
            "js" | "mjs" => Some(Lang::JavaScript),
            "ts" | "tsx" => Some(Lang::TypeScript),
            _ => None,
        }
    }

    fn ts_language(self) -> Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        }
    }

    /// Node kinds that mark a function/method definition.
    fn function_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["function_item"],
            Lang::Python => &["function_definition"],
            Lang::Go => &["function_declaration", "method_declaration"],
            Lang::JavaScript => &["function_declaration", "method_definition"],
            Lang::TypeScript => &["function_declaration", "method_definition"],
        }
    }

    /// Node kinds counted as a control-flow branch inside a function body.
    fn branch_kinds(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &[
                "if_expression",
                "match_expression",
                "match_arm",
                "for_expression",
                "while_expression",
                "loop_expression",
                "&&",
                "||",
            ],
            Lang::Python => &[
                "if_statement",
                "elif_clause",
                "for_statement",
                "while_statement",
                "match_statement",
                "case_clause",
                "boolean_operator",
            ],
            Lang::Go => &[
                "if_statement",
                "for_statement",
                "expression_switch_statement",
                "type_switch_statement",
                "expression_case",
                "type_case",
                "communication_case",
                "&&",
                "||",
            ],
            Lang::JavaScript | Lang::TypeScript => &[
                "if_statement",
                "for_statement",
                "for_in_statement",
                "while_statement",
                "switch_case",
                "switch_default",
                "&&",
                "||",
            ],
        }
    }
}

const SKIP_DIRS: &[&str] = &["target", "node_modules", ".git", "testsprite_tests"];

/// Walk `root` for source files and enumerate function/method definitions per
/// language, counting control-flow branch nodes inside each function body.
pub fn structural_surface(root: &Path) -> anyhow::Result<Vec<Unit>> {
    let mut units = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let is_hidden = e
                .file_name()
                .to_str()
                .map(|s| s != "." && s.starts_with('.'))
                .unwrap_or(false);
            let is_skipped = e
                .file_name()
                .to_str()
                .map(|s| SKIP_DIRS.contains(&s))
                .unwrap_or(false);
            !is_hidden && !is_skipped
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(lang) = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Lang::from_extension)
        else {
            continue;
        };
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        units.extend(units_in_file(root, path, lang, &src)?);
    }
    Ok(units)
}

fn units_in_file(root: &Path, path: &Path, lang: Lang, src: &str) -> anyhow::Result<Vec<Unit>> {
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return Ok(Vec::new());
    }
    let Some(tree) = parser.parse(src, None) else {
        return Ok(Vec::new());
    };
    let bytes = src.as_bytes();
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");

    let mut units = Vec::new();
    let function_kinds = lang.function_kinds();
    let branch_kinds = lang.branch_kinds();
    visit(tree.root_node(), &mut |node| {
        if !function_kinds.contains(&node.kind()) {
            return;
        }
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .unwrap_or("<anonymous>")
            .to_string();
        let line = node.start_position().row + 1;
        let branches = count_branches(node, function_kinds, branch_kinds);
        units.push(Unit {
            name,
            file: rel.clone(),
            line,
            branches,
        });
    });
    Ok(units)
}

/// Depth-first walk of `node` and every descendant, invoking `f` on each.
fn visit<'a>(node: Node<'a>, f: &mut dyn FnMut(Node<'a>)) {
    f(node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, f);
    }
}

/// Count control-flow branch nodes inside `node`, pruning at nested-function
/// boundaries so a nested function's branches are attributed only to it.
fn count_branches(node: Node, function_kinds: &[&str], branch_kinds: &[&str]) -> usize {
    let mut count = 0;
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if function_kinds.contains(&child.kind()) {
            continue;
        }
        if branch_kinds.contains(&child.kind()) {
            count += 1;
        }
        count += count_branches(child, function_kinds, branch_kinds);
    }
    count
}

/// If `root` has a Cargo.toml, run `cargo llvm-cov --summary-only` and parse
/// the overall function% and line%. Best-effort: any failure (missing
/// Cargo.toml, missing tool, non-zero exit, unparsable output) returns
/// `Ok(None)` rather than aborting the caller.
pub fn rust_llvm_cov(root: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    if !root.join("Cargo.toml").exists() {
        return Ok(None);
    }
    let output = std::process::Command::new("cargo")
        .args(["llvm-cov", "--summary-only", "--json"])
        .current_dir(root)
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Ok(None),
    };
    let Ok(text) = String::from_utf8(output.stdout) else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Ok(None);
    };
    let totals = value
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|d| d.get("totals"));
    let Some(totals) = totals else {
        return Ok(None);
    };
    let function_pct = totals
        .get("functions")
        .and_then(|f| f.get("percent"))
        .cloned();
    let line_pct = totals.get("lines").and_then(|l| l.get("percent")).cloned();
    if function_pct.is_none() && line_pct.is_none() {
        return Ok(None);
    }
    Ok(Some(serde_json::json!({
        "functionPercent": function_pct,
        "linePercent": line_pct,
    })))
}

/// Compute the structural surface (+ Rust real coverage when applicable),
/// print a human report or one JSON object, and return the process exit
/// code (always 0 — this is a report, not a pass/fail gate).
pub async fn coverage(root: &Path, json: bool) -> anyhow::Result<i32> {
    let root = root.to_path_buf();
    let units = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || structural_surface(&root)).await??
    };
    let rust_summary = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || rust_llvm_cov(&root)).await??
    };

    let mut by_lang: std::collections::BTreeMap<&'static str, (usize, usize, usize)> =
        std::collections::BTreeMap::new();
    let mut files_seen: std::collections::HashMap<&'static str, std::collections::HashSet<&str>> =
        std::collections::HashMap::new();
    for u in &units {
        let lang = lang_label_for_file(&u.file);
        let entry = by_lang.entry(lang).or_insert((0, 0, 0));
        entry.1 += 1; // functions
        entry.2 += u.branches; // branches
        files_seen
            .entry(lang)
            .or_default()
            .insert(u.file.as_str());
    }
    for (lang, files) in &files_seen {
        by_lang.entry(lang).or_insert((0, 0, 0)).0 = files.len();
    }

    let uncovered: Vec<String> = uncovered_names(&units, rust_summary.as_ref());

    if json {
        let languages: serde_json::Map<String, serde_json::Value> = by_lang
            .iter()
            .map(|(lang, (files, functions, branches))| {
                (
                    (*lang).to_string(),
                    serde_json::json!({
                        "files": files,
                        "functions": functions,
                        "branches": branches,
                    }),
                )
            })
            .collect();
        let out = serde_json::json!({
            "languages": languages,
            "functions": units,
            "uncovered": uncovered,
            "rust": rust_summary,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(0);
    }

    println!("Structural surface (tree-sitter):");
    println!(
        "{:<12} {:>8} {:>10} {:>10}",
        "language", "files", "functions", "branches"
    );
    for (lang, (files, functions, branches)) in &by_lang {
        println!("{lang:<12} {files:>8} {functions:>10} {branches:>10}");
    }

    match &rust_summary {
        Some(s) => {
            let f = s.get("functionPercent").and_then(|v| v.as_f64());
            let l = s.get("linePercent").and_then(|v| v.as_f64());
            println!();
            println!(
                "Rust coverage (cargo llvm-cov): functions {} / lines {}",
                f.map(|v| format!("{v:.2}%")).unwrap_or_else(|| "?".into()),
                l.map(|v| format!("{v:.2}%")).unwrap_or_else(|| "?".into()),
            );
        }
        None => {
            println!();
            println!("Rust coverage (cargo llvm-cov): unavailable (no Cargo.toml or llvm-cov run failed)");
        }
    }

    println!();
    println!("Uncovered/target functions: {}", uncovered.len());
    for name in uncovered.iter().take(20) {
        println!("  {name}");
    }
    if uncovered.len() > 20 {
        println!("  ... and {} more", uncovered.len() - 20);
    }

    Ok(0)
}

fn lang_label_for_file(file: &str) -> &'static str {
    let ext = Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    Lang::from_extension(ext).map(Lang::label).unwrap_or("other")
}

/// Best-effort list of functions to treat as coverage targets: for non-Rust
/// languages there is no real coverage signal yet, so every discovered unit
/// is listed; for Rust, listing stays best-effort too (llvm-cov's per-file
/// data isn't parsed here — only the aggregate), so all Rust units are
/// surfaced as candidates whenever real coverage isn't available.
fn uncovered_names(units: &[Unit], rust_summary: Option<&serde_json::Value>) -> Vec<String> {
    units
        .iter()
        .filter(|u| {
            let lang = lang_label_for_file(&u.file);
            lang != "rust" || rust_summary.is_none()
        })
        .map(|u| format!("{}:{} {}", u.file, u.line, u.name))
        .collect()
}

/// Cross-reference the structural surface against every STORED test: a unit
/// is "covered" iff its name appears as a whole word (case-insensitive) in
/// any stored test's title, description, spec, or code. Lets a caller loop
/// "generate the uncovered ones" until this reports zero uncovered.
#[derive(Debug, Clone, Serialize)]
pub struct GapReport {
    pub total: usize,
    pub covered: usize,
    pub uncovered: Vec<Unit>,
}

/// True iff `name` (case-insensitive) occurs in `haystack` as a whole word:
/// the characters immediately before and after the match (if any) are not
/// `[a-z0-9_]`. Empty names never match.
fn mentions(haystack: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let needle = name.to_lowercase();
    let is_word_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(&needle) {
        let idx = start + rel;
        let end = idx + needle.len();
        let before_ok = haystack[..idx].chars().next_back().map(|c| !is_word_char(c)).unwrap_or(true);
        let after_ok = haystack[end..].chars().next().map(|c| !is_word_char(c)).unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

/// Compute [`GapReport`] for `scan`'s structural surface against `root`'s
/// stored tests.
pub async fn gaps(root: &Path, scan: &Path) -> anyhow::Result<GapReport> {
    let units = structural_surface(scan)?;
    let tests = crate::local::store::list(root).await?;

    let mut haystack = String::new();
    for t in &tests {
        haystack.push_str(&t.title);
        haystack.push('\n');
        haystack.push_str(&t.description);
        haystack.push('\n');
        if let Some(spec) = &t.spec
            && let Ok(s) = serde_json::to_string(spec)
        {
            haystack.push_str(&s);
            haystack.push('\n');
        }
        if let Some(code) = t.extra.get("code").and_then(|v| v.as_str()) {
            haystack.push_str(code);
            haystack.push('\n');
        }
    }
    let haystack = haystack.to_lowercase();

    let mut covered = 0usize;
    let mut uncovered = Vec::new();
    for u in units.into_iter() {
        if mentions(&haystack, &u.name) {
            covered += 1;
        } else {
            uncovered.push(u);
        }
    }

    Ok(GapReport {
        total: covered + uncovered.len(),
        covered,
        uncovered,
    })
}

/// Print [`gaps`]'s report (JSON or human) and return exit code 0.
pub async fn gaps_report(root: &Path, scan: &Path, json: bool) -> anyhow::Result<i32> {
    let report = gaps(root, scan).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }

    println!(
        "coverage gaps: {}/{} functions referenced by tests",
        report.covered, report.total
    );
    for u in report.uncovered.iter().take(60) {
        println!("  [uncovered] {}  {}:{}", u.name, u.file, u.line);
    }
    if report.uncovered.len() > 60 {
        println!("  … and {} more", report.uncovered.len() - 60);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_functions_and_counts_branches() {
        let dir = crate::local::tmp_root();

        std::fs::write(
            dir.join("lib.rs"),
            r#"
fn plain() -> i32 {
    1
}

fn branchy(x: i32) -> i32 {
    if x > 0 {
        1
    } else if x < 0 {
        -1
    } else {
        0
    }
}
"#,
        )
        .unwrap();

        std::fs::write(
            dir.join("script.py"),
            r#"
def add(a, b):
    return a + b

def classify(x):
    if x > 0:
        return "pos"
    elif x < 0:
        return "neg"
    return "zero"
"#,
        )
        .unwrap();

        let units = structural_surface(&dir).unwrap();
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert!(names.contains(&"plain"), "missing rust fn plain: {names:?}");
        assert!(
            names.contains(&"branchy"),
            "missing rust fn branchy: {names:?}"
        );
        assert!(names.contains(&"add"), "missing python fn add: {names:?}");
        assert!(
            names.contains(&"classify"),
            "missing python fn classify: {names:?}"
        );

        let branchy = units.iter().find(|u| u.name == "branchy").unwrap();
        assert!(
            branchy.branches > 0,
            "expected branchy() to have branches, got {}",
            branchy.branches
        );
        assert_eq!(branchy.file, "lib.rs");
        assert!(branchy.line > 0);

        let plain = units.iter().find(|u| u.name == "plain").unwrap();
        assert_eq!(plain.branches, 0);

        let classify = units.iter().find(|u| u.name == "classify").unwrap();
        assert!(classify.branches > 0);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rust_llvm_cov_none_without_cargo_toml() {
        let dir = crate::local::tmp_root();
        let result = rust_llvm_cov(&dir).unwrap();
        assert!(result.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
    #[tokio::test]
    async fn gaps_reports_covered_and_uncovered_units() {
        let root = crate::local::tmp_root();
        let scan = root.join("scan");
        std::fs::create_dir_all(&scan).unwrap();
        std::fs::write(
            scan.join("lib.rs"),
            r#"
fn foo() -> i32 { 1 }
fn bar() -> i32 { 2 }
"#,
        )
        .unwrap();

        crate::local::store::add_value(
            &root,
            serde_json::json!({"title": "t", "code": "assert_eq!(foo(), 1);"}),
        )
        .await
        .unwrap();

        let report = gaps(&root, &scan).await.unwrap();
        assert_eq!(report.total, 2);
        assert_eq!(report.covered, 1);
        assert_eq!(report.uncovered.len(), 1);
        assert_eq!(report.uncovered[0].name, "bar");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn mentions_matches_whole_words_only() {
        assert!(mentions("call foo() here", "foo"));
        assert!(!mentions("foobar", "foo"));
        assert!(!mentions("", "foo"));
    }

}
