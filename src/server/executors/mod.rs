//! Executor seam — the architecture that lets one pipeline ship every test
//! modality (backend HTTP, browser/E2E, MCP, Rust) without a god object.
//!
//! The whole product is ONE fixed pipeline:
//!     surface → plan(edge cases) → generate(artifact) → execute → report
//! Only the *executor* changes between modalities. Everything else (store, API,
//! planner, coverage guard) is modality-agnostic and dispatches through the
//! [`Executor`] trait. A case is just JSON (`{id,title,description,spec?}`) so
//! the planner, store, and API never need to know which modality is running.

pub mod browser;
pub mod command;
pub mod http;
pub mod mcp;
pub mod rust;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::server::llm::LlmClient;

/// Which modality a run targets. Selected per-run; routes to one [`Executor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TestKind {
    #[default]
    Backend,
    Frontend,
    Mcp,
    Rust,
    #[serde(alias = "cargo", alias = "shell")]
    Command,
}

impl TestKind {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "frontend" | "browser" | "e2e" => TestKind::Frontend,
            "mcp" => TestKind::Mcp,
            "rust" | "unit" => TestKind::Rust,
            "command" | "shell" | "cargo" => TestKind::Command,
            _ => TestKind::Backend,
        }
    }
}

/// Shared context handed to every executor. The `llm` is injected (single owner)
/// — executors never construct their own client.
#[derive(Clone)]
pub struct ExecCtx {
    /// Backend/browser base URL, or the command/path for mcp/rust targets.
    pub target: String,
    /// Shared OpenAI client; `None` = deterministic mode.
    pub llm: Option<LlmClient>,
    /// PRD context for LLM artifact generation.
    pub prd: Arc<Value>,
    /// Which browser engine the frontend executor should launch
    /// (`chromium` | `firefox` | `webkit`); `None` defaults to `chromium`.
    pub browser: Option<String>,
    /// Directory to write per-case screenshots into; `None` skips screenshots.
    pub shots_dir: Option<std::path::PathBuf>,
    /// Project/repo root a `command` test runs in.
    pub root: std::path::PathBuf,
    /// Path-param variable seeds (`{id}` -> a real value) from
    /// `testsprite_tests/variables.json`; empty falls back to the `1` probe.
    pub variables: std::collections::HashMap<String, String>,
}

/// The outcome of executing one case.
pub struct Outcome {
    pub passed: bool,
    pub error: String,
    /// The artifact that was executed (Python / Playwright JS / JSON-RPC / Rust).
    pub code: String,
}

impl Outcome {
    pub fn pass(code: String) -> Self {
        Self {
            passed: true,
            error: String::new(),
            code,
        }
    }
    pub fn fail(error: impl Into<String>, code: String) -> Self {
        Self {
            passed: false,
            error: error.into(),
            code,
        }
    }
}

/// The one seam every modality implements. A case is JSON; executors that have
/// a deterministic spec read `case["spec"]`, others generate from the LLM.
#[async_trait::async_trait]
pub trait Executor: Send + Sync {
    /// Human label for logs/reports.
    fn label(&self) -> &'static str;
    /// Generate (if needed) and execute one case; return its outcome.
    async fn run(&self, case: &Value, ctx: &ExecCtx) -> Outcome;
}

/// Resolve the executor for a kind.
pub fn for_kind(kind: TestKind) -> Arc<dyn Executor> {
    match kind {
        TestKind::Backend => Arc::new(http::HttpExecutor),
        TestKind::Frontend => Arc::new(browser::BrowserExecutor),
        TestKind::Mcp => Arc::new(mcp::McpExecutor),
        TestKind::Rust => Arc::new(rust::RustExecutor),
        TestKind::Command => Arc::new(command::CommandExecutor),
    }
}

/// Bound subprocess output to `max` chars while preserving BOTH ends.
///
/// Compiler/build/test failures put the root cause at the TOP (e.g.
/// `error[E0308]: mismatched types`) and a long backtrace at the bottom, so
/// tail-only truncation drops the single most useful line. This keeps the head
/// (biased larger) and the tail with an elision marker between them. UTF-8-safe:
/// counts by `char` and never slices mid-codepoint.
pub(crate) fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head = (max * 3 / 5).max(1);
    let tail = max.saturating_sub(head);
    let chars: Vec<char> = s.chars().collect();
    let head_str: String = chars[..head].iter().collect();
    let tail_str: String = chars[n - tail..].iter().collect();
    let elided = n - head - tail;
    format!("{head_str}\n…[{elided} chars elided]…\n{tail_str}")
}

#[cfg(test)]
mod clip_tests {
    use super::clip;

    #[test]
    fn short_output_passes_through_trimmed() {
        assert_eq!(clip("  hello  ", 100), "hello");
    }

    #[test]
    fn keeps_root_cause_head_and_tail() {
        let input = format!("ROOT-CAUSE{}TRAILING-END", "M".repeat(400));
        let out = clip(&input, 40);
        assert!(
            out.starts_with("ROOT-CAUSE"),
            "head (root cause) kept: {out}"
        );
        assert!(out.ends_with("TRAILING-END"), "tail kept: {out}");
        assert!(out.contains("elided"), "marker present: {out}");
        assert!(
            out.chars().count() < input.chars().count(),
            "actually bounded"
        );
    }

    #[test]
    fn utf8_safe_on_multibyte_boundary() {
        // Byte-slicing this mid-codepoint would panic; char-based clip must not.
        let input = "🦀".repeat(500);
        let out = clip(&input, 100);
        assert!(out.contains("elided"));
        assert!(out.chars().any(|c| c == '🦀'));
    }
}
