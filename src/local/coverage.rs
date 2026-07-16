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
    for entry in walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
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
    }) {
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

/// Verbatim source of the given units, keyed by `(file, line)` — the focal
/// context generation prompts need (feeding the LLM only `{name,file,branches}`
/// is the dominant hallucination source in the literature). Parses each
/// distinct file once; each function body is truncated to `cap` characters on
/// a char boundary. Units whose file/line no longer match are simply absent.
pub fn function_sources(
    scan: &Path,
    units: &[Unit],
    cap: usize,
) -> std::collections::HashMap<(String, usize), String> {
    let mut by_file: std::collections::BTreeMap<&str, Vec<&Unit>> =
        std::collections::BTreeMap::new();
    for u in units {
        by_file.entry(u.file.as_str()).or_default().push(u);
    }
    let mut out = std::collections::HashMap::new();
    for (file, wanted) in by_file {
        let path = scan.join(file);
        let Some(lang) = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Lang::from_extension)
        else {
            continue;
        };
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut parser = Parser::new();
        if parser.set_language(&lang.ts_language()).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(&src, None) else {
            continue;
        };
        let bytes = src.as_bytes();
        let function_kinds = lang.function_kinds();
        visit(tree.root_node(), &mut |node| {
            if !function_kinds.contains(&node.kind()) {
                return;
            }
            let line = node.start_position().row + 1;
            let Some(unit) = wanted.iter().find(|u| u.line == line) else {
                return;
            };
            if let Ok(text) = node.utf8_text(bytes) {
                out.insert((unit.file.clone(), unit.line), truncate_chars(text, cap));
            }
        });
    }
    out
}

/// Truncate on a char boundary, marking the cut.
fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut out: String = s.chars().take(cap).collect();
    out.push_str("\n… (truncated)");
    out
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
    let file_is_tests = file_is_test_file(lang, &rel);
    visit(tree.root_node(), &mut |node| {
        if !function_kinds.contains(&node.kind()) {
            return;
        }
        let name = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(bytes).ok())
            .unwrap_or("<anonymous>")
            .to_string();
        // Test functions are not testable surface. Counting them asks the caller
        // to write tests for their tests, and inflates the denominator with code
        // that exists only to exercise the real code.
        if file_is_tests || is_test_unit(lang, node, &name, src) {
            return;
        }
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

/// Whole files that exist to test other files.
fn file_is_test_file(lang: Lang, rel: &str) -> bool {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    match lang {
        Lang::Rust => rel.starts_with("tests/") || rel.contains("/tests/"),
        Lang::Python => base.starts_with("test_") || base.ends_with("_test.py"),
        Lang::Go => base.ends_with("_test.go"),
        Lang::JavaScript | Lang::TypeScript => {
            base.contains(".test.") || base.contains(".spec.") || rel.contains("__tests__/")
        }
    }
}

/// Text of the attributes attached to `node`, whether the grammar models them as
/// children or as preceding siblings.
fn attribute_text(node: Node, src: &str) -> String {
    let mut out = String::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_item" {
            out.push_str(child.utf8_text(src.as_bytes()).unwrap_or(""));
            out.push('\n');
        }
    }
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_item" => {
                out.push_str(s.utf8_text(src.as_bytes()).unwrap_or(""));
                out.push('\n');
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sib = s.prev_sibling();
    }
    out
}

/// Is this function test code rather than testable surface?
fn is_test_unit(lang: Lang, node: Node, name: &str, src: &str) -> bool {
    match lang {
        Lang::Rust => {
            // `#[test]`, `#[tokio::test]`, `#[bench]`, ...
            if attribute_text(node, src).contains("test") {
                return true;
            }
            // Anything inside a `#[cfg(test)] mod tests { .. }`.
            let mut cur = node.parent();
            while let Some(n) = cur {
                if n.kind() == "mod_item" {
                    let mod_name = n
                        .child_by_field_name("name")
                        .and_then(|x| x.utf8_text(src.as_bytes()).ok())
                        .unwrap_or("");
                    if mod_name == "tests" || attribute_text(n, src).contains("cfg(test)") {
                        return true;
                    }
                }
                cur = n.parent();
            }
            false
        }
        Lang::Python => name.starts_with("test_"),
        Lang::Go => name.starts_with("Test") || name.starts_with("Benchmark"),
        Lang::JavaScript | Lang::TypeScript => false,
    }
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

/// Run `cargo llvm-cov --json` and return the parsed export, or `None` when it
/// cannot run at all (no Cargo.toml, tool missing, build/link failure,
/// unparsable output).
///
/// Deliberately WITHOUT `--summary-only`: that flag strips the per-function
/// array, which is the only real answer to "did this function execute?".
fn llvm_cov_export(root: &Path) -> Option<serde_json::Value> {
    if !root.join("Cargo.toml").exists() {
        return None;
    }
    let out_path = std::env::temp_dir().join(format!(
        "testsprite-rs-llvm-cov-{}.json",
        std::process::id()
    ));
    let output = std::process::Command::new("cargo")
        .args(["llvm-cov", "--json", "--output-path"])
        .arg(&out_path)
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        tracing::debug!(
            "cargo llvm-cov failed: {}",
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next_back()
                .unwrap_or("")
        );
        return None;
    }
    let text = std::fs::read_to_string(&out_path)
        .or_else(|_| String::from_utf8(output.stdout).map_err(std::io::Error::other))
        .ok()?;
    let _ = std::fs::remove_file(out_path);
    serde_json::from_str::<serde_json::Value>(&text).ok()
}

/// The bare function name from an llvm symbol like
/// `testsprite_rs::local::coverage::mentions` or a monomorphized
/// `core::ops::function::FnOnce::call_once<..>`.
fn bare_symbol_name(symbol: &str) -> &str {
    let no_generics = symbol.split_once('<').map(|(a, _)| a).unwrap_or(symbol);
    no_generics
        .rsplit("::")
        .next()
        .unwrap_or(no_generics)
        .trim()
}

fn symbol_identifiers(symbol: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = symbol.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut len = 0usize;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            len = len
                .saturating_mul(10)
                .saturating_add((bytes[i] - b'0') as usize);
            i += 1;
        }
        if len == 0 || i + len > bytes.len() {
            i = start + 1;
            continue;
        }
        if let Ok(s) = std::str::from_utf8(&bytes[i..i + len])
            && s.chars()
                .next()
                .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        {
            out.push(s.to_string());
        }
        i = start + 1;
    }
    if out.is_empty() {
        out.push(bare_symbol_name(symbol).to_string());
    }
    out
}

/// Names of functions a coverage run observed EXECUTING at least once.
///
/// `None` when llvm-cov could not produce per-function data — the caller must
/// then say so rather than silently reporting a weaker signal as if it were
/// this one.
pub fn rust_executed_functions(root: &Path) -> Option<std::collections::HashSet<String>> {
    let value = llvm_cov_export(root)?;
    let functions = value
        .get("data")
        .and_then(|d| d.get(0))
        .and_then(|d| d.get("functions"))
        .and_then(|f| f.as_array())?;
    let mut executed = std::collections::HashSet::new();
    for f in functions {
        let count = f
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if count == 0 {
            continue;
        }
        if let Some(name) = f.get("name").and_then(serde_json::Value::as_str) {
            executed.extend(symbol_identifiers(name));
        }
    }
    Some(executed)
}

/// If `root` has a Cargo.toml, run `cargo llvm-cov` and parse the overall
/// function% and line%. Best-effort: any failure (missing Cargo.toml, missing
/// tool, non-zero exit, unparsable output) returns `Ok(None)` rather than
/// aborting the caller.
pub fn rust_llvm_cov(root: &Path) -> anyhow::Result<Option<serde_json::Value>> {
    let Some(value) = llvm_cov_export(root) else {
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
        files_seen.entry(lang).or_default().insert(u.file.as_str());
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
            println!(
                "Rust coverage (cargo llvm-cov): unavailable (no Cargo.toml or llvm-cov run failed)"
            );
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
    Lang::from_extension(ext)
        .map(Lang::label)
        .unwrap_or("other")
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

/// How a unit came to be counted as covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    /// A coverage run observed the function execute. Ground truth.
    Executed,
    /// A stored test's text mentions the function's name. This proves only that
    /// someone typed the name — it is satisfied by a name-drop in a comment and
    /// it cannot see the repo's own `cargo test`/pytest suite at all.
    Named,
    /// Both signals were available and disagree across the surface.
    Mixed,
}

/// Cross-reference the structural surface against what is actually tested.
#[derive(Debug, Clone, Serialize)]
pub struct GapReport {
    pub total: usize,
    pub covered: usize,
    /// Units a coverage run observed executing.
    pub executed: usize,
    /// Units counted as covered ONLY because a stored test mentions the name.
    pub named_only: usize,
    pub uncovered: Vec<Unit>,
    /// Which signal `covered` rests on.
    pub evidence: Evidence,
    /// What the number does and does not mean, when that is not obvious.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// True iff `name` (case-insensitive) occurs in `haystack` as a whole word:
/// the characters immediately before and after the match (if any) are not
/// `[a-z0-9_]`. Empty names never match.
pub(crate) fn mentions(haystack: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let needle = name.to_lowercase();
    let is_word_char = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(&needle) {
        let idx = start + rel;
        let end = idx + needle.len();
        let before_ok = haystack[..idx]
            .chars()
            .next_back()
            .map(|c| !is_word_char(c))
            .unwrap_or(true);
        let after_ok = haystack[end..]
            .chars()
            .next()
            .map(|c| !is_word_char(c))
            .unwrap_or(true);
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
    }
    false
}

/// The lowercased text of one stored test (title + description + spec + code) —
/// the surface a coverage/diff matcher searches for unit names.
pub(crate) fn test_haystack(t: &super::LocalTest) -> String {
    let mut s = String::new();
    s.push_str(&t.title);
    s.push('\n');
    s.push_str(&t.description);
    s.push('\n');
    if let Some(spec) = &t.spec
        && let Ok(j) = serde_json::to_string(spec)
    {
        s.push_str(&j);
        s.push('\n');
    }
    if let Some(code) = t.extra.get("code").and_then(|v| v.as_str()) {
        s.push_str(code);
        s.push('\n');
    }
    s.to_lowercase()
}

/// Compute [`GapReport`] for `scan`'s structural surface.
///
/// A unit is covered when a coverage run observed it EXECUTE. Where real
/// coverage is unavailable (no Cargo.toml, llvm-cov not installed, the build
/// won't link), this falls back to matching the unit's name against the stored
/// tests' text and labels the result [`Evidence::Named`] — a much weaker claim,
/// reported as such rather than dressed up as coverage.
pub async fn gaps(root: &Path, scan: &Path) -> anyhow::Result<GapReport> {
    let units = structural_surface(scan)?;
    let tests = crate::local::store::list(root).await?;

    let mut haystack = String::new();
    for t in &tests {
        haystack.push_str(&test_haystack(t));
    }

    let executed_fns = {
        let root = root.to_path_buf();
        tokio::task::spawn_blocking(move || rust_executed_functions(&root)).await?
    };

    let mut executed = 0usize;
    let mut named_only = 0usize;
    let mut uncovered = Vec::new();
    for u in units.into_iter() {
        let ran = executed_fns.as_ref().is_some_and(|e| e.contains(&u.name));
        if ran {
            executed += 1;
        } else if mentions(&haystack, &u.name) {
            named_only += 1;
        } else {
            uncovered.push(u);
        }
    }

    let covered = executed + named_only;
    let (evidence, note) = match (&executed_fns, named_only) {
        (None, _) => (
            Evidence::Named,
            Some(
                "No execution data (cargo llvm-cov unavailable here), so `covered` means \
                 only that a stored test's text mentions the function's name — not that \
                 anything ran. Install cargo-llvm-cov for a real measurement."
                    .to_string(),
            ),
        ),
        (Some(_), 0) => (Evidence::Executed, None),
        (Some(_), n) => (
            Evidence::Mixed,
            Some(format!(
                "{n} unit(s) are counted as covered only because a stored test mentions \
                 the name; a coverage run did not observe them execute."
            )),
        ),
    };

    Ok(GapReport {
        total: covered + uncovered.len(),
        covered,
        executed,
        named_only,
        uncovered,
        evidence,
        note,
    })
}

/// Print [`gaps`]'s report (JSON or human) and return exit code 0.
pub async fn gaps_report(root: &Path, scan: &Path, json: bool) -> anyhow::Result<i32> {
    let report = gaps(root, scan).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }

    let basis = match report.evidence {
        Evidence::Executed => "observed executing",
        Evidence::Named => "referenced by a test's text (NOT executed)",
        Evidence::Mixed => "covered",
    };
    println!(
        "coverage gaps: {}/{} functions {basis}",
        report.covered, report.total
    );
    if report.evidence == Evidence::Mixed {
        println!(
            "  ({} observed executing, {} matched by name only)",
            report.executed, report.named_only
        );
    }
    if let Some(note) = &report.note {
        println!("  note: {note}");
    }
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
        // `root` has no Cargo.toml, so there is no execution data and the
        // report must say the number is only a name match.
        assert_eq!(report.evidence, Evidence::Named);
        assert_eq!(report.named_only, 1);
        assert_eq!(report.executed, 0);
        assert!(
            report
                .note
                .as_deref()
                .unwrap()
                .contains("not that anything ran")
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn test_functions_are_not_testable_surface() {
        // Regression: every `#[test]` fn was counted as a unit needing
        // coverage, so the tool asked you to write tests for your tests and
        // inflated the denominator with test code.
        let dir = crate::local::tmp_root();
        std::fs::write(
            dir.join("lib.rs"),
            r#"
fn real_work() -> i32 { 1 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_work_returns_one() {
        assert_eq!(real_work(), 1);
    }

    #[tokio::test]
    async fn also_a_test() {
        assert!(true);
    }

    fn helper_inside_the_test_module() -> i32 { 2 }
}
"#,
        )
        .unwrap();

        let units = structural_surface(&dir).unwrap();
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, vec!["real_work"], "only real surface counts");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_files_are_excluded_wholesale() {
        let dir = crate::local::tmp_root();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/integration.rs"), "fn helper() -> i32 { 1 }").unwrap();
        std::fs::write(dir.join("test_thing.py"), "def helper():\n    return 1\n").unwrap();
        std::fs::write(
            dir.join("app.py"),
            "def test_like_name():\n    return 1\ndef real():\n    return 2\n",
        )
        .unwrap();

        let units = structural_surface(&dir).unwrap();
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bare_symbol_name_strips_paths_and_generics() {
        assert_eq!(
            bare_symbol_name("testsprite_rs::local::coverage::mentions"),
            "mentions"
        );
        assert_eq!(
            bare_symbol_name("foo::bar::call_once<i32, ()>"),
            "call_once"
        );
        assert_eq!(bare_symbol_name("plain"), "plain");
    }

    #[test]
    fn rust_v0_mangled_symbol_exposes_function_identifiers() {
        let ids = symbol_identifiers(
            "_RNCNvNtNtNtCsynWAeEb10j_13testsprite_rs5local8coverage5testss_35finds_functions_and_counts_branches0B9_",
        );
        assert!(ids.contains(&"testsprite_rs".to_string()), "{ids:?}");
        assert!(ids.contains(&"coverage".to_string()), "{ids:?}");
        assert!(
            ids.contains(&"finds_functions_and_counts_branches".to_string()),
            "{ids:?}"
        );
    }

    #[test]
    fn mentions_matches_whole_words_only() {
        assert!(mentions("call foo() here", "foo"));
        assert!(!mentions("foobar", "foo"));
        assert!(!mentions("", "foo"));
    }

    #[test]
    fn function_sources_returns_verbatim_bodies_truncated_on_a_boundary() {
        let dir = crate::local::tmp_root();
        std::fs::write(
            dir.join("lib.rs"),
            "fn small() -> i32 { 42 }\n\nfn big() -> String {\n    let s = \"xxxxxxxxxxxxxxxx\";\n    s.repeat(100)\n}\n",
        )
        .unwrap();
        let units = structural_surface(&dir).unwrap();
        let small = units.iter().find(|u| u.name == "small").unwrap();
        let big = units.iter().find(|u| u.name == "big").unwrap();

        let src = function_sources(&dir, &units, 40);
        let small_src = &src[&(small.file.clone(), small.line)];
        assert!(
            small_src.contains("fn small() -> i32 { 42 }"),
            "{small_src}"
        );
        assert!(!small_src.contains("truncated"));

        // `big`'s body exceeds the 40-char cap → truncated on a char boundary.
        let big_src = &src[&(big.file.clone(), big.line)];
        assert!(big_src.contains("… (truncated)"), "{big_src}");
        assert!(big_src.starts_with("fn big()"), "{big_src}");

        std::fs::remove_dir_all(dir).ok();
    }
}
