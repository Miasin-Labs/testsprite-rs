# TestSprite V3 CLI contract vs `testsprite-rs` (2026-07-15, grounded)

How this was captured (no guessing):

1. **Static** — the installed `@testsprite/testsprite-cli@0.3.0` ships compiled-TS
   `.d.ts` files that *are* the wire contract. Mined
   `dist/commands/{test,project,auth,doctor}.d.ts` and
   `dist/lib/{runs.types,facade,junit-report,pagination}.d.ts`.
2. **Live (read-only, real key)** — a handful of **GET-only** calls with the real
   `sk-user-…` key at `~/.testsprite/credentials`: `auth status`, `usage`,
   `project list`, `test list`, `test result --include-analysis`, `test code get`.
   **No** test was created or run (each run costs 2 credits; account had 141.2,
   Free plan). We do **not** depend on any of this at runtime — the goal is a
   self-hostable/local backend whose *output shapes match* this contract so the
   same agents/CI can point at either.

The CLI talks to **one facade**: base `https://api.testsprite.com`, path prefix
**`/api/cli/v1`** (not the dashboard's `/v3/*` plane). Auth is the `sk-user-` key
as a bearer; pagination is `{ items: [...], nextToken }`.

---

## Real live outputs (redacted)

`auth status` / `usage` → `GET /api/cli/v1/me`:

```json
{ "userId":"…","keyId":"key_…","scopes":["read:projects","read:tests","read:me",
  "write:tests","run:tests","write:projects"], "env":"production",
  "email":"…","displayName":"…","credits":141.2,"creditsPerRun":2,"subPlan":"Free" }
```

`test result <id> --include-analysis` → `CliLatestResult` (a *passing* backend test):

```json
{ "testId":"e384…","status":"passed","startedAt":null,
  "finishedAt":"2026-07-15T00:03:34.892Z","videoUrl":null,"failureAnalysisUrl":null,
  "snapshotId":"snap_…","runIdIfAvailable":"6dfd…","codeVersion":"v1",
  "targetUrl":null,"targetUrlSource":null,"failedStepIndex":null,"failureKind":null,
  "verdict":"passed","executionStatus":"completed","summary":"Test passed.",
  "analysis":{ "rootCauseHypothesis":null,"recommendedFixTarget":null,
               "failureKind":null,"snapshotId":"snap_…" } }
```

`test code get <id>` → their generated **backend** test (the actual artifact):
python + pytest + `requests`, with an **auth-credential preamble**
(`__AUTH_CREDENTIAL__` read from env, redacted in transit), a **security-intent
docstring**, `BASE_URL` = the run's tunnel URL, real assertions with rich failure
messages (`status in {401,403}`, "leaked an email"), and a runnable
`if __name__ == "__main__"`.

---

## The contract shapes that matter (from `.d.ts`)

- **Verdict** (outcome): `passed | failed | blocked`. Separate from **execution
  status** (lifecycle): `draft|ready|queued|running|completed|cancelled|unknown`.
- **`failureKind`**: `assertion | assertion_blocked | routing_404 | network_timeout
  | network | timeout | browser_crash | infra | unknown | null`.
- **`fixKind`** (`recommendedFixTarget.kind`): `code | selector | data | env | unknown`.
- **`CliLatestResult`**: `{testId,status,verdict,executionStatus,failureKind,
  summary,snapshotId,targetUrl,finishedAt,analysis?{rootCauseHypothesis,
  recommendedFixTarget{kind,reference,rationale},failureKind}}`.
- **`CliRunDiff`** (`test diff runA runB`): `{runA{…},runB{…},verdictChanged,
  failedStepIndexChanged,failureKindChanged,codeVersionChanged,crossTest,
  changedSteps[{stepIndex,statusA,statusB,errorA?,errorB?}]}`. Exit **0 when
  verdicts match, 1 when they differ** (CI-scriptable).
- **`CliLintReport`** (`test lint`): `{checked,valid,issues[{file,field,reason}]}`.
  Exit **0 valid, 5 (VALIDATION_ERROR) otherwise**. Fully offline.
- **`test scaffold --type backend`** → `{type:"backend",language:"python",code}`.
- **`DoctorReport`**: `{checks[{name,status:ok|warn|fail,…}]}`.
- **Exit codes**: `0` pass · `1` fail/blocked · `5` VALIDATION_ERROR · `6` stale
  etag · `7` timeout/inconclusive · `11` RATE_LIMITED.
- **Project**: `CliProject{id,name,type:frontend|backend,createdFrom,createdAt,
  updatedAt}`; create body `{type,name,targetUrl?,description?,username?,password?,
  instruction?}`.

---

## Field-by-field: `testsprite-rs` local vs V3 CLI

`run_collect` emits per test: `{id,title,passed:bool,error,analysis?{fixKind,
verdict,cause,fix},fixPath?}` and `store::write_result` persists `{id,passed,
error,code,analysis?}`.

| Concept | V3 CLI | testsprite-rs today | Gap / action |
|---|---|---|---|
| outcome | `verdict: passed\|failed\|blocked` | `passed: bool` | **add 3-valued verdict** (blocked = couldn't run: conn-refused/timeout/infra) — local, deterministic |
| failure taxonomy | `failureKind` (9-enum) | none (free-text `error`) | **derive `failureKind` locally** from status code / error text (routing_404, network_timeout, network, timeout, assertion…) |
| fix routing | `fixKind: code\|selector\|data\|env\|unknown` | ✅ `analysis.fixKind` **already this enum** | keep — already contract-aligned |
| root cause | `analysis.rootCauseHypothesis` | `analysis.cause` | rename/alias in `--json` output |
| result field names | `testId`,`summary` | `id` | offer a `--json` that emits `CliLatestResult` shape (drop-in) |
| `test diff` exit | 0 match / **1 differ** | **always 0** | **fix: exit 1 when verdicts differ** + optional `CliRunDiff` json |
| `test lint` exit | 0 / **5** | **1** | **align to exit 5** + `CliLintReport{checked,valid,issues}` json |
| `test scaffold` | present | none (engine synthesizes) | **add `test scaffold --type backend\|frontend`** |
| generated BE code | auth preamble + docstring + runnable main | LLM/engine body | upgrade prompt to inject auth-env preamble + intent docstring |
| `doctor` | `{checks[{name,status}]}` | ✅ 7 checks (text) | add `--json` (DoctorReport shape) — optional |
| junit | sidecar for batch | ✅ `gate` writes junit.xml | aligned |
| pagination | `{items,nextToken}` | list prints all | fine for local (no paging needed) |

### Local-honest gaps to close (self-hostable, no cloud, no key, no deps)
1. **Verdict + failureKind vocabulary** in local run output — makes local JSON
   agent-routable exactly like the real CLI.
2. **`test diff` exit 1 on verdict mismatch** (+ `--json` `CliRunDiff` shape).
3. **`test lint` exit 5** on hard issues (+ `--json` `CliLintReport` shape).
4. **`test scaffold`** verb (backend python + requests starter; frontend plan json).
5. (opt) Upgrade the backend-codegen prompt to match their auth-preamble + docstring style.

### Deliberately NOT built (requires the cloud account / `/v3/*` — you said no)
Orgs/multi-tenancy, agent conversations, project "resources" (GitHub/Linear/design/
crawl), Slack/Jira integrations, monitoring/cron schedules, artifact/video download,
presigned code URLs, credit metering, the AppSync GraphQL plane. These are
network-only; local shells would be non-functional stubs.
