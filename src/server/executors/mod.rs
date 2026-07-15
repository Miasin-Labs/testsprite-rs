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
}

impl TestKind {
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "frontend" | "browser" | "e2e" => TestKind::Frontend,
            "mcp" => TestKind::Mcp,
            "rust" | "unit" => TestKind::Rust,
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
    }
}
