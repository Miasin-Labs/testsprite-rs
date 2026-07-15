# AGENTS.md

Guidance for AI agents (and humans) working in **testsprite-rs**.

## What this is

A from-scratch **Rust** reimplementation of the TestSprite MCP client
(`@testsprite/testsprite-mcp`). It has two halves that share the same types:

1. **Client** — speaks the real cloud protocol (`api.testsprite.com`), opens the
   reverse tunnel (`*.tun.testsprite.com`, yamux v2), and exposes 7 MCP tools
   over stdio (`mcp.rs::tool_list`; the README says "8", counting an
   original-plugin tool this port folds in).
2. **Local server** (`testsprite-rs backend`) — a drop-in local stand-in for the
   cloud, so the whole flow runs with no account and no network beyond the app
   under test. One pipeline (`surface → plan → generate → execute → report`)
   where only the *executor* varies by modality.

It is a research reimplementation, not affiliated with TestSprite. See
`README.md` for protocol details and the full flow.

## Build / test / lint

```bash
cargo build            # edition 2024
cargo test             # 4 unit tests (all in server/coverage.rs) — must stay green
cargo clippy --all-targets   # keep at 0 warnings (project standard)
```

Run modes:

```bash
testsprite-rs serve                    # stdio MCP server (default subcommand)
testsprite-rs account | check          # verify API key / show account
testsprite-rs generate-code-and-execute# console: tunnel → dispatch → poll → report
testsprite-rs backend --port 8787 --model gpt-4o-mini --kind backend
#   --kind ∈ { backend | frontend | mcp | rust }
```

## Environment

| Var | Purpose | Default |
|---|---|---|
| `API_KEY` | `sk-user-…` bearer key (client → cloud) | required for cloud calls |
| `API_URL` | REST base | `https://api.testsprite.com` |
| `TESTSPRITE_URL` | dashboard base (for result URLs) | `https://www.testsprite.com` |
| `TSEMCP_TUNNEL_CONTROL_URL` | control-plane WS | `wss://control.tun.testsprite.com/ws` |
| `TSEMCP_TUNNEL_DATA_ADDRESS` / `_DATA_HOST` / `_DATA_PORT` | yamux data plane | `data.tun.testsprite.com:7400` |
| `TSEMCP_TUNNEL_PROXY_URL` | proxy handed to the cloud runner | `http://proxy.tun.testsprite.com:9090` |
| `TSEMCP_TUNNEL_VERSION` | force tunnel version (0=ask, 1, 2) | `0` |
| `OPENAI_API_KEY` | enables LLM mode in `backend` | falls back to deterministic engine |

LLM mode also reads `~/.config/jfc/credentials.toml` → `[openai].api_key`.
Every endpoint/value is env-overridable; defaults point at production.

## Architecture

Two halves, one shared type layer (`types.rs`, `envs.rs`, `paths.rs`).

### Client (talks to the real cloud)

| Module | Role |
|---|---|
| `main.rs` | clap CLI + subcommand dispatch |
| `mcp.rs` | stdio JSON-RPC MCP server: `initialize` / `tools/list` / `tools/call` |
| `tools/*` | the 7 MCP tool handlers, incl. the `generate_code_and_execute` orchestrator (`execute.rs`) |
| `tunnel/{protocol,client,mod}.rs` | yamux-v2 reverse tunnel: frame codec, control WS, data plane |
| `backend.rs` | REST client for `api.testsprite.com` (bearer auth + multipart + poll) |
| `config.rs` / `paths.rs` | `testsprite_tests/tmp/config.json` + on-disk layout |
| `net.rs` / `report.rs` | port/tunnel probes; markdown report builder |

### Local server (`server/`, stand-in for the cloud)

| Module | Role |
|---|---|
| `server/mod.rs` | axum HTTP + accept-only control WebSocket |
| `server/api.rs` | the REST contract the client calls (router + handlers) |
| `server/llm.rs` | OpenAI PRD / plan / test-code generation |
| `server/engine.rs` | deterministic generator (no-LLM fallback) from `api_endpoints` |
| `server/store.rs` | test store + the single execution path |
| `server/coverage.rs` | Coverage Guard: declared vs. exercised surface (+ Mermaid) |
| `server/executors/` | the `Executor` seam |

### The Executor seam (the load-bearing abstraction)

`server/executors/mod.rs` defines `trait Executor` and `enum TestKind`. The
store, API, planner, and Coverage Guard are all modality-agnostic; only the
executor changes. A test case is just JSON (`{id,title,description,spec?}`), and
the `LlmClient` is injected once (single owner) into `ExecCtx` — executors never
construct their own. When adding a modality:

- Add a `TestKind` variant + `parse` mapping,
- Implement `Executor` (`label`, `run`) in a new `executors/<kind>.rs`,
- Wire it in `for_kind`.

Do **not** add a parallel code path or thread modality logic through the store /
API / planner — that defeats the seam.

## Conventions

- **Mirror the original plugin** module-for-module (see the Module map in
  `README.md`); keep that correspondence when adding features.
- **Everything env-overridable** with a production default (`envs.rs` pattern).
- **A case is JSON.** Don't add modality-specific structs to the planner/store.
- **Errors:** `anyhow::Result` at boundaries; non-2xx maps to hard-stop
  semantics matching the plugin (`backend.rs::check`).
- **Zero clippy warnings** is the bar; fix at the source, don't `#[allow]` around it.
- Unit tests live in-module under `#[cfg(test)]` (see `coverage.rs`).

## Cleanup & modularization findings

Grounded assessment (codegraph + `cargo build/test/clippy`, 2026-07):
the codebase is **genuinely well-factored** — do not invent a large refactor.

- **No monoliths.** Largest source files: `api.rs` 346, `backend.rs` 296,
  `execute.rs` 293, `tunnel/client.rs` 277, `llm.rs` 220 lines. All are single-
  responsibility and cohesive.
- **Clean signals.** `cargo build` + `cargo clippy` at 0 warnings; `cargo test`
  4/4; codegraph vuln scan 0 findings across 592 functions.

Minor, optional items (in priority order):

1. **Thin test coverage is the real gap.** Only `coverage.rs` has unit tests.
   Highest-value work: add pure-function unit tests for `tunnel/protocol.rs`
   (frame encode/decode round-trip), `engine.rs::concrete_path` (`{param}`
   templating), `execute.rs::parse_endpoint` (host/port parsing), and
   `account.rs::mask_api_key` / `execute.rs::redact` (secret masking). These are
   deterministic and currently unverified.
2. **`parse_endpoint` name collision.** Two unrelated functions share the name —
   `tools/execute.rs` (URL → `(host, port)`) and `server/engine.rs`
   (JSON → `EndpointSpec`). Not duplication, but consider renaming to
   `parse_host_port` / `parse_endpoint_spec` for clarity.
3. **`api.rs` split — only if it grows.** At 346 lines it's fine. Handlers are
   already grouped by comment banners (account/tunnel, PRD, plans, run, poll,
   coverage, sinks); a natural split into `api/{plan,run,coverage}.rs` submodules
   is available but not warranted at the current size.
4. **Not slop:** `log_sink` / `test_summary` in `api.rs` are intentional no-op
   acks for endpoints the client calls but a local backend needn't process.
   Leave them.
5. **Unverified end-to-end:** the `frontend` (Playwright) and `rust` (`cargo
   test`) executors build with deterministic fallbacks but were never run e2e.
   Exercise them before claiming those modalities work.

Uncommitted at time of writing: `server/llm.rs` omits `temperature` for newer
OpenAI models (gpt-5/6, o1/o3/o4) that reject an explicit `0.2` — a sensible fix
worth committing.
