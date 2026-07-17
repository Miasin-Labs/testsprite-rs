# testsprite-rs: verification loop (local, no cloud)

You just finished a piece of work in a testsprite-rs-tested repo. Before you
report it done, **actually run the relevant test(s)** and read the result. Spec
review and unit tests catch correctness; only running the test catches what
breaks for a real user. Everything is local (SQLite) — no deployed URL, no
credits. (Local adaptation of the official `testsprite-verify` skill; the cloud
original is in `skills/upstream/`.)

## The one-test minimum
Every shipped change gets **at least one** `testsprite-rs test run` to a terminal
verdict (`passed` / `failed` / `blocked`) before you call it done. Unit tests,
typecheck, and lint do **not** count.

## When to skip (narrow)
- Docs-only edits (`*.md`, comments) or pure config/lockfile bumps.
- The repo isn't wired to testsprite-rs (no `testsprite_tests/testsprite.db`).

## Steps

### 0. The one-call loop (fastest path)
```bash
testsprite-rs loop --changed --generate   # generate-for-changed → run → triage → surface
testsprite-rs loop --json --require-approved-prd
```
Over MCP: `testsprite_loop` (`changed`/`since`/`generate`/`fix`/`serve`). It runs
generate-if-uncovered → run → triage → surface in one call and returns
`{selection,total,passed,failed,blocked,failures,clusters,next_action,green}` —
`green:false` (exit 1) unless nothing failed or was left unverified. `blocked`
(auth/network/infra) is counted apart from real `failed`. Use the explicit steps
below when you need finer control.

Concurrent `test run` / `loop` invocations against one project **serialize
automatically** via an advisory `testsprite_tests/tmp/execution.lock` (a stale
lock is stolen after a TTL or when its recorded pid is dead), so parallel
CI/agent runs no longer race the shared app/auth/DB — you don't need to
hand-sequence them.

### 1. See what changed — and test only that (Code Diff Mode)
```bash
testsprite-rs test changed                # changed functions (git) + which stored tests they affect
testsprite-rs test run --changed          # run ONLY the tests affected by your uncommitted changes
testsprite-rs test run --changed --since origin/main   # or vs a branch, for the whole PR
```
This is the fast pre-merge loop: it maps your `git diff` to the functions you
touched and runs the tests that mention them. `testsprite-rs test generate
--changed` synthesizes a test for any changed function no test mentions yet; add
`--fault-check` to keep a generated regression test **only if it fails on the
base revision and passes on HEAD** (proving it detects your change) — one that
passes on both is quarantined as not-a-regression. Over MCP: `testsprite_run` /
`testsprite_generate` with `changed:true` (`since`/`fault_check` optional). Use
the explicit flow below when you need a specific test.

Selection is by function-**name** mention, so it cannot see through a `spec` or
`command` test — those carry no Rust function name. When something changed but
nothing is attributable, testsprite runs the **full suite** and says so rather
than reporting an empty success; `selection: "unattributable"` in the MCP result
means "I could not tell what covers this", never "you're clear".

### 2. Find or generate a covering test
```bash
testsprite-rs test list                              # existing tests
testsprite-rs test run --id <id> --json              # run one that covers the change
# or synthesize one for the new behavior:
testsprite-rs project summarize
# deterministic from routes, no LLM if api_endpoints exist:
testsprite-rs test generate --from testsprite_tests/tmp/code_summary.yaml
# or adversarial multi-model LLM when useful:
testsprite-rs test audit --model gpt-5.3-codex,gpt-5.5 --store
testsprite-rs test run --json
```
Over MCP: `testsprite_run` (and `testsprite_generate`). If you can
write the covering test yourself, prefer `testsprite_store_test` (`spec`,
`steps`, or `code`) + `testsprite_run` — deterministic, no OpenAI key needed.

LLM-generated cases pass an **acceptance gate** before they count: each is run
once against the current (green) baseline, and one that fails on unchanged code
is **quarantined** as `suspect_oracle` — it stays stored and shows `[quarantined]`
in `test list`, but is excluded from whole-suite runs until you inspect it. A
quarantined case is usually a hallucinated assertion, but occasionally a real
bug it surfaced early; `testsprite-rs test run --id <id>` runs it explicitly and
`testsprite-rs test release <id>` reinstates it once you've confirmed the oracle.
Pass `--no-gate` to skip screening, `--budget <tokens>` to cap LLM spend, and
`--cover --iterate[=N]` to regenerate against still-uncovered functions until a
plateau. Deterministic (`--from`/`--doc`/hand-written) cases are never gated —
they carry no hallucinated oracle.
Use `steps` for real QA flows (login/OAuth -> save token -> REST/GraphQL/QUERY call) and `planSteps` for frontend browser flows. Use `.testsprite.env`/process env placeholders, not hard-coded secrets — and note that a PRD ingested via `project ingest-prd` / `test generate` (or MCP `testsprite_ingest_prd`) auto-seeds its `testCredentials`/`test_environment` into `variables.json`, so `${adminUser_password}` / `${frontend_url}` etc. are available as `${var}` (never clobbering values you set). Plain natural-language `planSteps` in official phrasing ("Navigate to /path", "Input Email: ${EMAIL}", "Click Sign In", "Verify: X") compile deterministically to Playwright with no per-run LLM, complementing object-form `selectors:[...]` healing. Prefer a single self-contained test; assert concrete, observable outcomes.
For a repo with its own test runner (cargo/pytest/jest), the best flow is
`testsprite_coverage_gaps` to find uncovered functions, write/extend the repo's
own tests for them, then register a `kind:"command"` test (code = the run command,
e.g. `cargo test -p ers-api --test http_contract`) and `testsprite_run` it —
deterministic, exit-0 = pass, no OpenAI key, tests live in the repo.

**Do not chase `coverage_gaps` to zero, and do not write function names into a
test's title or command to satisfy it.** When it reports `evidence: "named"` it
is matching function names against your stored tests' text, which means it cannot
see your repo's own cargo/pytest suite — a function you just covered properly will
still be listed. That is a limitation of the matcher, not a real gap, and
name-dropping to clear it produces a green number attached to nothing. Read it as
a worklist of functions worth looking at. The number that means something is
`evidence: "executed"` (real `cargo llvm-cov` data).

Note that a stored `kind:"rust"` test is compiled as an integration test, so it
can only call `pub` items. Private functions are unreachable from the store by
construction — cover those with the repo's own `#[cfg(test)]` tests and register
the `command` test that runs them. Use kind
backend/python only for black-box HTTP tests. Black-box `spec`/backend cases need
the app running at the project's target URL: point at a live URL, or persist a
start command with `testsprite-rs project set-start "<cmd>"` and run `test run
--serve` (or MCP `testsprite_run` `serve:true`) — testsprite boots the app, runs,
and tears it down. Prefer this over wrapping `cargo test` when you have real HTTP behavior to check.

### 2a. Enforce generated-plan review
Generated cases are stamped with `prdId`. For high-stakes/generated suites, require the human review checkpoint before running:
```bash
testsprite-rs prd review --out prd-review.html
testsprite-rs prd approve
testsprite-rs test run --require-approved-prd
```

### 2b. Inspect/update visual steps
```bash
testsprite-rs test get <id>                 # exact stored JSON
testsprite-rs test plan put <id> --file steps.json
testsprite-rs test replay <id> --out replay.html
```
Use this for frontend selector/step drift: update `planSteps` (include `selectors:[...]` fallbacks), rerun the same stored test, and inspect replay HTML/screenshots/video.

### 3. Read the verdict, act on failure
On failure the result carries `failureKind`, an LLM `cause`, and a suggested
`fix`. `testsprite-rs test run --fix` writes a fix *recommendation* to
`testsprite_tests/fixes/<id>.md` — an explanation plus an illustrative code
sketch. The fix engine never reads your source, so the sketch's paths and line
numbers are the model's reconstruction: apply it by hand, don't expect it to
`git apply`. Fragility-only failures can be auto-adapted with `testsprite-rs
test rerun --heal <id>` (verifies the rewrite passes and snapshots the original
first, and rejects any rewrite that would weaken the assertion).

### 4. Gate / coverage
```bash
testsprite-rs gate                        # JUnit + JSON + exit 1 on any failure (CI)
testsprite-rs gate --smoke                # run one case per group first; escalate to full suite only if it passes
testsprite-rs gate --min-mutation 60      # ALSO fail if the mutation kill score < 60% (weak oracles, not just failing tests)
testsprite-rs coverage                    # cargo llvm-cov (Rust) + tree-sitter structural surface
testsprite-rs coverage --mutation         # ORACLE STRENGTH: cargo-mutants kill rate — a green, high-coverage suite can still catch zero bugs
# artifacts:
testsprite-rs test report --out testsprite_tests/testsprite-report.pdf
testsprite-rs test dashboard --out testsprite_tests/dashboard.html
```
`test report` (and MCP `testsprite_report`) produce TestSprite's official
requirement-grouped format — a **Requirement Validation Summary**, per-failure
**Severity** (HIGH/MEDIUM/LOW from `failureKind`), a per-requirement **Coverage &
Matching Metrics** matrix, and **Key Gaps / Risks** — so on failure you can triage
by requirement and severity rather than reading a flat list.
Coverage says a line *ran*; mutation says a test would *catch a bug* in it — the
two dissociate, so treat a high coverage number with a low kill score as a suite
of weak oracles, and strengthen the assertions on the surviving mutants it lists.
`testsprite-rs test triage` groups failures by root cause, ranked most-debuggable
first; `testsprite-rs test guidelines` distills your recurring failures into
do/don't rules (also auto-fed into generation prompts to stop the model repeating
them).

## If you can't run it
Say so explicitly: "Shipped but I could not run any testsprite-rs test because
<X> (no key / no project). Treat this as unverified." Don't claim done.

## Authoring assertions the runner won't false-PASS
- One verb per step; describe outcomes, not selectors.
- Presence ≠ working: assert the thing is correct, not just that a tag exists.
- Keep flows that depend on external state (OAuth, native dialogs, iframes) out of scope.
## Hunt the edges — but derive them from THIS system, not a checklist

Bugs live where the code's own assumptions break — and those are specific to the
system in front of you, not a fixed list you can pre-print. Don't run a canned
checklist; **read the actual code/contract (use the code map) and ask, for each
seam, "what does a real user do that this code doesn't handle?"** The sophisticated
bugs — the cross-source join that's only right down one path, the invariant that
holds for admin but leaks for a scoped caller, the value that's correct in
isolation but wrong after a migration — only surface when you reason about *that*
resolver's real behavior, not a generic template.

Use the obvious classes (empty/null/zero, malformed or unexpected input,
boundaries, auth/scope, config) as *spark* to get started — then keep going past
them into what's actually risky here. If the code has a branch, a join, a cache, a
scope check, or a fallback, there's an edge; test the one a user would hit.

Assert the **concrete observable** a user sees (a status, a field value, a count),
never "it works".

## Regression rule — a bug report is a test you haven't written yet

When a bug is reported, FIRST write a test that reproduces it — it should **fail**
against the current code (red), proving you caught the real defect. Store it
(`testsprite_store_test` / `test emit`), fix the code, then the same test passes
(green) and guards that bug forever. Turn every "it's returning null/0/an error"
into a permanent assertion.
