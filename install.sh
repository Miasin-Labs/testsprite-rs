#!/usr/bin/env bash
# testsprite-rs installer.
#
# 1. Builds + installs the `testsprite-rs` binary to ~/.cargo/bin.
# 2. Registers it as an MCP server ("testsprite") in Codex CLI's config.toml.
# 3. Installs the two agent skills as Codex prompts (/testsprite-onboard, /testsprite-verify).
#
# Idempotent: safe to re-run. Pass --discord to also build the Discord bot binary.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
BIN="$CARGO_BIN/testsprite-rs"

FEATURES=""
for arg in "$@"; do
  [ "$arg" = "--discord" ] && FEATURES="--features discord"
done

echo "==> installing testsprite-rs binary ${FEATURES:+($FEATURES)}"
# shellcheck disable=SC2086
cargo install --path "$REPO" --force $FEATURES

echo "==> registering MCP server in $CODEX_HOME/config.toml"
CFG="$CODEX_HOME/config.toml"
mkdir -p "$CODEX_HOME"
if [ -f "$CFG" ] && grep -q '^\[mcp_servers\.testsprite\]' "$CFG"; then
  echo "    [mcp_servers.testsprite] already present — leaving as-is"
else
  [ -f "$CFG" ] && cp "$CFG" "$CFG.bak-testsprite-$(date +%Y%m%d-%H%M%S)"
  cat >> "$CFG" <<EOF

[mcp_servers.testsprite]
command = "$BIN"
args = ["serve"]
startup_timeout_sec = 60.0
EOF
  echo "    added [mcp_servers.testsprite]"
fi

echo "==> installing skills as Codex prompts"
mkdir -p "$CODEX_HOME/prompts"
cp "$REPO/skills/testsprite-onboard.md" "$CODEX_HOME/prompts/testsprite-onboard.md"
cp "$REPO/skills/testsprite-verify.md" "$CODEX_HOME/prompts/testsprite-verify.md"
echo "    /testsprite-onboard and /testsprite-verify are now available in Codex"

echo
echo "Done. Restart Codex; the 'testsprite' MCP tools + skills are ready."
echo "Verify the environment any time with: testsprite-rs doctor"
