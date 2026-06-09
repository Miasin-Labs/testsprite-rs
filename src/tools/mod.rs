//! MCP tool implementations + the `next_action` output model.
//!
//! TestSprite tools mostly return a `next_action` that *steers the host LLM*
//! (run a command, call another tool, generate text) rather than doing all the
//! work themselves. We mirror that contract.

pub mod account;
pub mod execute;
pub mod init;
pub mod plan;
pub mod prd;

use serde_json::{Value, json};

/// Wrap a list of next-action steps in the standard tool output shape.
pub fn next_action(steps: Vec<Value>) -> Value {
    json!({ "next_action": steps })
}

/// A "call this tool next" step.
pub fn tool_use(tool: &str) -> Value {
    json!({ "type": "tool_use", "tool": tool })
}
