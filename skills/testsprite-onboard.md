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

### 3. Generate the suite (deterministic first, LLM when useful)
```bash
testsprite-rs project summarize                         # code_summary.yaml: stack/features/routes
testsprite-rs test generate --from testsprite_tests/tmp/code_summary.yaml  # on-device specs (+ boundary probes) if api_endpoints exist
testsprite-rs test generate --doc openapi.json           # Postman/OpenAPI/HAR/GraphQL SDL -> deterministic specs
testsprite-rs test generate --cover --iterate=3          # one test per uncovered function (rust/py/js/ts/go), re-measuring coverage each round
testsprite-rs test explore --depth 1 --store             # frontend: discover planSteps from live app
testsprite-rs test audit --model gpt-5.3-codex,gpt-5.5 --store  # LLM adversarial QA, merged (consensus-voted, novelty-filtered)
```
LLM-generated cases (`--cover`, `--audit`, `--instruction`) are auto-screened
against the current baseline before they count: a case that fails on unchanged
code is **quarantined** as a suspect oracle (shown `[quarantined]` in `test list`,
excluded from suite runs until you `test release <id>` it). Deterministic
`--from`/`--doc` cases are never gated. `--cover` fans each function through
normal/boundary/exception views, so the seed suite exercises error paths, not
just the happy path.
Or from your coding agent over MCP: **`testsprite_generate_code_summary`**, **`testsprite_generate`**, **`testsprite_explore`**, and **`testsprite_audit`**. If you (the
coding agent) can already write the test yourself, prefer **`testsprite_store_test`**
(hand over your `spec`, `steps`, `planSteps`, or `code`) + `testsprite_run` — it runs deterministically
with no OpenAI key. Aim for
~8–15 tests on the core behaviors; don't pad. Every assertion must name a
**concrete, observable** outcome (status code, body field, element, count) — never
"verify it works". For a repo that already has its own test runner (cargo/pytest/jest),
prefer the repo-native loop: `testsprite_coverage_gaps` to find uncovered functions,
write/extend the repo's own tests for them, then register a `kind:"command"` test
(code = the run command, e.g. `cargo test -p ers-api --test http_contract`) and
`testsprite_run` it — deterministic, exit-0 = pass, no OpenAI key, and the
tests live in the repo (cargo/CI own them). Use kind backend/python only for
black-box HTTP tests.

`coverage_gaps` is a **worklist, not a scoreboard**. When it reports
`evidence: "named"` it is matching function names against the text of stored
tests, so it cannot see the repo's own cargo/pytest suite: a function you just
covered properly will still appear. Don't chase it to zero, and never write a
function's name into a test title or command string to make it go away — that
moves the number without testing anything.

**Auth / real QA flows:** for anything behind a login, prefer deterministic
`steps` over Python wrappers: one step logs in or mints a token, `save` captures
it, later steps use `auth:{"bearer":"${accessToken}"}` or `${var}` in headers,
body, form, or path. Put local secrets in `.testsprite.env` (gitignored) or CI
env; never store passwords/tokens in SQLite. A single static bearer still works
with `testsprite-rs project set-var authToken <token>` or `spec.headers`, but a
multi-step flow is the right shape when you need to prove login/OAuth/session
behavior. Use `graphql:{query,variables?,expect_no_errors?,expect_data?}` for GraphQL; it is just a shorthand on the same HTTP QA runner. GraphQL SDL docs import directly (`test generate --doc schema.graphql`) when fields need no required args. HTTP `QUERY` endpoints are supported.

### 4. Review then smoke-run
```bash
testsprite-rs prd review --out prd-review.html
# after reading generated requirements/plan:
testsprite-rs prd approve
testsprite-rs test run --json --require-approved-prd  # all; or --id <id>
```
Or **`testsprite_run`** over MCP. Each result carries `verdict`
(passed|failed|blocked), `failureKind`, and on failure an LLM `cause` + `fix`.

### 5. Report/artifacts
```bash
testsprite-rs test report --out testsprite_tests/testsprite-report.pdf
testsprite-rs test dashboard --out testsprite_tests/dashboard.html
testsprite-rs test replay <frontend-id> --out testsprite_tests/replay.html
```
Tell the user plainly: "N tests covering <flows>; smoke-ran M — <pass/fail>; artifacts at <paths>; run the rest with `testsprite-rs test run`, or gate CI with `testsprite-rs gate` (`testsprite-rs ci init` drops a ready pull_request workflow)."

## Don'ts
- Don't write narrative assertions an AI judge can rubber-stamp.
- Don't init a frontend project without a `--url`.
- Don't re-seed a project that already has tests.

## Hand off to verify
Once the project has a seeded suite and a first green run, the
**`testsprite-verify`** skill takes over on every subsequent change.
