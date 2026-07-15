# Agent skills

Skills the coding agent loads to drive testsprite-rs.

## Parity with the official tool
- The **MCP plugin** (`@testsprite/testsprite-mcp`) ships **zero** skills — skills
  are a **CLI-only** feature (`testsprite agent install`).
- The official **CLI** (`@testsprite/testsprite-cli`) ships exactly **two**:
  `testsprite-onboard` and `testsprite-verify`.

## What's here
- `upstream/testsprite-onboard.skill.md`, `upstream/testsprite-verify.skill.md`
  — the **byte-exact upstream** skills, for reference/parity. They target the
  **cloud** CLI (`testsprite project create`, credits, a deployed URL) and are
  **not** runnable against testsprite-rs as written.
- `testsprite-onboard.md`, `testsprite-verify.md` — the **testsprite-rs local
  adaptations** (SQLite, no cloud, our `test generate|run|coverage|gate` commands
  and the `testsprite_local_*` MCP tools). These are what the installer ships.

Edit the local versions freely — they're plain Markdown.

## Install
```bash
./install.sh          # cargo install + register the MCP server with Codex + copy skills
```
This installs the binary, adds `[mcp_servers.testsprite]` to `~/.codex/config.toml`,
and copies the two local skills to `~/.codex/prompts/` (invoke as
`/testsprite-onboard` and `/testsprite-verify` in Codex).
