# testsprite-rs

A from-scratch **Rust** reimplementation of the TestSprite MCP client
(`@testsprite/testsprite-mcp`). It speaks the same backend protocol
(`api.testsprite.com`), opens the same reverse tunnel (`*.tun.testsprite.com`,
yamux v2), and exposes the same MCP tools over stdio.

> Built by reverse-engineering the published npm plugin (bundle + source map)
> and verified **live, end-to-end** against production: it dispatches a real
> test plan, the cloud reaches your localhost through the tunnel, runs the
> generated Python test, and returns `PASSED`. (1 credit per test case.)

## What TestSprite is

An AI testing agent delivered as an **MCP server** that plugs into your coding
agent (Cursor, Claude Code, etc.). It turns your code/PRD into a test plan,
generates executable tests, runs them **in TestSprite's cloud against your
locally-running app** (reached via a reverse tunnel), and feeds pass/fail +
root-cause back to your agent.

## The full flow

```
IDE agent ──stdio(JSON-RPC/MCP)──> testsprite-rs ──HTTPS(Bearer key)──> api.testsprite.com
                                         │                                      ▲
                                         └── reverse tunnel (yamux v2) ─────────┘
                                             control WS + data plane; the cloud
                                             reaches your localhost:<port>
```

1. **`testsprite_bootstrap`** — seeds `testsprite_tests/tmp/config.json`
   (type, scope, `localEndpoint`), git-ignores it, then asks the agent for a
   code summary.
2. **`testsprite_generate_code_summary`** — returns a `next_action` telling the
   *agent* to scan the repo and write `code_summary.yaml`.
3. **`testsprite_generate_standardized_prd`** — uploads the summary + any PRD
   files to `POST /mcp/common/generate-prd`; the cloud LLM returns a structured
   PRD (goals + features + user-flows).
4. **`testsprite_generate_{frontend,backend}_test_plan`** — `POST /mcp/.../plan`
   returns test cases `[{id,title,description}, …]`.
5. **`testsprite_generate_code_and_execute`** — the heavy one:
   - opens the **reverse tunnel** (`POST /api/tunnel/v2` → `{id,secret}`;
     control WS `wss://control.tun.testsprite.com/ws?client_id=…`),
   - on each `RequestTunnel`, dials `data.tun.testsprite.com:7400`, runs a
     **yamux** session, and for each inbound stream connects to the named
     `target_host:target_port` (your `localhost:<port>`) and splices traffic,
   - verifies connectivity through the proxy, then `POST /mcp/.../run`
     `{testPlan, prdContent, endpoint, proxy, autoFix:true, maxAttempts:3}`,
   - polls `GET /mcp/project/test/{id}` every 3s until done,
   - writes Python test files + `test_results.json` + `raw_report.md`
     (analysis sections are `{{TODO:AI_ANALYSIS}}` placeholders the host LLM
     fills in — mirroring the original `llm.generate` next-action design),
   - closes the tunnel.

Dev-mode caps frontend tests at 15, prod at 30. ~1 credit per executed case.

## Two auth realms (same host)

- The `sk-user-…` **API key** works on `/api/me` and `/mcp/*` (the test plane).
- `/user/*`, `/billing/*` require a **Cognito JWT** (browser session) — the API
  key is rejected there. A leaked key can run tests but can't touch the account.

## Usage

```bash
export API_KEY=sk-user-...          # from www.testsprite.com/dashboard/settings/apikey

# verify the key / show account
testsprite-rs account

# run as an MCP server over stdio (default)
testsprite-rs serve

# console execution path: reads testsprite_tests/tmp/config.json (executionArgs)
# and runs the full tunnel → dispatch → poll → report flow
testsprite-rs generate-code-and-execute
```

Register it with an MCP client by pointing the command at the built binary with
`API_KEY` in the environment.

## Local backend — run the whole thing with NO account / NO cloud

`testsprite-rs backend` is a drop-in local reimplementation of
`api.testsprite.com`: same endpoint contract, same control-WS handshake, but it
runs entirely on your machine. Because the executor runs the generated tests
directly against your local app, the reverse tunnel collapses to an accept-only
control socket — no data plane needed.

```bash
# start the local backend (port 8787). Uses OpenAI if a key is available,
# else a deterministic engine.
testsprite-rs backend --port 8787 --model gpt-4o-mini

# point the (unmodified) client at it — any API_KEY works, it's ignored:
API_KEY=local \
API_URL=http://127.0.0.1:8787 \
TSEMCP_TUNNEL_CONTROL_URL=ws://127.0.0.1:8787/ws \
TSEMCP_TUNNEL_VERSION=2 \
testsprite-rs generate-code-and-execute
```

**Two intelligence modes:**

- **LLM mode** (when an OpenAI key is found in `OPENAI_API_KEY` or
  `~/.config/jfc/credentials.toml` → `[openai].api_key`): the backend uses the
  model exactly like the real cloud — generates a structured **PRD** from the
  code summary, a **test plan** (happy paths + error cases), and **executable
  Python** (`requests`) per case, then runs `python3` against your app for real
  pass/fail. Verified end-to-end: 8 LLM-authored tests generated and executed,
  passing/failing on genuine assertions.
- **Deterministic mode** (no key): derives the plan + Python directly from the
  code summary's `api_endpoints` (`{method, path, body?, expect_status?}`),
  asserting status codes. No model, no network beyond the app under test.

The code summary may include `base_url` and per-endpoint `expect_status` to make
the deterministic checks precise:

```json
{ "project_name": "app", "base_url": "http://localhost:8333",
  "api_endpoints": [ {"method":"GET","path":"/health","expect_status":200} ] }
```

## Module map (mirrors the original plugin)

| Rust module | Original | Role |
|---|---|---|
| `envs.rs` | `common/envs.ts` | endpoints (API, tunnel control/data/proxy) |
| `paths.rs` | `common/path.ts` | `testsprite_tests/...` layout |
| `types.rs` | `common/interface.ts` | Config, TestType, TestCase, TestEntity, … |
| `config.rs` | `common/config.ts` | read/save/wait-commited config |
| `backend.rs` | `common/backendClient.ts` | REST client + multipart + poll |
| `net.rs` | `common/network.ts` | port check + tunnel probe |
| `report.rs` | `generateMcpTestReport` | markdown report |
| `tunnel/protocol.rs` | `tunnelClient/v2/protocol.ts` | frame codec + control msgs |
| `tunnel/client.rs` | `tunnelClient/v2/client.ts` | control WS + yamux data plane |
| `tunnel/mod.rs` | `tunnelClient/v2/index.ts` | version negotiation + proxy URL |
| `tools/*` | `tools/*.ts` | the 8 MCP tools + orchestrator |
| `mcp.rs` | `index.ts` | stdio JSON-RPC MCP server |
| `server/api.rs` | (the cloud) | local `api.testsprite.com` REST contract |
| `server/llm.rs` | (the cloud LLM) | OpenAI PRD/plan/test-code generation |
| `server/engine.rs` | (the cloud) | deterministic generator (no-LLM fallback) |
| `server/store.rs` | (the sandbox) | test store + executor (HTTP spec or `python3`) |
| `server/mod.rs` | (control plane) | HTTP + accept-only control WebSocket |

## Endpoints

```
GET  /api/me                       account (plan, credits, email)
POST /mcp/common/generate-prd      multipart: files[] + codeSummary + testType
POST /mcp/backend-test/plan        {prdContent, targetScope}
POST /mcp/frontend-test/generate-plan
POST /mcp/{frontend,backend}-test/run   {testPlan, prdContent, endpoint, proxy, ...}
GET  /mcp/project/test/{id}        poll a test entity
POST /api/tunnel/v2                {} -> {id, secret}
GET  /api/tunnel/v2/version
```

## Status

Builds clean (`cargo build`, `cargo clippy` — 0 warnings). The account check,
MCP `initialize`/`tools/list`/`tools/call`, the tunnel, and a full backend
`generate-code-and-execute` run were all verified live. Tunnel **v2** is
implemented; v1 (legacy HMAC-challenge TCP tunnel) is detected and rejected.

This is a research reimplementation for understanding the protocol; it is not
affiliated with TestSprite.
