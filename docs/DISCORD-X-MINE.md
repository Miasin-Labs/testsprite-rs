# Discord + X signal mining → implemented local features (2026-07-15)

Goal G003: read the TestSprite Discord (via the DiscordMCP JSON-RPC on
`127.0.0.1:3001`) + the X/Twitter scrape, dedup actionable requests against what
`testsprite-rs` already ships, and **implement the genuinely-local missing ones**.

## Sources mined

- Discord channels via DiscordMCP `get_messages`: `feature-requests`
  (1227394621362143323), `cli-contribution` (1519472149629243534),
  `need-support` (1227394621362143326), `general-discussion`
  (1227394621206827116).
- X/Twitter scrape `/tmp/wtf` (@Test_Sprite): CLI open-sourced Jun 11
  (Apache-2.0), Season 3 hackathon closed Jul 10 — marketing, no build signal.

## Deduped signal

| Community ask (who) | Status |
|---|---|
| Decouple generate ↔ execute (jmparsons) | ✅ `store_test` + `generate` + `run` |
| Rename tests / TC000 dupes (jmparsons) | ✅ `test rename` |
| Smart Failure Clustering / root-cause (uxdev) | ✅ `test triage` |
| Custom / own test cases (fiery_kitten) | ✅ `test add` / `store_test` |
| Flaky detection, auth-aware (#199) | ✅ `test flaky` |
| `blocked` while assertions PASS (#208/#221, strider/yazan) | ✅ impossible — deterministic `verdict::classify` |
| Edge-hunting / user-journey (8adah `blocked→24/24`) | ✅ validates onboard skill's edge-hunting rule |
| Mobile / iOS / Electron (hiyo, phantomxd) | ❌ out of scope (cloud roadmap; webkit needs Docker, now supported) |
| Custom hostname / non-localhost (rishi) | ✅ `--url` override |
| Email OTP / Okta auth (screamingtech, fiery_kitten) | ✅ auth → `blocked` (encoded in onboard skill) |

### cli-contribution PRs (what TestSprite valued) → local mapping

| PR | Improvement | Local status |
|---|---|---|
| #48 | bounded pagination | n/a — local store lists all, no cursor |
| #128 | idempotency-key in `--output json` | cloud concept |
| #118 | prompt input buffering | n/a — testsprite-rs has no interactive prompts |
| #90/#114 | AGENTS.md verify-skill detection | install.sh concern; local skills shipped |
| #207 | Windows harness portability | n/a on Linux host |
| **#213** | **`doctor --output` validation** | 🔨 **built this goal** → `doctor --json` |
| `retryFailedAgents` (frontend plan, from G002 JS mine) | rerun only failed | 🔨 **built this goal** → `test rerun --failed` |

## Implemented this goal

Two genuinely-local, community-driven gaps — both built, tested, committed:

1. **`test rerun --failed`** — replay only the tests whose most recent run
   failed (the local analogue of V3's `retryFailedAgents`). New
   `store::last_failed_ids` (latest-run `passed = 0` over the append-only `runs`
   table); `--failed` replaces `--id`; prints `no failed tests to rerun` when the
   board is green. Also fixed `rerun()` to be **project-tolerant**
   (`project::load().ok()`, matching `run()`), so command-kind reruns need no
   project config. Verified: 2 tests (1 pass, 1 fail) → `rerun --failed` replays
   only the red; after fixing it, replays nothing.

2. **`doctor --json`** — emit a `DoctorReport` = `{checks:[{name,status,detail}]}`
   (`status ∈ ok|warn|fail`), matching the V3 `DoctorReport` shape (cli-contribution
   PR #213). Also added a **docker** check (webkit-via-Docker availability).
   Verified: 8 checks, exit 1 iff any check fails.

## Conclusion

Everything else actionable from Discord/X is already shipped (often better —
local + deterministic) or genuinely cloud/out-of-scope. The two new local
features above close the last community-signal gaps that were locally buildable.
Dependency waves (`skipDependencies`) and test lists (`maxTestList`) remain for
G4.
