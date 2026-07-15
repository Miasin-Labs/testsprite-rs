//! Discord front-end for the conversational test agent (`--features discord`).
//!
//! A thin [serenity](https://docs.rs/serenity) shell over [`crate::local::agent`].
//! You can reach the same engine four ways: a message starting with the
//! configured prefix (default `ts `), an @mention of the bot, the
//! `/testsprite <message>` slash command, or any message in a DM.
//! Each proposed action renders as a ✅ Approve / ❌ Reject button; a click calls
//! `agent::resolve()`. One Discord channel maps to one conversation thread.
//!
//! The bot token is read from a file (never inlined in config, never logged).

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{Context as _, Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use serenity::async_trait;
use serenity::builder::{
    CreateActionRow, CreateButton, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateMessage, EditInteractionResponse,
};
use serenity::model::prelude::*;
use serenity::prelude::*;

const SLASH_COMMAND: &str = "testsprite";

/// Runtime settings for the bot (everything except the secret token).
#[derive(Clone)]
struct BotConfig {
    prefix: String,
    model: String,
    root: PathBuf,
}

#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    discord: Option<DiscordSection>,
}

#[derive(Debug, Default, Deserialize)]
struct DiscordSection {
    token_file: Option<String>,
    prefix: Option<String>,
    model: Option<String>,
    root: Option<String>,
}

fn config_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(".config/testsprite")
}

/// Resolve `(token, settings)`. Token order: explicit override → `TESTSPRITE_DISCORD_TOKEN`
/// → config `token_file` → `~/.config/testsprite/token.key` → `./token.key`.
/// The token value is never printed or logged.
fn load(token_override: Option<String>) -> Result<(String, BotConfig)> {
    let file: FileConfig = std::fs::read_to_string(config_dir().join("config.toml"))
        .ok()
        .map(|s| toml::from_str(&s))
        .transpose()
        .context("parsing ~/.config/testsprite/config.toml")?
        .unwrap_or_default();
    let d = file.discord.unwrap_or_default();

    let token = read_token(token_override, d.token_file.as_deref())?;

    let cfg = BotConfig {
        prefix: d.prefix.unwrap_or_else(|| "ts ".to_string()),
        model: d.model.unwrap_or_else(|| "gpt-4o-mini".to_string()),
        root: d
            .root
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(std::env::current_dir)
            .context("resolving agent root")?,
    };
    Ok((token, cfg))
}

fn read_token(token_override: Option<String>, token_file: Option<&str>) -> Result<String> {
    if let Some(t) = token_override.filter(|t| !t.trim().is_empty()) {
        return Ok(t.trim().to_string());
    }
    if let Ok(t) = std::env::var("TESTSPRITE_DISCORD_TOKEN")
        && !t.trim().is_empty()
    {
        return Ok(t.trim().to_string());
    }
    let candidates = [
        token_file.map(PathBuf::from),
        Some(config_dir().join("token.key")),
        Some(PathBuf::from("token.key")),
    ];
    for path in candidates.into_iter().flatten() {
        if let Ok(t) = std::fs::read_to_string(&path)
            && !t.trim().is_empty()
        {
            return Ok(t.trim().to_string());
        }
    }
    Err(anyhow!(
        "no Discord token: set TESTSPRITE_DISCORD_TOKEN, config [discord].token_file, or token.key"
    ))
}

struct Handler {
    cfg: BotConfig,
    bot_id: OnceLock<UserId>,
}

/// ✅ Approve / ❌ Reject rows, one pair per proposed action. Reused by message
/// replies (`CreateMessage`) and slash responses (`EditInteractionResponse`).
fn action_rows(conv: &str, v: &Value) -> Vec<CreateActionRow> {
    let mut rows = Vec::new();
    if let Some(actions) = v["pendingActions"].as_array() {
        for a in actions {
            let aid = a["id"].as_i64().unwrap_or_default();
            let kind = a["kind"].as_str().unwrap_or("action");
            let summary = a["summary"].as_str().unwrap_or(kind);
            rows.push(CreateActionRow::Buttons(vec![
                CreateButton::new(format!("approve:{conv}:{aid}"))
                    .label(format!("✅ {}", truncate(summary, 74)))
                    .style(ButtonStyle::Success),
                CreateButton::new(format!("reject:{conv}:{aid}"))
                    .label("❌ Reject")
                    .style(ButtonStyle::Danger),
            ]));
        }
    }
    rows
}

/// Strip leading `<@id>` / `<@!id>` mention tokens from message content.
fn strip_mentions(content: &str) -> String {
    content
        .split_whitespace()
        .filter(|t| !(t.starts_with("<@") && t.ends_with('>')))
        .collect::<Vec<_>>()
        .join(" ")
}

const HELP: &str = "I'm the testsprite agent. Say `ts generate a test that …` or `ts run them`, \
mention me, or use `/testsprite`. I'll propose an action and you approve it with the buttons.";

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        let _ = self.bot_id.set(ready.user.id);
        tracing::info!(
            "discord: connected as {} — agent root {}",
            ready.user.name,
            self.cfg.root.display()
        );

        // Register the slash command per-guild (instant) in every guild we're in.
        let cmd = CreateCommand::new(SLASH_COMMAND)
            .description("Ask the local test agent to generate or run tests")
            .add_option(
                CreateCommandOption::new(
                    CommandOptionType::String,
                    "message",
                    "what you want the agent to do",
                )
                .required(true),
            );
        for g in &ready.guilds {
            match g.id.create_command(&ctx.http, cmd.clone()).await {
                Ok(_) => tracing::info!("discord: registered /{SLASH_COMMAND} in guild {}", g.id),
                Err(e) => tracing::warn!(
                    "discord: /{SLASH_COMMAND} registration failed in guild {} \
                     (invite the bot with the applications.commands scope): {e}",
                    g.id
                ),
            }
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        let content = msg.content.trim();
        let mentioned = self
            .bot_id
            .get()
            .is_some_and(|id| msg.mentions.iter().any(|u| u.id == *id));

        let text = match content.strip_prefix(self.cfg.prefix.trim()) {
            Some(rest) => rest.trim().to_string(),
            None if mentioned => strip_mentions(content).trim().to_string(),
            None if msg.guild_id.is_none() => content.to_string(), // DMs need no prefix
            None => return,
        };
        if text.is_empty() {
            let _ = msg.channel_id.say(&ctx.http, HELP).await;
            return;
        }

        let conv = msg.channel_id.to_string();
        tracing::info!("discord: message in {conv}: {}", truncate(&text, 80));
        match crate::local::agent::message(&self.cfg.root, Some(&conv), &text, &self.cfg.model).await
        {
            Ok(v) => {
                let builder = CreateMessage::new()
                    .content(reply_text(&v))
                    .components(action_rows(&conv, &v));
                if let Err(e) = msg.channel_id.send_message(&ctx.http, builder).await {
                    tracing::warn!("discord: send failed: {e}");
                }
            }
            Err(e) => {
                let _ = msg.channel_id.say(&ctx.http, format!("agent error: {e}")).await;
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(c) => self.handle_slash(&ctx, c).await,
            Interaction::Component(c) => self.handle_button(&ctx, c).await,
            _ => {}
        }
    }
}

impl Handler {
    /// `/testsprite <message>` — same engine as a plain message.
    async fn handle_slash(&self, ctx: &Context, c: CommandInteraction) {
        if c.data.name != SLASH_COMMAND {
            return;
        }
        let message = c
            .data
            .options()
            .into_iter()
            .find(|o| o.name == "message")
            .and_then(|o| match o.value {
                ResolvedValue::String(s) => Some(s.to_string()),
                _ => None,
            })
            .unwrap_or_default();

        // Ack within Discord's 3s window (agent may call the LLM).
        if c.create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
        )
        .await
        .is_err()
        {
            return;
        }

        let conv = c.channel_id.to_string();
        tracing::info!("discord: /{SLASH_COMMAND} in {conv}: {}", truncate(&message, 80));
        let edit = match crate::local::agent::message(
            &self.cfg.root,
            Some(&conv),
            &message,
            &self.cfg.model,
        )
        .await
        {
            Ok(v) => EditInteractionResponse::new()
                .content(reply_text(&v))
                .components(action_rows(&conv, &v)),
            Err(e) => EditInteractionResponse::new().content(format!("agent error: {e}")),
        };
        let _ = c.edit_response(&ctx.http, edit).await;
    }

    /// A ✅/❌ button press → `agent::resolve`.
    async fn handle_button(&self, ctx: &Context, c: ComponentInteraction) {
        // custom_id = "approve|reject:<conv>:<action_id>"
        let parts: Vec<&str> = c.data.custom_id.splitn(3, ':').collect();
        let [verb, conv, aid] = parts.as_slice() else {
            return;
        };
        let Ok(action_id) = aid.parse::<i64>() else {
            return;
        };
        let approve = *verb == "approve";
        tracing::info!("discord: {verb} action {action_id} in {conv}");

        if c.create_response(
            &ctx.http,
            CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new()),
        )
        .await
        .is_err()
        {
            return;
        }

        let content = match crate::local::agent::resolve(
            &self.cfg.root,
            conv,
            action_id,
            approve,
            &self.cfg.model,
        )
        .await
        {
            Ok(v) => self.render(&v).await,
            Err(e) => format!("error: {e}"),
        };
        let _ = c
            .edit_response(
                &ctx.http,
                EditInteractionResponse::new().content(truncate(&content, 1900)),
            )
            .await;
    }

    /// Render a resolve result richly: a per-test code-block table for a run, a
    /// generated-test list for a generate, else the plain assistant line.
    async fn render(&self, v: &Value) -> String {
        match v["kind"].as_str() {
            Some("run") => {
                let results: Vec<Value> =
                    v["result"]["results"].as_array().cloned().unwrap_or_default();
                format_run(&results)
            }
            Some("generate") => {
                let mut tests = Vec::new();
                if let Some(ids) = v["result"]["ids"].as_array() {
                    for id in ids.iter().filter_map(Value::as_str) {
                        let title = crate::local::store::load_one(&self.cfg.root, id)
                            .await
                            .map(|t| t.title)
                            .unwrap_or_default();
                        tests.push((id.to_string(), title));
                    }
                }
                format_generate(&tests)
            }
            _ => v["assistant"].as_str().unwrap_or("done").to_string(),
        }
    }
}

/// Assistant text plus a hint when actions are attached.
fn reply_text(v: &Value) -> String {
    v["assistant"].as_str().unwrap_or("(no reply)").to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A fenced-code-block table of per-test outcomes with error + verdict + fix.
fn format_run(results: &[Value]) -> String {
    let total = results.len();
    let passed = results
        .iter()
        .filter(|r| r["passed"].as_bool() == Some(true))
        .count();
    let mut body = String::new();
    let mut budget = 1600usize;
    for r in results {
        let id = r["id"].as_str().unwrap_or("?");
        let title = r["title"].as_str().unwrap_or("");
        let line = if r["passed"].as_bool() == Some(true) {
            format!("PASS  {id}  {}\n", truncate(title, 52))
        } else {
            let fk = r["failureKind"].as_str().unwrap_or("failed");
            let mut s = format!("FAIL  {id}  [{fk}] {}\n", truncate(title, 44));
            if let Some(err) = r["error"].as_str().filter(|e| !e.is_empty()) {
                s.push_str(&format!("      error: {}\n", truncate(err, 100)));
            }
            if let Some(cause) = r["analysis"]["cause"].as_str().filter(|c| !c.is_empty()) {
                s.push_str(&format!("      cause: {}\n", truncate(cause, 100)));
            }
            if let Some(fix) = r["analysis"]["fix"].as_str().filter(|f| !f.is_empty()) {
                s.push_str(&format!("      fix:   {}\n", truncate(fix, 100)));
            }
            s
        };
        if line.len() > budget {
            body.push_str("      … (more)\n");
            break;
        }
        budget -= line.len();
        body.push_str(&line);
    }
    format!(
        "**Ran {total} · {passed} passed · {} failed**\n```\n{body}```",
        total - passed
    )
}

/// A bullet list of the tests a generate action produced.
fn format_generate(tests: &[(String, String)]) -> String {
    let mut out = format!("**Generated {} test(s):**\n", tests.len());
    for (id, title) in tests {
        out.push_str(&format!("• `{id}` — {}\n", truncate(title, 90)));
    }
    truncate(&out, 1900)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn run_table_shows_pass_fail_error_cause_and_fix() {
        let results = vec![
            json!({"id":"TC001","title":"root ok","passed":true}),
            json!({"id":"TC002","title":"root bad","passed":false,"failureKind":"routing_404",
                   "error":"expected 200, got 404",
                   "analysis":{"cause":"the route is missing","fix":"add the handler"}}),
        ];
        let out = format_run(&results);
        assert!(out.contains("1 passed · 1 failed"), "{out}");
        assert!(out.contains("PASS  TC001"));
        assert!(out.contains("FAIL  TC002  [routing_404]"));
        assert!(out.contains("expected 200, got 404"));
        assert!(out.contains("the route is missing"));
        assert!(out.contains("add the handler"));
        assert!(out.contains("```"));
    }

    #[test]
    fn generate_list_shows_ids_and_titles() {
        let out = format_generate(&[("TC001".into(), "First".into()), ("TC002".into(), "Second".into())]);
        assert!(out.contains("Generated 2 test(s)"));
        assert!(out.contains("`TC001` — First"));
        assert!(out.contains("`TC002` — Second"));
    }
}

/// Connect to Discord and run the agent bot until the process is stopped.
pub async fn run(token_override: Option<String>) -> Result<()> {
    let (token, cfg) = load(token_override)?;
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(&token, intents)
        .event_handler(Handler {
            cfg,
            bot_id: OnceLock::new(),
        })
        .await
        .context("building the Discord client")?;
    client.start().await.context("Discord client error")?;
    Ok(())
}
