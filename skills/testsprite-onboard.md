# testsprite-rs: onboard a repo with a seed test suite (local, no cloud)

Take a repo that has **no testsprite-rs tests yet** and leave it with a runnable
suite plus a couple of **already-green** smoke tests — in one pass. Everything is
local: a single SQLite DB (`testsprite_tests/testsprite.db`), no account, no
credits, no reverse tunnel. (This is the local adaptation of the official
`testsprite-onboard` skill — see `skills/upstream/` for the cloud original.)

## When to use
- Any project with 0–1 stored tests. The user says "set up tests", "bootstrap",
  "seed a suite", "get me started".

## When NOT to use
- The project already has tests — that's `testsprite-verify`'s job.

## Steps

### 1. Understand the app from the source
Read the repo to establish concretely: backend → the key API endpoints and their
success/error contracts; frontend → the 4–8 most important user flows and whether
they need login. Prefer code-derived routes over guessing.

### 2. Init the project
```bash
testsprite-rs project init --type backend --name "<repo>" --url <base-url>
#   --type ∈ backend | frontend | mcp | rust ; frontend needs a real --url
```

### 3. Generate the suite (LLM; needs OPENAI_API_KEY, else deterministic)
```bash
testsprite-rs test generate --instruction "cover <the key behaviors you found>"
testsprite-rs test generate --cover        # one test per currently-uncovered function
testsprite-rs test generate --doc README.md   # distill a PRD from a README/notes/spec, then plan
```
Or from your coding agent over MCP: **`testsprite_local_generate`**. If you (the
coding agent) can already write the test yourself, prefer **`testsprite_store_test`**
(hand over your `spec` or `code`) + `testsprite_local_run` — it runs deterministically
with no OpenAI key. Aim for
~8–15 tests on the core behaviors; don't pad. Every assertion must name a
**concrete, observable** outcome (status code, body field, element, count) — never
"verify it works". For a repo that already has its own test runner (cargo/pytest/jest),
prefer the repo-native loop: `testsprite_coverage_gaps` to find uncovered functions,
write/extend the repo's own tests for them, then register a `kind:"command"` test
(code = the run command, e.g. `cargo test -p ers-api --test http_contract`) and
`testsprite_local_run` it — deterministic, exit-0 = pass, no OpenAI key, and the
tests live in the repo (cargo/CI own them). Use kind backend/python only for
black-box HTTP tests.

**Auth:** for anything behind a login, provide test credentials (or have the test
inject the auth header itself) — otherwise authenticated flows come back `blocked`,
not `failed`, and you'll chase a phantom bug. Use a dedicated test user (e.g.
`you+test@example.com`, a known OTP/password) and configure it once so runs stay green.

### 4. Smoke-run a few
```bash
testsprite-rs test run --json                 # all; or --id <id> for a subset
```
Or **`testsprite_local_run`** over MCP. Each result carries `verdict`
(passed|failed|blocked), `failureKind`, and on failure an LLM `cause` + `fix`.

### 5. Report
Tell the user plainly: "N tests covering <flows>; smoke-ran M — <pass/fail>; run
the rest with `testsprite-rs test run`, or gate CI with `testsprite-rs gate`
(`testsprite-rs ci init` drops a ready pull_request workflow)."

## Don'ts
- Don't write narrative assertions an AI judge can rubber-stamp.
- Don't init a frontend project without a `--url`.
- Don't re-seed a project that already has tests.

## Hand off to verify
Once the project has a seeded suite and a first green run, the
**`testsprite-verify`** skill takes over on every subsequent change.
