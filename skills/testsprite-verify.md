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

### 1. See what changed
```bash
git diff --stat        # then read the changed files
```

### 2. Find or generate a covering test
```bash
testsprite-rs test list                              # existing tests
testsprite-rs test run --id <id> --json              # run one that covers the change
# or synthesize one for the new behavior:
testsprite-rs test generate --instruction "<the behavior you changed>"
testsprite-rs test run --json
```
Over MCP: `testsprite_local_run` (and `testsprite_local_generate`). If you can
write the covering test yourself, prefer `testsprite_store_test` (`spec` or
`code`) + `testsprite_local_run` — deterministic, no OpenAI key needed. Prefer a
single self-contained test; assert concrete, observable outcomes.
For a repo with its own test runner (cargo/pytest/jest), the best flow is
`testsprite_coverage_gaps` to find uncovered functions, write/extend the repo's
own tests for them, then register a `kind:"command"` test (code = the run command,
e.g. `cargo test -p ers-api --test http_contract`) and `testsprite_local_run` it —
deterministic, exit-0 = pass, no OpenAI key, tests live in the repo. Use kind
backend/python only for black-box HTTP tests.

### 3. Read the verdict, act on failure
On failure the result carries `failureKind`, an LLM `cause`, and a suggested
`fix`. `testsprite-rs test run --fix` writes a unified-diff repair patch to
`testsprite_tests/fixes/<id>.md` for you to apply. Fragility-only failures can
be auto-adapted with `testsprite-rs test rerun --heal <id>` (never masks a real bug).

### 4. Gate / coverage
```bash
testsprite-rs gate            # JUnit + JSON + exit 1 on any failure (CI)
testsprite-rs coverage        # cargo llvm-cov (Rust) + tree-sitter structural surface
```

## If you can't run it
Say so explicitly: "Shipped but I could not run any testsprite-rs test because
<X> (no key / no project). Treat this as unverified." Don't claim done.

## Authoring assertions the runner won't false-PASS
- One verb per step; describe outcomes, not selectors.
- Presence ≠ working: assert the thing is correct, not just that a tag exists.
- Keep flows that depend on external state (OAuth, native dialogs, iframes) out of scope.
