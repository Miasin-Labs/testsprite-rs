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
```
Or from your coding agent over MCP: **`testsprite_local_generate`**. If you (the
coding agent) can already write the test yourself, prefer **`testsprite_store_test`**
(hand over your `spec` or `code`) + `testsprite_local_run` — it runs deterministically
with no OpenAI key. Aim for
~8–15 tests on the core behaviors; don't pad. Every assertion must name a
**concrete, observable** outcome (status code, body field, element, count) — never
"verify it works".

### 4. Smoke-run a few
```bash
testsprite-rs test run --json                 # all; or --id <id> for a subset
```
Or **`testsprite_local_run`** over MCP. Each result carries `verdict`
(passed|failed|blocked), `failureKind`, and on failure an LLM `cause` + `fix`.

### 5. Report
Tell the user plainly: "N tests covering <flows>; smoke-ran M — <pass/fail>; run
the rest with `testsprite-rs test run` or gate CI with `testsprite-rs gate`."

## Don'ts
- Don't write narrative assertions an AI judge can rubber-stamp.
- Don't init a frontend project without a `--url`.
- Don't re-seed a project that already has tests.

## Hand off to verify
Once the project has a seeded suite and a first green run, the
**`testsprite-verify`** skill takes over on every subsequent change.
