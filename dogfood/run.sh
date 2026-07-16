#!/usr/bin/env bash
# Dogfood: testsprite-rs green-gates its OWN repo through its OWN command
# executor — the ultimate self-test. Imports the stored quality-gate tests
# (dogfood/tests.json) and runs them deterministically: no LLM, no cloud,
# just the `command` executor shelling out to cargo in the repo root.
#
# Exit 0 iff build + unit tests + clippy all pass; non-zero otherwise
# (testsprite-rs test run exits 1 on any FAIL).
#
# Override the binary with TESTSPRITE_RS=/path/to/testsprite-rs (defaults to
# the one on PATH). The SQLite store lives under testsprite_tests/ (gitignored);
# import is idempotent (upsert by id), so re-running is safe.
set -euo pipefail
cd "$(dirname "$0")/.."
BIN="${TESTSPRITE_RS:-testsprite-rs}"

"$BIN" test import dogfood/tests.json
exec "$BIN" test run --id df-build --id df-test --id df-clippy --id df-generation-regressions
