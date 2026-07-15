# testsprite-rs gap analysis (current) — vs TestSprite + competitors

Grounded in: the installed CLI's `.d.ts` contract, the mindemon dump beautified
with **`jsbeautify -d --rename-vars`** (oxc-powered; chunks `2642` plan-gating,
`2787` schedules/resources, `4116` schedule hooks), the 530 `use-cases/compare_*`
SEO pages (`research/`), and the live V3 API probe.

> This supersedes `research/COMPARE-ANALYSIS.md`, which was written at hour 1 and
> is now stale — the "intelligence" column it marked ❌ is almost entirely ✅.

## Closed this session (COMPARE-ANALYSIS.md marked these ❌)

Each row says what the mechanism **actually is**, not just that a verb exists. A
capability whose name promises more than its mechanism delivers is a gap, not a
win — that distinction is what this column is for.

| capability | mechanism | where |
|---|---|---|
| LLM test-code generation in the CLI | ✅ real | `test generate`, `testsprite_generate` |
| Failure classification (bug vs fragility) | ⚠️ **keyword match on the error string**, gated by executor modality so a subprocess's own output can't be mistaken for our transport | `verdict::classify` |
| Root-cause analysis | ⚠️ LLM opinion, unverified | `analysis.cause` |
| Autonomous fix recommendations | ⚠️ **the engine never reads your source** — output is a sketch, not an appliable patch, and is labelled as such | `test run --fix` → `fixes/<id>.md` |
| Auto-heal / self-heal on rerun | ✅ verify-before-persist; prior definition snapshotted to `test_revisions`; rewrites that widen the expectation or drop assertions are rejected | `test rerun --heal`, `test revisions` |
| GitHub App / PR gating | ✅ real | `gate` (JUnit + JSON + gh PR comment) |
| Cross-browser diffing | ✅ real | `test run --browser` |
| Visual diffing | ⚠️ **manual two-file compare** — no baseline store, not wired into a run | `visual` |
| Exit-code / `--json` contract | ✅ real | verdict/failureKind, `--json`, exit 0/1/5 |
| `test lint` / `diff` / `scaffold` (offline verbs) | ✅ all local |

### The pass criterion (the thing everything else rests on)
A deterministic API check passes when the response is in the expected band
(`server::engine::Expect`). Absent an explicit `expect_status`, that band is
`success` (2xx/3xx) — **not** "anything under 500". A route that 404s, sits
behind an auth wall (401/403), or rejects the request (400/405) is a failure.
Writes with a synthesized body get the wider `accepted` band (2xx/3xx + 400/422)
because our body is a guess, but they still fail on 401/403/404/405. The old
lenient behaviour survives only as an explicit opt-in (`"expect_status": "any"`).

This matters more than any feature in the table above: triage clusters this
boolean, flaky scores it, `gate` ships on it. A lenient default here manufactures
confident green over a wall of errors.

## Genuinely still missing — and **buildable locally** (the real "what else")
Evidence from the beautified JS + the CLI parity table:
1. **Test Lists / grouping** (`testlistId` all over chunk `2787`) — name a subset of
   stored tests and run/gate the list. We store tests flat; add a `list` column + `test list --tag`.
2. **Flaky detection** (`test flaky <id>` in the CLI) — replay a test N times, emit a
   stability score. Pure-local, deterministic loop over `run_collect`.
3. **API dependency waves** (`--produces` / `--needs`, `--category teardown`; V3 backend
   wave engine) — producer→consumer ordering with variable passing. We have the `command`
   kind but no scheduler; a topological run order over stored tests is local + real.
4. **Local scheduling / monitoring** (chunk `2787`: `/v3/schedule`, cronExpression,
   timezone, nextRun) — their version is SaaS; ours is a one-liner: `testsprite-rs gate`
   under system cron, or a thin `schedule` verb that writes a crontab entry.
5. **`test flaky`'s sibling — result history/trends** — we now have the append-only `runs`
   table (unique!); surface `test result --history` / a pass-rate trend per test.

## Cloud-only — deliberately NOT built (needs their account/infra)
From chunk `2642` plan-gating (`schedule`, `org_collaboration`, `slack_notification`,
`jira_asana_trigger`) and `2787` (`/v3/project/{id}/slack`, `resource/ticket` Linear,
`resource/document/upload-url`): **scheduled runs as a service, Slack/Jira/Asana/Linear
integrations, orgs/multi-tenancy, per-project file uploads to their storage, ephemeral
cloud sandboxes, the 33-command `/v3` REST CLI.** These are SaaS plumbing, not
intelligence — a local tool shouldn't shell them.

## Unique to testsprite-rs (no competitor or the cloud advertises)
- **Coverage Guard** — declared-vs-exercised API surface (`GET /todos` and
  `POST /todos` are separate elements; a test must evidence the method, not just
  the path).
- **`coverage_gaps`** — see the honest limits below.
- **`store_test` + `command` kind** — the host coding agent writes the test, we run it
  deterministically (repo-native `cargo test …`), no second LLM, no cloud.
- **SQLite/sqlx durable store** with append-only run history and `test_revisions`.
- **Discord bot** front-end over the same approval-gated agent engine.
- **Single offline binary**, no node runtime, works with zero OpenAI key on the
  spec/code/command paths.

## What `coverage_gaps` does and does not measure
`gaps()` prefers **execution**: `cargo llvm-cov --json` (no `--summary-only` —
that flag strips the per-function array) tells us which functions actually ran,
and the report is labelled `evidence: executed`.

Where llvm-cov cannot run (no Cargo.toml, tool absent, the build won't link), it
falls back to matching a function's **name** against the stored tests' text and
labels the report `evidence: named`, with a note. That fallback is weak on
purpose-built terms:
- it is satisfied by a name-drop — typing the name in a title or a shell command
  is indistinguishable from testing it;
- it cannot see your repo's own `cargo test`/pytest suite at all, so a
  thoroughly-tested private function reads "uncovered".

So: treat it as a **to-write worklist, not a coverage measurement**, and do not
chase it to zero. A stored `rust` test is compiled as an integration test
(`tests/<uuid>.rs`), so it can only reach `pub` items — private functions are
unreachable from the DB by construction and are covered by your native tests
instead. Test functions themselves are excluded from the denominator.

## Verdict
Parity on *verbs* is essentially done. The honest remaining work is **mechanism**,
not surface: the ⚠️ rows above are features whose names outrun what they enforce.
The moat is real — Coverage Guard, the agent-driven `store_test`/`command`
harness, fully-local operation — but it is only worth as much as the base signal
underneath it, which is why the pass criterion gets its own section.
