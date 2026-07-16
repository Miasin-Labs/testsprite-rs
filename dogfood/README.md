# Dogfood: testsprite-rs testing testsprite-rs

testsprite-rs green-gates its **own** repository through its own `command`
executor — the strongest self-test there is. The stored tests are deterministic:
no OpenAI key, no cloud, no tunnel. Each is a `command`-kind case that shells out
to cargo in the repo root and passes on exit 0.

## Run it

```bash
./dogfood/run.sh
# or, explicitly:
cargo install --path . --force        # get the current binary on PATH
testsprite-rs test import dogfood/tests.json
testsprite-rs test run                # exits non-zero if any gate fails
```

## What it checks

| id          | gate                                            |
|-------------|-------------------------------------------------|
| `df-build`  | `cargo build --quiet`                           |
| `df-test`   | `cargo test --quiet` (in-crate `#[cfg(test)]`)  |
| `df-clippy` | `cargo clippy --all-targets -- -D warnings`     |
| `df-generation-regressions` | focused generation/auth-flow regression tests |

`dogfood/tests.json` is the committed, reproducible source of truth (the
`test import` format). The SQLite store it imports into (`testsprite_tests/`)
is gitignored and ephemeral — import is an idempotent upsert by id.

## Coverage gaps

See which functions in the source surface no stored test references yet:

```bash
testsprite-rs coverage --gaps --path src
```

This is a structural (tree-sitter) surface intersection, not line coverage —
it points the next test at the uncovered functions.
