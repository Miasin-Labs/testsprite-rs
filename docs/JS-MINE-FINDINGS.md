# Frontend JS mining — API/feature signal vs `testsprite-rs` (2026-07-15)

Goal G002: beautify the captured TestSprite frontend JS and extract any API
endpoint, feature flag, tool, or capability **not already** documented in
`docs/V3-CONTRACT.md` / `docs/GAP-ANALYSIS.md`, then implement any
locally-buildable gap. Honesty requirement: if there is no new local signal,
say so.

## Method

Sources mined (already beautified):
- `research/deobf/app-deobf.js` — oxc-deobfuscated app bundle. No `ApiRequest`
  / `/api/` / `/v3/` / `testsprite_` literals (it is the marketing shell).
- `~/VulnerabilityResearch/testspite/dump-0715/pretty/www.testsprite.com/_next/static/chunks/*`
  — the **dashboard** chunks. Every backend call goes through `ApiRequest.{get,post,put,patch,delete}("<path>")`; I extracted the full literal inventory.

## Complete endpoint inventory (dashboard plane, `api.testsprite.com`)

**Account / billing / auth** (`/user/*`, `/billing/*`, `/auth/*`):
`/user/me`, `/user/me/apikey` (GET/POST/DELETE), `/user/me/accept-tos`,
`/user/me/enable-v3`, `/auth/password`, `/auth/password/canReset`,
`/billing/changePlan`, `/billing/cancelDowngrade`, `/billing/portalSession`,
`/feedback/downgradeSurvey`, `/finishGuide`.

**Orgs / multi-tenancy** (`/v3/org/*`):
`/v3/org/billing/usage/timeseries`, `/v3/org/billing/invoices`,
`/v3/org/billing/settings`, `/v3/org/members` (+ PATCH role, DELETE),
`/v3/org/invitations` (+ DELETE), `/v3/org-invitations/{id}/accept`.

**Projects + agent inputs ("resources")** (`/v3/project/*`):
list/create/get/delete, `/strategy/versions`, `/events`, `/creators`,
`/export/github`, `/resource` (+ `/codebase`, `/github-status`,
`/crawled-pages`, `/agent-web-exploration` [+instruction], `/design` [url],
`/ticket` [linearProjectId], `/document`, `/document/upload-url`,
`/latest-frames`, `/env`), `/integration/github/connect`.

**Agentic conversations** (`/v3/agent/*`):
`/conversations` (GET/POST), `/conversations/{id}`,
`/conversations/{id}/assistant-message`, `/conversations/{id}/messages`,
`/conversations/{id}/image-upload-url`,
`/pending-actions/{id}/confirm` (+ overrides), `/pending-actions/{id}/reject`,
`/agent/settings`.

**Test execution** (V2 + project-scoped):
`/v2/frontend-test/{id}/steps` (+ PUT `/steps/{i}`, `/run-from-step/{i}`,
`/run-status`); `/project/{id}/backend/plan-all`, `/backend/rerun`
(`{testIds, skipDependencies}`), `/backend/wizard-run`,
`/backend/{bpId}/test/{id}/{code,run,action}` (instruction);
`/project/{id}/frontend/{fpId}/{credentials,plan}` (`{manualGate,
retryFailedAgents, reconfigure}`), `/frontend/{fpId}/test/{id}/{run,rerun}`
(`{autoHeal}`), `/frontend/{fpId}/agent/{n}/retry`, `/frontend/wizard-run`,
`/frontend/wizard-rerun` (`{autoHeal}`); `/project/{id}/analyze`
(processStatus ∈ Idle|Failed|ReRunFailed), `/project/search`
(q,page,cursor,limit,sort,order,hasType,stats), `/project/{id}/stats`.

**Feature flags**: `GET /feature-flags`, `POST /user/me/enable-v3`,
`testSpriteV3Enabled` (gates `/dashboard` → `/dashboard-v3` redirect).

## Dedup: is any of this NEW and locally-buildable?

| Capability (from JS) | Status in testsprite-rs | Verdict |
|---|---|---|
| Agent conversations + pending-actions **confirm/reject** | ✅ built: `agent::{message,resolve,history}` + `testsprite_agent_*` MCP + Discord ✅/❌ buttons | already built; JS **validates our shape** (conversation → pending action → confirm/reject) |
| `autoHeal` on rerun | ✅ built: `rerun --heal` (fragility-only) | already built |
| `manualGate` before run | ✅ built: agent approval gate | already built |
| `verdict` / `failureKind` / `fixKind` | ✅ built: `verdict::classify` | already built (see V3-CONTRACT.md) |
| **`/backend/rerun {skipDependencies}`** | ⏳ **G4 target** (dependency waves `--produces`/`--needs`) | confirms G4 is real; not new |
| **`/frontend/plan {retryFailedAgents}`** (rerun only failed) | ⏳ **G4-forward** — local `test rerun --failed` (replay only tests whose latest run failed, read from the `runs` table) | locally buildable, not yet built |
| **`maxTestList` / `maxTestListSize`** (from `/user/me`) | ⏳ **G4 target** (test lists / grouping) | confirms G4 test-lists; not new |
| `/project/search`, `/stats`, sort/order/filter | list-all locally (no paging) | cloud UX; local `test list` covers it |
| Orgs / billing / invoices / members / invitations | ❌ deliberately not built (cloud SaaS) | cloud-only, per V3-CONTRACT.md |
| Project "resources" (GitHub/Linear/design/crawl/env/docs) | ❌ deliberately not built | cloud-only |
| `/v2/frontend-test/*/steps`, `run-from-step`, step recording | ❌ not built (no server-side step store) | cloud-only; local browser executor is whole-script |
| `/integration/github/connect`, image upload, presigned URLs | ❌ not built | cloud-only SaaS plumbing |
| `/feature-flags`, `enable-v3`, `testSpriteV3Enabled` | n/a | cloud rollout gating |

## Conclusion (honest)

**No new locally-buildable signal.** Every capability in the beautified frontend
JS maps to one of three buckets:
1. **Already built** — agent conversations (with confirm/reject!), autoHeal,
   manual gate, verdict/failureKind. The JS actively *validates* our local
   agent-conversation design: their `/v3/agent/conversations` +
   `/pending-actions/{id}/{confirm,reject}` is the exact shape of our local
   `agent::message` → pending action → `agent::resolve`.
2. **Already planned (G4)** — `/backend/rerun {skipDependencies}` (dependency
   waves), `/frontend/plan {retryFailedAgents}` (a local `test rerun --failed`
   that replays only last-failed tests from the `runs` table), and `maxTestList`
   (test lists/grouping) are all locally buildable and carried into G4.
3. **Cloud-only SaaS, deliberately excluded** — orgs, billing, project
   resources, integrations, step recording, feature flags. Local shells would be
   non-functional stubs (already stated in V3-CONTRACT.md's "Deliberately NOT
   built" section).

Nothing to implement under G002. The one actionable confirmation —
`skipDependencies` on rerun — is carried into G4 (BE dependency waves).

## Deep deobfuscation pass (2026-07-15) — confirms the above

Re-mined with `jsbeautify -d --rename-vars` (oxc) over **all** app-logic chunks,
including the 186 KB `9da6db1e` the first pass skipped, then searched the
readable output. Result: **no new endpoints, no new locally-buildable signal.**
`9da6db1e` is mostly the embedded **PostHog analytics SDK**; the rest is cloud
workflow (`/backend/plan-all`, `/wizard-run`, `processStatus` Executing/Failed
lifecycle, `pollInterval:2000`, `testPlan` PUT, server-side generation).

Positive validations of already-shipped local features:
- `maxTestList` / `maxSchedule` are real plan-gated product limits → local
  **test lists** (`test list/run --group`) + **schedules** mirror real features.
- `retryFailedAgents` → local `test rerun --failed`; `skipDependencies` → local
  dependency waves; `autoHeal` → `rerun --heal`. All built.

The dump (`~/VulnerabilityResearch/testspite/dump-0715`, target
`dashboard/settings/apikey`) is fully handled: present, extracted (479/479), and
mined twice. Nothing to build from it.

Lazy-chunk fetch (`curl_chrome120`, 2026-07-15): pulled the 9 dynamically-
importable chunks in the webpack `.u` map that were missing from the dump — all
third-party libs (Monaco editor, recharts, react-redux), **zero API/product
signal**. The API wrappers live in the already-mined shared chunks; the only
un-fetchable chunks are auth-gated dashboard route UIs that call the known
endpoints. Mining is complete: no new locally-buildable signal.
