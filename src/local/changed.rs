//! Code Diff Mode — scope test selection and generation to what changed in git.
//!
//! `git diff <since>` gives changed files and (via `--unified=0`) the exact new
//! line ranges. Each changed line is attributed to its enclosing function from
//! the structural surface (the greatest-start-line unit at or above it), so a
//! one-line edit flags one function, not the whole file. Untracked source files
//! count fully (every function is new). A stored test is "affected" iff its text
//! mentions a changed unit's name — reusing the same whole-word matcher the
//! Coverage Guard uses. This is TestSprite's "test only what you just changed"
//! pre-merge loop, done locally with git + the structural surface.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use anyhow::Context;
use serde::Serialize;

use super::coverage::{mentions, structural_surface, test_haystack, Unit};

/// The changed surface computed from a git diff.
#[derive(Debug, Clone, Serialize)]
pub struct ChangedSurface {
    pub since: String,
    /// Changed source files (root-relative, forward-slashed).
    pub files: Vec<String>,
    /// Functions whose body a changed line touched.
    pub units: Vec<Unit>,
    /// Files with changes outside any function (top-level: imports, consts,
    /// types) or that the surface can't parse (e.g. `.toml`, `.md`).
    pub file_level: Vec<String>,
}

/// Parse per-file changed NEW-side line numbers from `git diff --unified=0`.
fn parse_diff_lines(diff: &str) -> BTreeMap<String, Vec<usize>> {
    let mut out: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut cur: Option<String> = None;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let p = rest.trim();
            cur = if p == "/dev/null" {
                None
            } else {
                Some(p.strip_prefix("b/").unwrap_or(p).replace('\\', "/"))
            };
        } else if let Some(rest) = line.strip_prefix("@@ ")
            && let Some(file) = &cur
            && let Some((start, count)) = parse_new_hunk(rest)
        {
            let entry = out.entry(file.clone()).or_default();
            if count == 0 {
                entry.push(start); // pure deletion: attribute to the surrounding line
            } else {
                for l in start..start + count {
                    entry.push(l);
                }
            }
        }
    }
    out
}

/// Parse the new-side `+c,d` (or `+c`) out of a hunk header body `-a,b +c,d @@ …`.
fn parse_new_hunk(body: &str) -> Option<(usize, usize)> {
    let plus = body.split_whitespace().find(|t| t.starts_with('+'))?;
    let mut it = plus.trim_start_matches('+').splitn(2, ',');
    let start: usize = it.next()?.parse().ok()?;
    let count: usize = it.next().map(|c| c.parse().unwrap_or(1)).unwrap_or(1);
    Some((start.max(1), count))
}

/// Attribute changed lines to enclosing functions. Returns (touched units,
/// whether any changed line fell outside every unit = top-level change).
fn attribute(units_in_file: &[&Unit], changed_lines: &[usize]) -> (Vec<Unit>, bool) {
    let mut sorted: Vec<&Unit> = units_in_file.to_vec();
    sorted.sort_by_key(|u| u.line);
    let mut hit = BTreeSet::new();
    let mut top_level = false;
    for &l in changed_lines {
        match sorted.iter().rposition(|u| u.line <= l) {
            Some(i) => {
                hit.insert(i);
            }
            None => top_level = true,
        }
    }
    (hit.into_iter().map(|i| sorted[i].clone()).collect(), top_level)
}

/// Run `git ls-files --others --exclude-standard` at `root` for untracked files.
fn untracked_files(root: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--others", "--exclude-standard"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().replace('\\', "/"))
        .filter(|l| !l.is_empty())
        .collect()
}

/// Run `git diff <since>` at `root` and intersect with the structural surface.
pub fn changed_surface(root: &Path, since: &str) -> anyhow::Result<ChangedSurface> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        // `--no-ext-diff` + `--no-pager` bypass a user's external diff driver
        // (difftastic/delta) and pager so we get git's plain unified diff;
        // `diff.noprefix=false` guarantees the `a/`,`b/` path prefixes we parse.
        .args([
            "--no-pager",
            "-c",
            "diff.noprefix=false",
            "diff",
            "--no-ext-diff",
            "--relative",
            "--unified=0",
            "--no-color",
            since,
            "--",
        ])
        .output()
        .context("running `git diff` (is git installed and is this a repo?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "git diff {since} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let per_file = parse_diff_lines(&String::from_utf8_lossy(&out.stdout));

    let surface = structural_surface(root)?;
    let mut units: Vec<Unit> = Vec::new();
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut file_level: BTreeSet<String> = BTreeSet::new();

    for (file, lines) in &per_file {
        files.insert(file.clone());
        let in_file: Vec<&Unit> = surface.iter().filter(|u| &u.file == file).collect();
        if in_file.is_empty() {
            file_level.insert(file.clone());
            continue;
        }
        let (touched, top) = attribute(&in_file, lines);
        units.extend(touched);
        if top {
            file_level.insert(file.clone());
        }
    }

    // Untracked (newly added) source files: every function in them is new.
    for f in untracked_files(root) {
        let file_units: Vec<Unit> = surface.iter().filter(|u| u.file == f).cloned().collect();
        if file_units.is_empty() {
            continue; // untracked non-source / unparsed — not a testable surface change
        }
        files.insert(f);
        units.extend(file_units);
    }

    units.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    units.dedup_by(|a, b| a.file == b.file && a.name == b.name && a.line == b.line);

    Ok(ChangedSurface {
        since: since.to_string(),
        files: files.into_iter().collect(),
        units,
        file_level: file_level.into_iter().collect(),
    })
}

/// Stored test ids whose text mentions any changed unit, sorted and deduped.
pub async fn affected_test_ids(root: &Path, changed: &ChangedSurface) -> anyhow::Result<Vec<String>> {
    let names: Vec<&str> = changed.units.iter().map(|u| u.name.as_str()).collect();
    let tests = super::store::list(root).await?;
    let mut ids = Vec::new();
    for t in &tests {
        let hay = test_haystack(t);
        if names.iter().any(|n| mentions(&hay, n)) {
            ids.push(t.id.clone());
        }
    }
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Changed units not mentioned by any stored test — the `generate --changed`
/// targets (don't regenerate tests for functions already covered).
pub async fn uncovered_changed_units(root: &Path, changed: &ChangedSurface) -> anyhow::Result<Vec<Unit>> {
    let tests = super::store::list(root).await?;
    let hays: Vec<String> = tests.iter().map(test_haystack).collect();
    Ok(changed
        .units
        .iter()
        .filter(|u| !hays.iter().any(|h| mentions(h, &u.name)))
        .cloned()
        .collect())
}

/// Print the changed surface + affected test ids (JSON or human). Exit 0.
pub async fn changed_report(root: &Path, since: &str, json: bool) -> anyhow::Result<i32> {
    let changed = changed_surface(root, since)?;
    let ids = affected_test_ids(root, &changed).await?;
    if json {
        let v = serde_json::json!({
            "since": changed.since,
            "changedFiles": changed.files,
            "changedUnits": changed.units,
            "fileLevel": changed.file_level,
            "affectedTestIds": ids,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "changed since {since}: {} file(s), {} function(s)",
            changed.files.len(),
            changed.units.len()
        );
        for u in &changed.units {
            println!("  ~ {}  ({}:{})", u.name, u.file, u.line);
        }
        if !changed.file_level.is_empty() {
            println!("  top-level/unparsed changes in: {}", changed.file_level.join(", "));
        }
        println!(
            "affected tests: {}",
            if ids.is_empty() {
                "(none)".to_string()
            } else {
                ids.join(", ")
            }
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(name: &str, line: usize) -> Unit {
        Unit {
            name: name.into(),
            file: "f.rs".into(),
            line,
            branches: 0,
        }
    }

    #[test]
    fn parses_new_side_hunk_lines() {
        let diff = "diff --git a/src/foo.rs b/src/foo.rs\n\
--- a/src/foo.rs\n\
+++ b/src/foo.rs\n\
@@ -10,0 +11,3 @@ fn a()\n\
+x\n+y\n+z\n\
@@ -20,2 +24,0 @@ fn b()\n";
        let m = parse_diff_lines(diff);
        assert_eq!(m["src/foo.rs"], vec![11, 12, 13, 24]);
    }

    #[test]
    fn new_hunk_defaults_count_to_one() {
        assert_eq!(parse_new_hunk("-5,0 +6 @@"), Some((6, 1)));
        assert_eq!(parse_new_hunk("-5,2 +6,4 @@ ctx"), Some((6, 4)));
        assert_eq!(parse_new_hunk("-1 +0,0 @@"), Some((1, 0)));
    }

    #[test]
    fn attributes_lines_to_enclosing_function() {
        let a = u("alpha", 5);
        let b = u("beta", 20);
        let units = vec![&a, &b];
        let (hit, top) = attribute(&units, &[8, 25, 2]);
        let names: BTreeSet<&str> = hit.iter().map(|u| u.name.as_str()).collect();
        assert!(names.contains("alpha")); // line 8 is inside alpha
        assert!(names.contains("beta")); // line 25 is inside beta
        assert!(top); // line 2 is above the first function
    }

    #[test]
    fn deletion_only_hunk_attributes_to_surrounding_unit() {
        let a = u("alpha", 5);
        let units = vec![&a];
        let (hit, _) = attribute(&units, &[9]);
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "alpha");
    }
}
