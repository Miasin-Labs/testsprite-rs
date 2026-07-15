//! The conversational test agent: a threaded, DB-backed chat that proposes
//! ONE action at a time (generate tests / run tests) which the caller must
//! approve before it executes. Reuses the existing generate/run pipeline;
//! this module is purely the conversation and approval loop on top of it.

use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{generate, run, store};
use crate::server::llm::LlmClient;

/// Send a user message, get back the agent's reply and (at most) one pending
/// action for the caller to approve/reject via [`resolve`].
pub async fn message(
    root: &Path,
    conversation_id: Option<&str>,
    user_msg: &str,
    model: &str,
    auto_approve: bool,
) -> Result<Value> {
    let pool = super::db::open(root).await?;

    let conv_id = match conversation_id {
        Some(id) => {
            let title: String = user_msg.chars().take(60).collect();
            sqlx::query("INSERT OR IGNORE INTO conversations (id,title) VALUES (?,?)")
                .bind(id)
                .bind(&title)
                .execute(&pool)
                .await?;
            id.to_string()
        }
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let title: String = user_msg.chars().take(60).collect();
            sqlx::query("INSERT INTO conversations (id,title) VALUES (?,?)")
                .bind(&id)
                .bind(&title)
                .execute(&pool)
                .await?;
            id
        }
    };

    sqlx::query("INSERT INTO messages (conversation_id,role,content) VALUES (?,'user',?)")
        .bind(&conv_id)
        .bind(user_msg)
        .execute(&pool)
        .await?;

    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT role,content FROM messages WHERE conversation_id=? ORDER BY msg_id")
            .bind(&conv_id)
            .fetch_all(&pool)
            .await?;
    let history: String = rows
        .iter()
        .rev()
        .take(20)
        .rev()
        .map(|(role, content)| format!("{role}: {content}\n"))
        .collect();

    let tests = json!(
        store::list(root)
            .await?
            .iter()
            .map(|t| json!({ "id": t.id, "title": t.title }))
            .collect::<Vec<_>>()
    );

    let decision = decide(model, &history, user_msg, &tests).await;
    let assistant_text = decision["assistant"].as_str().unwrap_or("").to_string();
    let action = decision["action"].clone();
    let kind = action["kind"].as_str().unwrap_or("none").to_string();

    sqlx::query("INSERT INTO messages (conversation_id,role,content) VALUES (?,'assistant',?)")
        .bind(&conv_id)
        .bind(&assistant_text)
        .execute(&pool)
        .await?;

    let mut pending = Vec::new();
    let mut auto_approved = Value::Null;
    if kind == "generate" || kind == "run" {
        let args = serde_json::to_string(&action)?;
        let summary = action["summary"].as_str().unwrap_or("").to_string();
        let action_id: i64 = sqlx::query_scalar(
            "INSERT INTO pending_actions (conversation_id,kind,args,summary) VALUES (?,?,?,?) RETURNING action_id",
        )
        .bind(&conv_id)
        .bind(&kind)
        .bind(&args)
        .bind(&summary)
        .fetch_one(&pool)
        .await?;
        if auto_approve {
            // Mirror TestSprite's agent autoApprove: execute the proposed action
            // immediately instead of waiting for a manual resolve().
            auto_approved = resolve(root, &conv_id, action_id, true, model).await?;
        } else {
            pending.push(json!({
                "id": action_id,
                "kind": kind,
                "summary": summary,
                "args": action,
            }));
        }
    }

    sqlx::query("UPDATE conversations SET updated_at=datetime('now') WHERE id=?")
        .bind(&conv_id)
        .execute(&pool)
        .await?;

    Ok(json!({
        "conversationId": conv_id,
        "assistant": assistant_text,
        "pendingActions": pending,
        "autoApproved": auto_approved,
    }))
}

/// Decide the next action: LLM when a key is available (falling back to the
/// deterministic router on any LLM/parse error), else the deterministic
/// keyword router.
async fn decide(model: &str, history: &str, user_msg: &str, tests: &Value) -> Value {
    let llm = LlmClient::from_env(model);
    if let Some(llm) = &llm
        && let Ok(v) = llm.plan_action(history, user_msg, tests).await
    {
        return v;
    }
    deterministic_route(user_msg, llm.is_some())
}

/// Keyword-based routing used when there is no OpenAI key (or the LLM call
/// failed) — keeps the propose/approve loop fully usable without a key.
fn deterministic_route(user_msg: &str, has_key: bool) -> Value {
    let lower = user_msg.to_lowercase();
    if lower.contains("cover") {
        return json!({
            "assistant": "I'll target currently-uncovered functions with new test cases.",
            "action": {
                "kind": "generate",
                "instruction": user_msg,
                "cover": true,
                "ids": [],
                "summary": "Generate tests for uncovered functions",
            }
        });
    }
    if lower.contains("generate") || lower.contains("create") || lower.contains("write") {
        return json!({
            "assistant": format!("I'll generate test cases for: {user_msg}"),
            "action": {
                "kind": "generate",
                "instruction": user_msg,
                "cover": false,
                "ids": [],
                "summary": format!("Generate tests from: {user_msg}"),
            }
        });
    }
    if lower.contains("run") || lower.contains("execute") {
        return json!({
            "assistant": "I'll run the stored tests.",
            "action": {
                "kind": "run",
                "instruction": user_msg,
                "cover": false,
                "ids": [],
                "summary": "Run all stored tests",
            }
        });
    }
    let assistant = if has_key {
        "Not sure that needs a test action — ask me to generate or run tests.".to_string()
    } else {
        "No OpenAI key set — say 'generate <what>' or 'run' and I'll propose an action.".to_string()
    };
    json!({
        "assistant": assistant,
        "action": { "kind": "none" }
    })
}

/// Approve or reject a pending action by id. Approving executes the
/// underlying generate/run pipeline and records the result.
pub async fn resolve(
    root: &Path,
    conversation_id: &str,
    action_id: i64,
    approve: bool,
    model: &str,
) -> Result<Value> {
    let pool = super::db::open(root).await?;

    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT kind,args,status FROM pending_actions WHERE action_id=? AND conversation_id=?",
    )
    .bind(action_id)
    .bind(conversation_id)
    .fetch_optional(&pool)
    .await?;
    let Some((kind, args_raw, status)) = row else {
        bail!("no pending action {action_id}");
    };
    if status != "pending" {
        bail!("action {action_id} already {status}");
    }

    if !approve {
        sqlx::query("UPDATE pending_actions SET status='rejected' WHERE action_id=?")
            .bind(action_id)
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO messages (conversation_id,role,content) VALUES (?,'assistant','Skipped.')",
        )
        .bind(conversation_id)
        .execute(&pool)
        .await?;
        return Ok(json!({
            "conversationId": conversation_id,
            "actionId": action_id,
            "status": "rejected",
        }));
    }

    let args: Value = serde_json::from_str(&args_raw).context("parsing stored action args")?;

    let (result, summary) = match kind.as_str() {
        "generate" => {
            let out = if args["cover"].as_bool() == Some(true) {
                generate::generate_cover(root, root, model).await?
            } else {
                generate::generate(root, None, args["instruction"].as_str(), None, model, None)
                    .await?
            };
            let summary = format!("Generated {} test(s).", out.test_ids.len());
            (
                json!({ "generated": out.test_ids.len(), "ids": out.test_ids, "prdId": out.prd_id }),
                summary,
            )
        }
        "run" => {
            let ids: Vec<String> = args["ids"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let results = run::run_collect(root, &ids, None, model, false, None, 1, false).await?;
            let passed = results
                .iter()
                .filter(|r| r["passed"].as_bool() == Some(true))
                .count();
            let summary = format!(
                "Ran {} test(s): {} passed, {} failed.",
                results.len(),
                passed,
                results.len() - passed
            );
            (json!({ "results": results }), summary)
        }
        _ => bail!("unknown action kind"),
    };

    let result_str = serde_json::to_string(&result)?;
    sqlx::query("UPDATE pending_actions SET status='applied', result=? WHERE action_id=?")
        .bind(&result_str)
        .bind(action_id)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO messages (conversation_id,role,content) VALUES (?,'assistant',?)")
        .bind(conversation_id)
        .bind(&summary)
        .execute(&pool)
        .await?;

    Ok(json!({
        "conversationId": conversation_id,
        "actionId": action_id,
        "kind": kind,
        "status": "applied",
        "result": result,
        "assistant": summary,
    }))
}

/// Full transcript + pending-action ledger for a conversation.
pub async fn history(root: &Path, conversation_id: &str) -> Result<Value> {
    let pool = super::db::open(root).await?;

    let messages: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT role,content,created_at FROM messages WHERE conversation_id=? ORDER BY msg_id",
    )
    .bind(conversation_id)
    .fetch_all(&pool)
    .await?;
    let messages: Vec<Value> = messages
        .into_iter()
        .map(|(role, content, created_at)| {
            json!({ "role": role, "content": content, "createdAt": created_at })
        })
        .collect();

    let pending: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT action_id,kind,summary,status FROM pending_actions WHERE conversation_id=? ORDER BY action_id",
    )
    .bind(conversation_id)
    .fetch_all(&pool)
    .await?;
    let pending: Vec<Value> = pending
        .into_iter()
        .map(|(action_id, kind, summary, status)| {
            json!({ "id": action_id, "kind": kind, "summary": summary, "status": status })
        })
        .collect();

    Ok(json!({
        "conversationId": conversation_id,
        "messages": messages,
        "pendingActions": pending,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Serializes + neutralizes `OPENAI_API_KEY` / `HOME` (the two key
    /// sources `LlmClient::from_env` checks) for the lifetime of the guard,
    /// so these tests exercise the deterministic no-key path regardless of
    /// the ambient environment. Restores both on drop.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct NoKeyGuard<'a> {
        _lock: std::sync::MutexGuard<'a, ()>,
        key: Option<String>,
        home: Option<String>,
    }

    impl<'a> NoKeyGuard<'a> {
        fn new() -> Self {
            let _lock = ENV_LOCK.lock().unwrap();
            let key = std::env::var("OPENAI_API_KEY").ok();
            let home = std::env::var("HOME").ok();
            unsafe {
                std::env::remove_var("OPENAI_API_KEY");
                std::env::remove_var("HOME");
            }
            Self { _lock, key, home }
        }
    }

    impl Drop for NoKeyGuard<'_> {
        fn drop(&mut self) {
            unsafe {
                match &self.key {
                    Some(k) => std::env::set_var("OPENAI_API_KEY", k),
                    None => std::env::remove_var("OPENAI_API_KEY"),
                }
                match &self.home {
                    Some(h) => std::env::set_var("HOME", h),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    #[tokio::test]
    async fn message_with_no_key_proposes_a_run_action() {
        let _guard = NoKeyGuard::new();
        let root = super::super::tmp_root();
        let out = message(&root, None, "run the tests", "gpt-4o-mini", false)
            .await
            .unwrap();

        let conv_id = out["conversationId"].as_str().unwrap().to_string();
        assert!(!conv_id.is_empty());
        let pending = out["pendingActions"].as_array().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["kind"], "run");

        let pool = super::super::db::open(&root).await.unwrap();
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT role FROM messages WHERE conversation_id=? ORDER BY msg_id")
                .bind(&conv_id)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "user");
        assert_eq!(rows[1].0, "assistant");

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn auto_approve_executes_without_a_pending_action() {
        let _guard = NoKeyGuard::new();
        let root = super::super::tmp_root();
        let out = message(&root, None, "run the tests", "gpt-4o-mini", true)
            .await
            .unwrap();
        // Auto-approved: nothing left pending, and the proposed action executed.
        assert!(out["pendingActions"].as_array().unwrap().is_empty());
        assert!(!out["autoApproved"].is_null());
        assert_eq!(out["autoApproved"]["kind"], "run");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn resolve_reject_then_second_resolve_errors() {
        let _guard = NoKeyGuard::new();
        let root = super::super::tmp_root();
        let out = message(&root, None, "run the tests", "gpt-4o-mini", false)
            .await
            .unwrap();
        let conv_id = out["conversationId"].as_str().unwrap().to_string();
        let action_id = out["pendingActions"][0]["id"].as_i64().unwrap();

        let rejected = resolve(&root, &conv_id, action_id, false, "gpt-4o-mini")
            .await
            .unwrap();
        assert_eq!(rejected["status"], "rejected");

        let err = resolve(&root, &conv_id, action_id, false, "gpt-4o-mini")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already rejected"));

        std::fs::remove_dir_all(&root).ok();
    }
}
