# testsprite-rs gap analysis (current) — vs TestSprite + competitors

Grounded in: the installed CLI's `.d.ts` contract, the mindemon dump beautified
with **`jsbeautify -d --rename-vars`** (oxc-powered; chunks `2642` plan-gating,
`2787` schedules/resources, `4116` schedule hooks), the 530 `use-cases/compare_*`
SEO pages (`research/`), and the live V3 API probe.

> This supersedes `research/COMPARE-ANALYSIS.md`, which was written at hour 1 and
> is now stale — the "intelligence" column it marked ❌ is almost entirely ✅.

## Closed this session (COMPARE-ANALYSIS.md marked these ❌)
| capability | now | where |
|---|---|---|
| LLM test-code generation in the CLI | ✅ | `test generate`, `testsprite_local_generate` |
| Failure classification (bug vs fragility) | ✅ | `verdict::classify`, `llm::analyze_failure` |
| Root-cause analysis | ✅ | `analysis.cause` |
| Autonomous fix recommendations | ✅ | `test run --fix` → `fixes/<id>.md` |
| Auto-heal / self-heal on rerun | ✅ | `test rerun --heal` (fragility only) |
| GitHub App / PR gating | ✅ | `gate` (JUnit + JSON + gh PR comment) |
| Cross-browser + visual diffing | ✅ | `test run --browser`, `visual` |
| Exit-code / `--json` contract | ✅ | verdict/failureKind, `--json`, exit 0/1/5 |
| `test lint` / `diff` / `scaffold` (offline verbs) | ✅ | all local |

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
- **Coverage Guard + `coverage_gaps`** — declared-vs-exercised surface + the uncovered-
  function loop that drives "generate the missing ones until empty."
- **`store_test` + `command` kind** — the host coding agent writes the test, we run it
  deterministically (repo-native `cargo test …`), no second LLM, no cloud.
- **SQLite/sqlx durable store** with append-only run history.
- **Discord bot** front-end over the same approval-gated agent engine.
- **Single offline binary**, no node runtime, works with zero OpenAI key on the
  spec/code/command paths.

## Verdict
The parity chase is essentially **done** — every *intelligence* feature TestSprite
markets is built, and the remaining deltas are either (a) small local features worth
adding (Test Lists, flaky, dep waves, `--history`, cron) or (b) cloud SaaS surfaces to
skip. The moat is the three things nobody else has: Coverage Guard, the agent-driven
`store_test`/`command` harness, and fully-local operation.
