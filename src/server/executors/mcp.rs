//! MCP executor — fuzz a JSON-RPC tool surface.
//!
//! The target is a command that starts a stdio MCP server (e.g. jfc's own MCP
//! server). For each case we send a `tools/call` with an edge-case argument
//! payload (the case's `payload`, or `{}`), and assert the server returns a
//! structured response (result or a clean error) rather than crashing or
//! emitting malformed JSON-RPC. This is the one modality where jfc itself is a
//! legitimate local target.

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::{ExecCtx, Executor, Outcome};

pub struct McpExecutor;

#[async_trait::async_trait]
impl Executor for McpExecutor {
    fn label(&self) -> &'static str {
        "mcp"
    }

    async fn run(&self, case: &Value, ctx: &ExecCtx) -> Outcome {
        let tool = case.get("tool").and_then(|v| v.as_str()).unwrap_or("");
        let payload = case.get("payload").cloned().unwrap_or_else(|| json!({}));
        let call = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": payload }
        });
        let code = serde_json::to_string_pretty(&call).unwrap_or_default();

        match probe_tool(&ctx.target, &call).await {
            Ok(probe) => {
                // A server that failed to initialize never dispatched anything,
                // so whatever came back for the call says nothing about how it
                // handles the payload.
                if let Some(err) = probe.init.as_ref().and_then(|i| i.get("error")) {
                    return Outcome::fail(format!("MCP server failed to initialize: {err}"), code);
                }
                evaluate(&probe.call, code)
            }
            Err(e) => Outcome::fail(format!("server did not respond cleanly: {e}"), code),
        }
    }
}

/// JSON-RPC errors that mean the server rejected THE CALL rather than handling
/// our edge-case payload.
///
/// `-32601` (method not found) is the one that matters: fuzzing a tool name the
/// server doesn't export returns it for every single case, and counting that as
/// "handled gracefully" scores a server that rejects everything at 100%.
/// `-32600`/`-32700` mean our own framing was malformed — a harness bug, not a
/// result. `-32603` (internal error) is an unhandled failure, which is exactly
/// what a fuzz case is looking for.
fn rejected_the_call(code: i64) -> Option<&'static str> {
    match code {
        -32601 => Some(
            "method not found — the server does not export this tool, so the payload was never exercised",
        ),
        -32600 => Some("invalid request — the server rejected our JSON-RPC framing"),
        -32700 => Some("parse error — the server could not parse our JSON-RPC framing"),
        -32603 => Some("internal error — the server did not handle the payload"),
        _ => None,
    }
}

/// A malformed/edge payload PASSES when the server *handled* it: a structured
/// result (including a tool-level `isError`), or a clean rejection of the
/// arguments (`-32602 invalid params`, or an application-defined error).
///
/// It FAILS when the server crashed, returned non-JSON, blew up internally, or
/// never dispatched to the tool at all.
fn evaluate(resp: &Value, code: String) -> Outcome {
    if resp.get("result").is_some() {
        return Outcome::pass(code);
    }
    let Some(err) = resp.get("error") else {
        return Outcome::fail("response missing both `result` and `error`", code);
    };
    match err.get("code").and_then(Value::as_i64) {
        Some(n) => match rejected_the_call(n) {
            Some(why) => Outcome::fail(format!("JSON-RPC error {n}: {why}"), code),
            // -32602 invalid params, or a server-defined error: the server
            // validated our payload and said no. That is graceful handling.
            None => Outcome::pass(code),
        },
        None => Outcome::fail("JSON-RPC error object has no `code`", code),
    }
}

/// The two responses a probe collects.
struct Probe {
    /// The `initialize` response (id=0), if the server sent one.
    init: Option<Value>,
    /// The `tools/call` response (id=1).
    call: Value,
}

/// Spawn the MCP server command, perform `initialize`, send `call`, and read
/// both matching response lines. `target` is a shell-style command string.
async fn probe_tool(target: &str, call: &Value) -> anyhow::Result<Probe> {
    let mut parts = shell_split(target);
    if parts.is_empty() {
        anyhow::bail!("empty MCP target command");
    }
    let program = parts.remove(0);
    let mut child = tokio::process::Command::new(program)
        .args(&parts)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    let mut reader = BufReader::new(stdout).lines();

    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": { "name": "testsprite-rs", "version": "0" } }
    });
    write_line(&mut stdin, &init).await?;
    write_line(&mut stdin, call).await?;

    // Read lines until we see the response to id=1 (the tools/call), keeping the
    // id=0 (initialize) response along the way, with a cap.
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut init = None;
        while let Ok(Some(line)) = reader.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match v.get("id").and_then(|i| i.as_i64()) {
                Some(0) => init = Some(v),
                Some(1) => return Ok(Probe { init, call: v }),
                _ => {}
            }
        }
        anyhow::bail!("no response for tools/call before stream closed")
    })
    .await;

    if let Err(e) = child.kill().await {
        tracing::debug!("could not kill MCP target process: {e}");
    }
    result.map_err(|_| anyhow::anyhow!("MCP server timed out"))?
}

async fn write_line(stdin: &mut tokio::process::ChildStdin, v: &Value) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(v)?;
    bytes.push(b'\n');
    stdin.write_all(&bytes).await?;
    stdin.flush().await?;
    Ok(())
}

/// Minimal shell-ish splitter (whitespace, double-quote grouping).
fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for ch in s.chars() {
        match ch {
            '"' => in_quote = !in_quote,
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    fn eval(resp: Value) -> Outcome {
        evaluate(&resp, String::new())
    }

    #[test]
    fn a_structured_result_is_graceful_handling() {
        assert!(eval(json!({"id": 1, "result": {"content": []}})).passed);
        // A tool-level error is still the server handling the input.
        assert!(eval(json!({"id": 1, "result": {"isError": true}})).passed);
    }

    #[test]
    fn cleanly_rejected_arguments_are_graceful_handling() {
        // -32602 is precisely what a fuzz case wants to see: the server
        // validated the edge-case payload and said no.
        assert!(
            eval(json!({"id": 1, "error": {"code": -32602, "message": "invalid params"}})).passed
        );
        // Application-defined errors are handled too.
        assert!(eval(json!({"id": 1, "error": {"code": -32000, "message": "nope"}})).passed);
    }

    #[test]
    fn method_not_found_is_not_a_pass() {
        // Regression: `pass iff result OR error` meant fuzzing a tool name the
        // server does not export returned -32601 for every case and scored
        // 100%. A server that rejects everything must not look perfect.
        let out = eval(json!({"id": 1, "error": {"code": -32601, "message": "method not found"}}));
        assert!(!out.passed);
        assert!(
            out.error.contains("does not export this tool"),
            "{}",
            out.error
        );
    }

    #[test]
    fn internal_errors_and_bad_framing_are_not_passes() {
        // An unhandled server-side blowup is the bug a fuzz case hunts for.
        assert!(!eval(json!({"id": 1, "error": {"code": -32603, "message": "boom"}})).passed);
        // These indicate our own framing is wrong — a harness bug, not a result.
        assert!(!eval(json!({"id": 1, "error": {"code": -32600}})).passed);
        assert!(!eval(json!({"id": 1, "error": {"code": -32700}})).passed);
    }

    #[test]
    fn a_response_with_neither_result_nor_error_fails() {
        assert!(!eval(json!({"id": 1})).passed);
        // An error object with no code cannot be classified.
        assert!(!eval(json!({"id": 1, "error": {"message": "?"}})).passed);
    }
}
