//! Discord front-end for the conversational test agent (`--features discord`).
//!
//! A thin [serenity](https://docs.rs/serenity) shell over [`crate::local::agent`]:
//! a Discord message becomes `agent::message()`, each proposed action renders as a
//! ✅ Approve / ❌ Reject button, and a button click calls `agent::resolve()`.
//! One Discord channel maps to one conversation thread (the channel id is the
//! conversation id), so the whole approval-gated loop happens in chat.
//!
//! The bot token is read from a file (never inlined in config, never logged).

use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow};
use serde::Deserialize;
use serenity::async_trait;
use serenity::builder::{
    CreateButton, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
    EditInteractionResponse,
};
use serenity::model::prelude::*;
use serenity::prelude::*;

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
}

impl Handler {
    /// Build the reply message for an `agent::message` result: the assistant text
    /// plus one Approve/Reject button pair per proposed action.
    fn reply_message(&self, conv: &str, v: &serde_json::Value) -> CreateMessage {
        let assistant = v["assistant"].as_str().unwrap_or("(no reply)");
        let mut builder = CreateMessage::new().content(assistant);
        if let Some(actions) = v["pendingActions"].as_array() {
            for a in actions {
                let aid = a["id"].as_i64().unwrap_or_default();
                let kind = a["kind"].as_str().unwrap_or("action");
                let summary = a["summary"].as_str().unwrap_or(kind);
                let label = format!("✅ {}", truncate(summary, 74));
                builder = builder
                    .button(
                        CreateButton::new(format!("approve:{conv}:{aid}"))
                            .label(label)
                            .style(ButtonStyle::Success),
                    )
                    .button(
                        CreateButton::new(format!("reject:{conv}:{aid}"))
                            .label("❌ Reject")
                            .style(ButtonStyle::Danger),
                    );
            }
        }
        builder
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!(
            "discord: connected as {} — agent root {}",
            ready.user.name,
            self.cfg.root.display()
        );
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        let content = msg.content.trim();
        let text = match content.strip_prefix(self.cfg.prefix.trim()) {
            Some(rest) => rest.trim().to_string(),
            None if msg.guild_id.is_none() => content.to_string(), // DMs need no prefix
            None => return,
        };
        if text.is_empty() {
            return;
        }

        let conv = msg.channel_id.to_string();
        match crate::local::agent::message(&self.cfg.root, Some(&conv), &text, &self.cfg.model).await
        {
            Ok(v) => {
                let builder = self.reply_message(&conv, &v);
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
        let Interaction::Component(c) = interaction else {
            return;
        };
        // custom_id = "approve|reject:<conv>:<action_id>"
        let parts: Vec<&str> = c.data.custom_id.splitn(3, ':').collect();
        let [verb, conv, aid] = parts.as_slice() else {
            return;
        };
        let Ok(action_id) = aid.parse::<i64>() else {
            return;
        };
        let approve = *verb == "approve";

        // Ack within Discord's 3s window (resolve may run an LLM + the pipeline).
        if c.create_response(&ctx.http, CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new(),
        ))
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
            Ok(v) => v["assistant"].as_str().unwrap_or("done").to_string(),
            Err(e) => format!("error: {e}"),
        };
        let _ = c
            .edit_response(&ctx.http, EditInteractionResponse::new().content(truncate(&content, 1900)))
            .await;
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Connect to Discord and run the agent bot until the process is stopped.
pub async fn run(token_override: Option<String>) -> Result<()> {
    let (token, cfg) = load(token_override)?;
    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;
    let mut client = Client::builder(&token, intents)
        .event_handler(Handler { cfg })
        .await
        .context("building the Discord client")?;
    client.start().await.context("Discord client error")?;
    Ok(())
}
