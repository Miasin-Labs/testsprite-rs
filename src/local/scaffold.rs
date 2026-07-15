//! `testsprite-rs test scaffold` — emit a schema-correct starter test,
//! fully offline. Mirrors the real CLI's `test scaffold --type backend|frontend`
//! output shapes: `{type:"backend",language:"python",code}` and a frontend
//! plan-input JSON.

use serde_json::json;

const BACKEND_TEMPLATE: &str = r#"__AUTH_CREDENTIAL__ = __import__("os").environ.get("TESTSPRITE_AUTH_CREDENTIAL", "")

"""
Security intent: verify the endpoint returns the expected status and does not
leak sensitive data to unauthenticated or malformed requests.
"""

import os

import requests

BASE_URL = os.environ.get("TESTSPRITE_TARGET_URL", "http://127.0.0.1:8080").rstrip("/")


def test_health_check():
    resp = requests.get(f"{BASE_URL}/")
    assert resp.status_code == 200, f"expected 200, got {resp.status_code}: {resp.text[:200]}"


if __name__ == "__main__":
    test_health_check()
    print("ok")
"#;

/// Emit a starter test for `kind` (`backend` | `frontend`). Prints the code
/// (backend) or plan JSON (frontend), as text or as a `--json` object.
/// Returns `0` on success.
pub fn scaffold(kind: &str, json: bool) -> anyhow::Result<i32> {
    match kind {
        "backend" => {
            if json {
                let obj = serde_json::json!({
                    "type": "backend",
                    "language": "python",
                    "framework": "pytest",
                    "code": BACKEND_TEMPLATE,
                });
                println!("{}", serde_json::to_string_pretty(&obj)?);
            } else {
                print!("{BACKEND_TEMPLATE}");
            }
            Ok(0)
        }
        "frontend" => {
            let plan = json!({
                "title": "Frontend smoke test",
                "description": "Verify the app loads and renders successfully.",
                "steps": [
                    {"description": "navigate to the app"},
                    {"description": "assert the page loaded"},
                ],
            });
            // A plan IS JSON; text and --json output are identical here.
            println!("{}", serde_json::to_string_pretty(&plan)?);
            Ok(0)
        }
        other => anyhow::bail!("unknown scaffold type {other:?}; expected backend | frontend"),
    }
}
