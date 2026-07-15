//! Pure, deterministic classification of a completed `Outcome` into the V3
//! `(verdict, failureKind)` pair. Driven only by `passed` + the executor's
//! free-text `error` string — no I/O, no network, no LLM.

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
}

/// Classify a completed outcome into the V3 `(verdict, failureKind)` pair.
/// `failureKind` is `None` when passed. First matching rule wins.
pub fn classify(passed: bool, error: &str) -> (Verdict, Option<&'static str>) {
    if passed {
        return (Verdict::Passed, None);
    }

    let lower = error.to_lowercase();

    // Dependency-wave skip: a test not run because an upstream producer failed.
    if lower.starts_with("skipped: dependency") {
        return (Verdict::Blocked, Some("dependency"));
    }

    if lower.contains("could not write test file")
        || lower.contains("python3 failed to launch")
        || lower.contains("no spec and no llm")
        || lower.contains("code generation failed")
    {
        return (Verdict::Blocked, Some("infra"));
    }

    let mentions_browser = ["webkit", "chromium", "firefox", "playwright", "browser"]
        .iter()
        .any(|k| lower.contains(k));
    let mentions_crash = ["missing", "launch", "executable", "crash", "shared librar", ".so"]
        .iter()
        .any(|k| lower.contains(k));
    if mentions_browser && mentions_crash {
        return (Verdict::Blocked, Some("browser_crash"));
    }

    if lower.starts_with("request failed") {
        if lower.contains("timed out") || lower.contains("timeout") || lower.contains("deadline") {
            return (Verdict::Failed, Some("network_timeout"));
        }
        if lower.contains("refused")
            || lower.contains("connect")
            || lower.contains("dns")
            || lower.contains("could not resolve")
            || lower.contains("tcp")
        {
            return (Verdict::Blocked, Some("network"));
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
        assert_eq!(classify(true, ""), (Verdict::Passed, None));
    }

    #[test]
    fn routing_404() {
        assert_eq!(
            classify(false, "expected Some(200), got 404"),
            (Verdict::Failed, Some("routing_404"))
        );
    }

    #[test]
    fn connection_refused_is_blocked_network() {
        assert_eq!(
            classify(
                false,
                "request failed: error sending request: connection refused"
            ),
            (Verdict::Blocked, Some("network"))
        );
    }

    #[test]
    fn timed_out_request_is_network_timeout() {
        assert_eq!(
            classify(false, "request failed: operation timed out"),
            (Verdict::Failed, Some("network_timeout"))
        );
    }

    #[test]
    fn python_assertion_error() {
        assert_eq!(
            classify(false, "AssertionError: 1 != 2"),
            (Verdict::Failed, Some("assertion"))
        );
    }

    #[test]
    fn python_launch_failure_is_infra() {
        assert_eq!(
            classify(false, "python3 failed to launch: No such file"),
            (Verdict::Blocked, Some("infra"))
        );
    }

    #[test]
    fn webkit_missing_shared_lib_is_browser_crash() {
        assert_eq!(
            classify(false, "webkit: missing shared library libicudata.so.74"),
            (Verdict::Blocked, Some("browser_crash"))
        );
    }

    #[test]
    fn dependency_skip_is_blocked() {
        assert_eq!(
            classify(false, "skipped: dependency 'auth_token' unmet (upstream producer failed)"),
            (Verdict::Blocked, Some("dependency"))
        );
    }
}
