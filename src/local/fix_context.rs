//! Ground a `--fix` proposal in the repository's actual source.
//!
//! `propose_fix` otherwise sees only the failing case, its test code, and the
//! error text, so any diff it emits has invented paths and line numbers. When
//! the failure output points at real files in the repo — a Rust panic
//! `src/x.rs:12:5`, a Python traceback `File "app.py", line 4` — we read a
//! line-numbered window of that source and feed it to the model, then verify the
//! returned patch with `git apply --check`. A patch that actually applies can be
//! labelled as such; anything else stays an illustrative sketch.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MAX_FILES: usize = 3;
const CONTEXT_LINES: usize = 30;
const MAX_SNIPPET_BYTES: usize = 8 * 1024;

/// File+line references parsed out of failure output, de-duplicated by path and
/// each read as a labelled, line-numbered source window. Returns `None` when
/// nothing in the repo is referenced (e.g. a black-box HTTP failure), so the
/// caller keeps the honest "illustrative sketch" path.
pub fn source_context(root: &Path, error: &str) -> Option<String> {
    let mut out = String::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for (rel, line) in extract_refs(error) {
        if seen.len() >= MAX_FILES || out.len() >= MAX_SNIPPET_BYTES {
            break;
        }
        let Some(abs) = safe_join(root, &rel) else {
            continue;
        };
        if seen.contains(&abs) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&abs) else {
            continue;
        };
        seen.insert(abs);
        out.push_str(&format!(
            "// FILE: {rel} (failure referenced line {line})\n"
        ));
        out.push_str(&window(&text, line));
        out.push_str("\n\n");
    }
    (!out.is_empty()).then_some(out)
}

/// True iff `patch` applies cleanly to the working tree at `root`
/// (`git apply --check`, patch on stdin). Best-effort: any git error → false, so
/// an unproven patch is never labelled appliable.
pub fn patch_applies(root: &Path, patch: &str) -> bool {
    if patch.trim().is_empty() {
        return false;
    }
    let Ok(mut child) = Command::new("git")
        .args(["apply", "--check", "-"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(patch.as_bytes()).is_err()
    {
        return false;
    }
    // `stdin` (when taken) has dropped here, signalling EOF so git can finish.
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// Resolve `rel` against `root` and confirm it stays inside the repo — rejecting
/// a crafted `../../etc/passwd` reference from the (untrusted) error text.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let candidate = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    };
    let canon = candidate.canonicalize().ok()?;
    let root_canon = root.canonicalize().ok()?;
    canon.starts_with(&root_canon).then_some(canon)
}

/// A line-numbered window of `±CONTEXT_LINES` around `line` (1-based).
fn window(text: &str, line: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return String::new();
    }
    let idx = line.saturating_sub(1).min(lines.len() - 1);
    let start = idx.saturating_sub(CONTEXT_LINES);
    let end = (idx + CONTEXT_LINES + 1).min(lines.len());
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>5}  {}", start + i + 1, l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Pull `(path, line)` references out of free-text failure output: `<path>.<ext>:
/// <line>` (Rust/JS/TS/Go panics and compiler notes) and Python's
/// `File "<path>", line <N>`.
fn extract_refs(error: &str) -> Vec<(String, usize)> {
    let mut refs = Vec::new();

    for ext in [".rs", ".py", ".ts", ".tsx", ".js", ".go"] {
        let needle = format!("{ext}:");
        let mut from = 0;
        while let Some(rel) = error[from..].find(&needle) {
            let ext_dot = from + rel;
            let colon = ext_dot + ext.len();
            let start = path_start(error, ext_dot);
            let digits_start = colon + 1;
            let digits_end = digit_end(error, digits_start);
            if digits_end > digits_start {
                let path = &error[start..colon];
                if !path.is_empty()
                    && path.len() <= 256
                    && !path.contains(char::is_whitespace)
                    && let Ok(line) = error[digits_start..digits_end].parse::<usize>()
                {
                    refs.push((path.to_string(), line));
                }
            }
            from = colon + 1;
        }
    }

    let marker = "File \"";
    let mut from = 0;
    while let Some(rel) = error[from..].find(marker) {
        let pstart = from + rel + marker.len();
        let Some(qrel) = error[pstart..].find('"') else {
            break;
        };
        let path = &error[pstart..pstart + qrel];
        let after = pstart + qrel;
        if let Some(lrel) = error[after..].find("line ") {
            let ls = after + lrel + "line ".len();
            let le = digit_end(error, ls);
            if le > ls
                && let Ok(line) = error[ls..le].parse::<usize>()
            {
                refs.push((path.to_string(), line));
            }
        }
        from = after + 1;
    }

    refs
}

/// Walk back from the extension dot over path-legal bytes to the path's start.
fn path_start(s: &str, ext_dot: usize) -> usize {
    let b = s.as_bytes();
    let mut start = ext_dot;
    while start > 0 {
        let c = b[start - 1];
        if c.is_ascii_alphanumeric() || matches!(c, b'/' | b'.' | b'_' | b'-') {
            start -= 1;
        } else {
            break;
        }
    }
    start
}

/// Index just past the run of ASCII digits beginning at `from`.
fn digit_end(s: &str, from: usize) -> usize {
    let b = s.as_bytes();
    let mut end = from;
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_and_python_refs() {
        let err = "thread 'x' panicked at src/api.rs:12:5\n  \
                   File \"app/main.py\", line 7, in handler";
        let refs = extract_refs(err);
        assert!(refs.iter().any(|(p, l)| p == "src/api.rs" && *l == 12));
        assert!(refs.iter().any(|(p, l)| p == "app/main.py" && *l == 7));
    }

    #[test]
    fn no_refs_yields_no_context() {
        let root = crate::local::tmp_root();
        assert!(source_context(&root, "expected a 2xx/3xx response, got 500").is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reads_the_referenced_window_and_rejects_traversal() {
        let root = crate::local::tmp_root();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/api.rs"),
            "fn a() {}\nfn b() {}\nfn target() { panic!() }\nfn d() {}\n",
        )
        .unwrap();

        let ctx = source_context(&root, "panicked at src/api.rs:3:1").expect("some context");
        assert!(ctx.contains("fn target"), "{ctx}");
        assert!(ctx.contains("src/api.rs"), "{ctx}");

        // A crafted traversal reference is refused (never read outside the repo).
        assert!(source_context(&root, "at ../../../../etc/hosts.rs:1").is_none());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn patch_applies_only_when_it_matches_the_tree() {
        let root = crate::local::tmp_root();
        // `git apply` wants a work tree; give it a throwaway repo.
        let _ = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status();
        std::fs::write(root.join("f.txt"), "one\ntwo\nthree\n").unwrap();

        let good = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        assert!(patch_applies(&root, good), "a matching patch must apply");

        let bad = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n xxx\n-yyy\n+ZZZ\n zzz\n";
        assert!(!patch_applies(&root, bad), "a non-matching patch must not");
        assert!(!patch_applies(&root, ""), "an empty patch is not appliable");

        std::fs::remove_dir_all(&root).ok();
    }
}
