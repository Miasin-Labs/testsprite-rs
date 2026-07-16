//! Compile-repair helpers for generated test code.
//!
//! 34–92% of first-pass LLM tests fail to compile in the literature; a compile
//! failure is a toolchain event, never a product bug, and most instances are
//! mechanically fixable. The pipeline: cheap deterministic fixups first, then
//! a bounded LLM regeneration fed the exact compiler diagnostic (in
//! [`super::rust`]), and finally an explicit `build error:` stamp so triage
//! classifies leftovers as `build_error`, not as a failing product.

/// Does this cargo output describe a COMPILE failure (vs a failing test)?
///
/// A failing test run prints `test result: FAILED` from a binary that built;
/// a compile failure never gets that far and carries rustc's own markers.
pub(crate) fn is_compile_failure(cargo_output: &str) -> bool {
    if cargo_output.contains("test result:") {
        return false;
    }
    cargo_output.contains("could not compile")
        || cargo_output.contains("error[E")
        || cargo_output.contains("error: expected")
}

/// Zero-cost cleanup of model output before it ever reaches rustc: drop
/// markdown fences and any leading prose lines before the first Rust item.
/// Conservative on purpose — when nothing looks like Rust, the input is
/// returned unchanged so the compiler (not this heuristic) reports the error.
pub(crate) fn deterministic_fixups(code: &str) -> String {
    let no_fences: Vec<&str> = code
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect();
    let first_item = no_fences.iter().position(|l| {
        let t = l.trim_start();
        [
            "use ",
            "#[",
            "#!",
            "fn ",
            "pub ",
            "mod ",
            "//",
            "/*",
            "extern ",
            "const ",
            "static ",
            "type ",
            "struct ",
            "enum ",
            "impl ",
            "macro_rules!",
        ]
        .iter()
        .any(|p| t.starts_with(p))
    });
    match first_item {
        Some(idx) => no_fences[idx..].join("\n"),
        None => code.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_failures_are_distinguished_from_failing_tests() {
        assert!(is_compile_failure(
            "error[E0308]: mismatched types\nerror: could not compile `demo`"
        ));
        assert!(is_compile_failure("error: expected `;`, found `}`"));
        // A test that BUILT and then failed is not a compile failure, even
        // when its output happens to print compiler-looking words.
        assert!(!is_compile_failure(
            "running 1 test\ntest t ... FAILED\nerror[E0308]-style text in a log\n\
             test result: FAILED. 0 passed; 1 failed"
        ));
        assert!(!is_compile_failure(
            "running 1 test\ntest t ... FAILED\ntest result: FAILED. 0 passed; 1 failed"
        ));
    }

    #[test]
    fn fixups_strip_fences_and_leading_prose_only() {
        let messy = "Here is the test you asked for:\n```rust\nuse demo::add;\n\n#[test]\nfn adds() {\n    assert_eq!(add(1, 2), 3);\n}\n```\n";
        let fixed = deterministic_fixups(messy);
        assert!(fixed.starts_with("use demo::add;"), "{fixed}");
        assert!(!fixed.contains("```"));
        assert!(!fixed.contains("Here is"));
        assert!(fixed.contains("assert_eq!(add(1, 2), 3);"));

        // Nothing Rust-like → untouched, so rustc reports the real problem.
        assert_eq!(deterministic_fixups("just prose"), "just prose");
        // Already-clean code → unchanged.
        let clean = "#[test]\nfn ok() {}";
        assert_eq!(deterministic_fixups(clean), clean);
    }
}
