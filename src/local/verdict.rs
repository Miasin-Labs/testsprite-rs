//! Pure, deterministic classification of a completed `Outcome` into the V3
//! `(verdict, failureKind)` pair. Driven by `passed`, the executor's free-text
//! `error` string, and the modality that produced it — no I/O, no network, no
//! LLM.
//!
//! The modality matters because `error` means different things per executor.
//! For reqwest/browser/MCP it is the harness reporting its OWN failure, so
//! "connection refused" really does mean the target is unreachable. For `rust`
//! and `command` it is a subprocess's captured stdout+stderr, where the same
//! substring is just something the test printed. Without that gate, a cargo
//! integration test that logs "connection refused" while failing a genuine
//! assertion gets excused as an environment problem — and `flaky` then drops it
//! from the denominator, so a real, every-time bug reads "inconclusive".

use crate::server::executors::TestKind;

/// Outcome verdict: `passed | failed | blocked` (distinct from lifecycle
/// status). Mirrors the real TestSprite V3 CLI's `CliLatestResult.verdict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed,
    Blocked,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Passed => "passed",
            Verdict::Failed => "failed",
            Verdict::Blocked => "blocked",
        }
    }

    /// Parse a verdict previously written by [`Verdict::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "passed" => Some(Verdict::Passed),
            "failed" => Some(Verdict::Failed),
            "blocked" => Some(Verdict::Blocked),
            _ => None,
        }
    }
}

/// Every `failureKind` [`classify`] can return, plus the kinds written
/// explicitly by pipeline stages rather than derived from error text:
/// `suspect_oracle` (a freshly generated test failed against the current
/// green baseline — the oracle, not the product, is the suspect),
/// `build_error` (the generated test never compiled; a toolchain diagnostic,
/// never a product bug), and `residual_alignment` (a changed-code test that
/// passes on the OLD revision while failing on the new one it supposedly
/// verifies — it encodes stale semantics, not a regression).
///
/// Re-borrowing a stored kind as `&'static str` means it has to be one of
/// these — an unrecognized value is dropped rather than leaked onward as if
/// it were meaningful.
pub const FAILURE_KINDS: &[&str] = &[
    "dependency",
    "infra",
    "auth",
    "network",
    "network_timeout",
    "browser_crash",
    "routing_404",
    "assertion",
    "timeout",
    "suspect_oracle",
    "build_error",
    "residual_alignment",
    "unknown",
];

/// Resolve a stored failure-kind string to its `&'static str` form.
pub fn known_failure_kind(s: &str) -> Option<&'static str> {
    FAILURE_KINDS.iter().copied().find(|k| *k == s)
}

/// True when this executor's `error` field is the harness's own words, so
/// transport/browser keywords in it describe OUR failure to reach the target.
///
/// False for the executors that splice a subprocess's captured output into
/// `error` (`rust`, `command`): there, the same keywords are the test's own
/// printout and say nothing about our environment.
fn error_is_harness_transport(kind: TestKind) -> bool {
    match kind {
        TestKind::Backend | TestKind::Frontend | TestKind::Mcp => true,
        TestKind::Rust | TestKind::Command => false,
    }
}

/// Errors the harness itself writes when it cannot get a test to the starting
/// line. These are ours regardless of modality — no subprocess produced them.
fn is_infra(lower: &str) -> bool {
    [
        "could not write test file",
        "could not create tests/",
        "python3 failed to launch",
        "cargo failed to launch",
        "could not launch command",
        "command test needs a `code`",
        "no spec and no llm",
        "code generation failed",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// Classify a completed outcome into the V3 `(verdict, failureKind)` pair.
/// `failureKind` is `None` when passed. First matching rule wins.
///
/// `kind` is the executor that produced `error`; see the module docs for why
/// the free-text rules cannot be applied without it.
pub fn classify(passed: bool, error: &str, kind: TestKind) -> (Verdict, Option<&'static str>) {
    if passed {
        return (Verdict::Passed, None);
    }

    let lower = error.to_lowercase();

    // Dependency-wave skip: a test not run because an upstream producer failed.
    // Written by the runner, so it applies to every modality.
    if lower.starts_with("skipped: dependency") {
        return (Verdict::Blocked, Some("dependency"));
    }

    if is_infra(&lower) {
        return (Verdict::Blocked, Some("infra"));
    }

    // Pipeline-authored prefixes (ours, not the target's, so they precede the
    // modality gate): the acceptance gate, the compile-repair loop, and the
    // cross-version fault-check each stamp their own kind.
    if lower.starts_with("suspect oracle:") {
        return (Verdict::Failed, Some("suspect_oracle"));
    }
    if lower.starts_with("build error:") {
        // The generated test never compiled — a toolchain problem, never a
        // product bug; Blocked keeps it out of the flaky denominator.
        return (Verdict::Blocked, Some("build_error"));
    }
    if lower.starts_with("residual alignment:") {
        return (Verdict::Failed, Some("residual_alignment"));
    }

    // The harness's own wall-clock killed the test — a hang/deadlock, not a
    // subprocess message, so it is a real failure for every modality.
    if lower.contains("exceeded the") && lower.contains("time limit") {
        return (Verdict::Failed, Some("timeout"));
    }

    // Everything below reads the target's response or our transport's
    // complaint. For `rust`/`command` the error is a subprocess's output, so
    // none of it applies: a failing cargo test is a failing cargo test, and
    // excusing it as "network" would drop a real bug out of the flaky
    // denominator entirely.
    if !error_is_harness_transport(kind) {
        return (Verdict::Failed, Some("assertion"));
    }

    // The target rejected our credentials. Not a product bug and not flakiness:
    // the run never got far enough to prove anything, so it is Blocked and
    // `flaky` excludes it from the stability denominator.
    //
    // Match the harness's own phrasing ("expected …, got 401") and the standard
    // HTTP status line ("401 Unauthorized"), NOT a bare "unauthorized"/
    // "forbidden": for a Backend-Python test `error` is the subprocess's output,
    // where those words routinely appear in an echoed response body. Excusing
    // that as an auth wall would drop a genuine every-time failure out of the
    // flaky denominator — the exact env-excusing bug the modality gate prevents.
    if lower.contains("got 401")
        || lower.contains("got 403")
        || lower.contains("401 unauthorized")
        || lower.contains("403 forbidden")
    {
        return (Verdict::Blocked, Some("auth"));
    }

    // Target unreachable (connection refused / DNS / no route) — from the
    // deterministic reqwest path ("error sending request") or LLM-generated
    // Python (urllib3 "Failed to establish a new connection" / "Max retries").
    // It's an env problem (the app isn't up), not a code bug — hence Blocked.
    if lower.contains("connection refused")
        || lower.contains("failed to establish a new connection")
        || lower.contains("max retries exceeded")
        || lower.contains("name or service not known")
        || lower.contains("error sending request")
    {
        return (Verdict::Blocked, Some("network"));
    }

    if kind == TestKind::Frontend {
        let mentions_browser = ["webkit", "chromium", "firefox", "playwright", "browser"]
            .iter()
            .any(|k| lower.contains(k));
        let mentions_crash = [
            "missing",
            "launch",
            "executable",
            "crash",
            "shared librar",
            ".so",
        ]
        .iter()
        .any(|k| lower.contains(k));
        if mentions_browser && mentions_crash {
            return (Verdict::Blocked, Some("browser_crash"));
        }
    }

    if lower.starts_with("request failed") {
        if lower.contains("timed out") || lower.contains("timeout") || lower.contains("deadline") {
            return (Verdict::Failed, Some("network_timeout"));
        }
        return (Verdict::Blocked, Some("network"));
    }

    if lower.contains("got 404") || lower.contains(" 404") {
        return (Verdict::Failed, Some("routing_404"));
    }

    if lower.contains("expected ") && lower.contains("got ") {
        return (Verdict::Failed, Some("assertion"));
    }

    if lower.contains("assertionerror") || lower.contains("assert") {
        return (Verdict::Failed, Some("assertion"));
    }

    if lower.contains("timed out") || lower.contains("timeout") || lower.contains("deadline") {
        return (Verdict::Failed, Some("timeout"));
    }

    (Verdict::Failed, Some("unknown"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passed_is_passed_none() {
        assert_eq!(
            classify(true, "", TestKind::Backend),
            (Verdict::Passed, None)
        );
    }

    #[test]
    fn routing_404() {
        assert_eq!(
            classify(
                false,
                "expected a 2xx/3xx response, got 404",
                TestKind::Backend
            ),
            (Verdict::Failed, Some("routing_404"))
        );
    }

    #[test]
    fn connection_refused_is_blocked_network() {
        assert_eq!(
            classify(
                false,
                "request failed: error sending request: connection refused",
                TestKind::Backend
            ),
            (Verdict::Blocked, Some("network"))
        );
    }

    #[test]
    fn timed_out_request_is_network_timeout() {
        assert_eq!(
            classify(
                false,
                "request failed: operation timed out",
                TestKind::Backend
            ),
            (Verdict::Failed, Some("network_timeout"))
        );
    }

    #[test]
    fn python_assertion_error() {
        assert_eq!(
            classify(false, "AssertionError: 1 != 2", TestKind::Backend),
            (Verdict::Failed, Some("assertion"))
        );
    }

    #[test]
    fn python_launch_failure_is_infra() {
        assert_eq!(
            classify(
                false,
                "python3 failed to launch: No such file",
                TestKind::Backend
            ),
            (Verdict::Blocked, Some("infra"))
        );
    }

    #[test]
    fn webkit_missing_shared_lib_is_browser_crash() {
        assert_eq!(
            classify(
                false,
                "webkit: missing shared library libicudata.so.74",
                TestKind::Frontend
            ),
            (Verdict::Blocked, Some("browser_crash"))
        );
    }

    #[test]
    fn dependency_skip_is_blocked() {
        assert_eq!(
            classify(
                false,
                "skipped: dependency 'auth_token' unmet (upstream producer failed)",
                TestKind::Backend
            ),
            (Verdict::Blocked, Some("dependency"))
        );
    }

    #[test]
    fn a_subprocess_printing_transport_noise_is_still_a_real_failure() {
        // Regression: the network rule ran before the assertion rule with no
        // modality gate, so ANY cargo/shell output containing "connection
        // refused" — extremely common in integration-test logs — relabelled a
        // genuine assertion failure as Blocked/network. `flaky` then dropped
        // the run from its denominator, so a test that fails every single time
        // read "inconclusive" instead of "failing".
        let noisy = "running 1 test\n\
                     test api::connects ... FAILED\n\
                     thread 'api::connects' panicked at src/api.rs:12:5:\n\
                     assertion `left == right` failed\n\
                       left: 500\n\
                      right: 200\n\
                     warning: connection refused while probing replica";
        for kind in [TestKind::Rust, TestKind::Command] {
            assert_eq!(
                classify(false, noisy, kind),
                (Verdict::Failed, Some("assertion")),
                "{kind:?} must not excuse a subprocess failure as a network problem"
            );
        }
        // The same text from our own reqwest transport genuinely does mean the
        // target is unreachable.
        assert_eq!(
            classify(false, noisy, TestKind::Backend),
            (Verdict::Blocked, Some("network"))
        );
    }

    #[test]
    fn harness_authored_infra_errors_survive_the_modality_gate() {
        // These are the harness's own words even for subprocess modalities.
        assert_eq!(
            classify(
                false,
                "cargo failed to launch: No such file",
                TestKind::Rust
            ),
            (Verdict::Blocked, Some("infra"))
        );
        assert_eq!(
            classify(
                false,
                "could not write test file: permission denied",
                TestKind::Rust
            ),
            (Verdict::Blocked, Some("infra"))
        );
        assert_eq!(
            classify(
                false,
                "could not launch command: No such file",
                TestKind::Command
            ),
            (Verdict::Blocked, Some("infra"))
        );
    }

    #[test]
    fn auth_failures_are_blocked_so_flaky_can_exclude_them() {
        // `testsprite_flaky` advertises that auth failures are excluded rather
        // than scored as flakiness. That was false: no auth rule existed, so a
        // 401 fell through to Failed/assertion and landed in the denominator —
        // an intermittently-authenticating test read "flaky" precisely when the
        // contract promised it would not.
        for err in [
            "expected a 2xx/3xx response, got 401",
            "expected a 2xx/3xx response, got 403",
            "401 Unauthorized",
        ] {
            assert_eq!(
                classify(false, err, TestKind::Backend),
                (Verdict::Blocked, Some("auth")),
                "{err:?} should be blocked/auth"
            );
        }
    }

    #[test]
    fn an_echoed_auth_word_in_a_response_body_is_not_an_auth_wall() {
        // Regression: the auth rule matched a bare "unauthorized"/"forbidden",
        // which routinely appear in a Backend-Python test's echoed response
        // body. A genuine assertion failure whose body carries the word must
        // stay Failed/assertion — classifying it Blocked/auth would drop a real,
        // deterministic failure out of the flaky denominator as an env problem.
        let err = "AssertionError: expected 200, got 500; body={\"error\":\"Unauthorized\"}";
        assert_eq!(
            classify(false, err, TestKind::Backend),
            (Verdict::Failed, Some("assertion"))
        );
        // The real HTTP status line still classifies as auth.
        assert_eq!(
            classify(false, "GET /admin -> 403 Forbidden", TestKind::Backend),
            (Verdict::Blocked, Some("auth"))
        );
    }

    #[test]
    fn a_harness_timeout_is_failed_timeout_for_every_modality() {
        // The wall-clock backstop for hangs/deadlocks must fail (not block, not
        // get excused as subprocess noise) whichever executor produced it.
        for kind in [
            TestKind::Rust,
            TestKind::Command,
            TestKind::Backend,
            TestKind::Frontend,
        ] {
            assert_eq!(
                classify(
                    false,
                    "test exceeded the 300s time limit (possible hang/deadlock)",
                    kind
                ),
                (Verdict::Failed, Some("timeout")),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn pipeline_authored_prefixes_survive_every_modality() {
        // These are OUR words (the acceptance gate / repair loop / fault-check
        // write them), so they classify identically for subprocess modalities.
        for kind in [TestKind::Backend, TestKind::Rust, TestKind::Command] {
            assert_eq!(
                classify(
                    false,
                    "suspect oracle: failed against the current baseline at generation time: x",
                    kind
                ),
                (Verdict::Failed, Some("suspect_oracle")),
                "{kind:?}"
            );
            assert_eq!(
                classify(false, "build error: expected `;`", kind),
                (Verdict::Blocked, Some("build_error")),
                "{kind:?}"
            );
            assert_eq!(
                classify(false, "residual alignment: passes on the base commit", kind),
                (Verdict::Failed, Some("residual_alignment")),
                "{kind:?}"
            );
        }
        // And the kinds are registered, so stored rows re-borrow cleanly.
        for k in ["suspect_oracle", "build_error", "residual_alignment"] {
            assert_eq!(known_failure_kind(k), Some(k));
        }
    }

    #[test]
    fn a_browser_crash_string_from_a_shell_test_is_not_a_browser_crash() {
        assert_eq!(
            classify(false, "playwright executable missing", TestKind::Command),
            (Verdict::Failed, Some("assertion"))
        );
    }
}
