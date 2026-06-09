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
            Ok(resp) => evaluate(&resp, code),
            Err(e) => Outcome::fail(format!("server did not respond cleanly: {e}"), code),
        }
    }
}

/// A malformed/edge payload PASSES if the server replies with a structured
/// JSON-RPC response (result, or an error/isError) — i.e. it handled the bad
/// input gracefully. It FAILS only if the server crashed or returned non-JSON.
fn evaluate(resp: &Value, code: String) -> Outcome {
    if resp.get("result").is_some() || resp.get("error").is_some() {
        Outcome::pass(code)
    } else {
        Outcome::fail("response missing both `result` and `error`", code)
    }
}

/// Spawn the MCP server command, perform `initialize`, send `call`, read the
/// matching response line. `target` is a shell-style command string.
async fn probe_tool(target: &str, call: &Value) -> anyhow::Result<Value> {
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

    // Read lines until we see the response to id=1 (the tools/call), with a cap.
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Ok(Some(line)) = reader.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v.get("id").and_then(|i| i.as_i64()) == Some(1)
            {
                return Ok(v);
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
