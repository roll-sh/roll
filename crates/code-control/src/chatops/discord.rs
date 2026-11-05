//! Discord (docs/code/chatops.md, "Discord"): the same shape as Slack over Discord's
//! interactions. `/roll prompt:…` is the `discord` trigger's `command`; approvals are buttons and
//! questions selects on request cards; a status card's Reply button opens a modal whose text
//! steers the agent. Discord's bot only sees what's sent to it (no message events without the
//! gateway), so replies go through that modal rather than through the channel.
//!
//! Discord asks for an answer to every interaction within three seconds: the endpoint answers
//! at once (a pong, an ephemeral line, a deferred update or a modal) and records the rest in
//! `chat_actions`, like Slack's.

use super::cards;
use super::follow::{self, Failure, Follow, Surface};
use super::{Handled, Install, Platform};
use crate::Code;
use crate::agents::automations::{NewEvent, OwnerRef, Registry, StepContext, StepKind, StepOutcome, TriggerSource};
use agent_proto::automation::{AutomationEvent, EventInfo, Retry, StepInfo, TriggerInfo};
use code_chat::card::{RequestCard, StatusCard};
use code_chat::discord::{self as wire, Client, Interaction};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Weak};
use std::time::Duration;

pub const SOURCE: &str = "discord";
/// A status card's Reply button and its modal (`roll.steer:<binding>`).
pub(crate) const STEER: &str = "roll.steer:";

pub(super) fn register(code: &Arc<Code>, r: &Registry) {
    let me = Arc::downgrade(code);
    r.register_trigger(Arc::new(DiscordTrigger(me.clone())));
    for kind in [Kind::Post, Kind::Status, Kind::Notify] {
        r.register_step(Arc::new(DiscordStep { kind, code: me.clone() }));
    }
}

fn not_set_up(code: &Weak<Code>, owner: &OwnerRef) -> bool {
    let Some(code) = code.upgrade() else { return true };
    code.discord().is_none() || super::installs(&code, &owner.id, Platform::Discord).map(|i| i.is_empty()).unwrap_or(true)
}

struct DiscordTrigger(Weak<Code>);

#[derive(Debug, Default, Deserialize)]
struct TriggerConfig {
    #[serde(default)]
    guild: Option<String>,
    #[serde(default)]
    channels: Vec<String>,
}

impl TriggerSource for DiscordTrigger {
    fn info(&self) -> TriggerInfo {
        TriggerInfo {
            source: SOURCE.into(),
            title: "Discord".into(),
            description: "/roll in a Discord server.".into(),
            events: vec![EventInfo { kind: "command".into(), title: "/roll <prompt>".into(), fields: ["prompt", "user", "user_name", "channel", "guild"].iter().map(|s| s.to_string()).collect(),
                sample: json!({ "guild": "1100000000000000001", "channel": "1100000000000000002", "user": "1100000000000000003", "user_name": "ada", "prompt": "fix the build", "text": "fix the build" }),
            }],
            config: json!({
                "type": "object",
                "properties": {
                    "guild": { "type": "string", "description": "The server (guild id); unset: the owner's only one." },
                    "channels": { "type": "array", "items": { "type": "string" }, "description": "Channel ids; unset: any." }
                }
            }),
            needs_setup: false,
        }
    }

    fn check(&self, owner: &OwnerRef, events: &[String], config: &Value) -> Result<(), String> {
        let _: TriggerConfig = serde_json::from_value(if config.is_null() { json!({}) } else { config.clone() }).map_err(|e| format!("discord trigger config: {e}"))?;
        if let Some(k) = events.iter().find(|k| k.as_str() != "command") {
            return Err(format!("discord has no event {k}"));
        }
        if not_set_up(&self.0, owner) {
            return Err("Discord isn't connected: connect it in the agents settings".into());
        }
        Ok(())
    }

    fn matches(&self, config: &Value, event: &AutomationEvent) -> bool {
        let c: TriggerConfig = serde_json::from_value(config.clone()).unwrap_or_default();
        let s = |k: &str| event.data[k].as_str().unwrap_or_default();
        c.guild.as_deref().is_none_or(|g| g.is_empty() || g == s("guild")) && (c.channels.is_empty() || c.channels.iter().any(|ch| ch == s("channel")))
    }

    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        not_set_up(&self.0, owner)
    }
}

// ── the interactions endpoint's answers ──

/// What the endpoint answers a verified interaction at once, and whether to record it for the
/// worker.
pub(crate) fn immediate(code: &Code, install: Option<&Install>, i: &Interaction) -> (Value, bool) {
    let say = |t: &str| (wire::respond::message(t, true), false);
    match i {
        Interaction::Ping => (wire::respond::pong(), false),
        Interaction::Other(_) => (wire::respond::deferred_update(), false),
        _ if install.is_none() => say("This server isn't connected to roll. The roll owner connects it in the agents settings."),
        Interaction::Command { user_id, options, .. } => {
            let install = install.expect("checked");
            if !install.allows(user_id) {
                return say("Only people the roll owner allows can use roll here.");
            }
            if options.get("prompt").is_none_or(|p| p.trim().is_empty()) {
                return say("Tell roll what to do: `/roll prompt: fix the flaky login test`.");
            }
            if !super::anyone_listens(code, &install.owner_id, SOURCE, "command") {
                return say("No automation answers this yet. In roll, add one with a Discord trigger.");
            }
            (wire::respond::message("Sent to roll.", true), true)
        }
        Interaction::Component { custom_id, user_id, .. } => {
            let install = install.expect("checked");
            if !install.allows(user_id) {
                return say("Only people the roll owner allows can do this.");
            }
            // Free text and replies open a modal named like the button; its submission is recorded.
            if custom_id.starts_with(wire::ANSWER_PREFIX) {
                return (wire::respond::modal(custom_id, "Answer", "Your answer"), false);
            }
            if custom_id.starts_with(STEER) {
                return (wire::respond::modal(custom_id, "Reply to the agent", "Your message"), false);
            }
            (wire::respond::deferred_update(), true)
        }
        Interaction::ModalSubmit { user_id, .. } => {
            let install = install.expect("checked");
            if !install.allows(user_id) {
                return say("Only people the roll owner allows can do this.");
            }
            (wire::respond::message("Sent to roll.", true), true)
        }
    }
}

/// The interaction's id (the action's key) and guild.
pub(crate) fn ids(i: &Interaction) -> (Option<&str>, Option<&str>) {
    match i {
        Interaction::Command { id, guild_id, .. } | Interaction::Component { id, guild_id, .. } | Interaction::ModalSubmit { id, guild_id, .. } => (Some(id), guild_id.as_deref()),
        _ => (None, None),
    }
}

/// Handle one recorded Discord interaction.
pub(super) async fn handle(code: &Arc<Code>, install: &Install, action: &str, _kind: &str, payload: &str) -> Handled {
    let Some((_, client)) = code.discord() else { return Handled::Retry("Discord isn't configured on this node".into()) };
    let i = match wire::parse_interaction(payload.as_bytes()) {
        Ok(i) => i,
        Err(e) => return Handled::Fail(e),
    };
    match i {
        Interaction::Command { id, guild_id, channel_id, user_id, user_name, options, .. } => {
            let prompt = options.get("prompt").cloned().unwrap_or_default();
            let data = json!({ "guild": guild_id, "channel": channel_id, "user": user_id, "user_name": user_name, "prompt": prompt, "text": prompt });
            let e = NewEvent { source: SOURCE.into(), kind: "command".into(), id: format!("{}:{id}", install.team_id), data, summary: format!("command: {prompt}"), automation: None };
            match code.automation_event(&install.owner_id, e) {
                Ok(_) => Handled::Done,
                Err(e) => Handled::Retry(e.to_string()),
            }
        }
        Interaction::Component { custom_id, values, user_id, user_name, .. } => {
            if let Some((submit, qid)) = wire::question_id(&custom_id) {
                // A select: remembered until Submit (Discord sends one component at a time).
                if let Some((card, _)) = cards::parse_value(submit) {
                    super::set_mark(code, &pick_key(card, qid), &serde_json::to_string(&values).unwrap_or_default());
                }
                return Handled::Done;
            }
            let Some((card, pick)) = cards::parse_value(&custom_id) else { return Handled::Done };
            answer(code, client, install, action, card, pick, &user_id, &user_name).await
        }
        Interaction::ModalSubmit { custom_id, fields, user_id, user_name, .. } => {
            let text = fields.get("roll.text").cloned().unwrap_or_default();
            if let Some(binding) = custom_id.strip_prefix(STEER) {
                let Ok(Some(b)) = super::binding_by_id(code, binding) else { return Handled::Done };
                if b.install_id != install.id || text.trim().is_empty() {
                    return Handled::Done;
                }
                let cmd = json!({ "type": "message.send", "thread": b.thread, "text": format!("{text}\n\n(from {user_name} in Discord)"), "attachments": [], "mode": "steer" });
                return match super::command(code, &b, &format!("discord:steer:{action}"), cmd).await {
                    Ok(_) => Handled::Done,
                    Err((true, why)) => Handled::Retry(why),
                    Err((false, why)) => Handled::Fail(why),
                };
            }
            if let Some((submit, qid)) = wire::question_id(&custom_id)
                && let Some((card, _)) = cards::parse_value(submit)
            {
                super::set_mark(code, &pick_key(card, qid), &serde_json::to_string(&vec![text]).unwrap_or_default());
                // One question: the answer is complete.
                return answer(code, client, install, action, card, "s", &user_id, &user_name).await;
            }
            Handled::Done
        }
        _ => Handled::Done,
    }
}

fn pick_key(card: &str, qid: &str) -> String {
    format!("dq:{card}:{qid}")
}

#[allow(clippy::too_many_arguments)]
async fn answer(code: &Arc<Code>, client: &Client, install: &Install, action: &str, card: &str, pick: &str, user_id: &str, user_name: &str) -> Handled {
    let Ok(Some(row)) = super::card_by_id(code, card) else { return Handled::Done };
    let Ok(Some(b)) = super::binding_by_id(code, &row.binding_id) else { return Handled::Done };
    if b.install_id != install.id || !install.allows(user_id) {
        return Handled::Done;
    }
    let r = match crate::agents::store::mirrored(code.db.sql(), &b.machine_id, agent_proto::EntityKind::Request, &row.request) {
        Ok(Some(agent_proto::Entity::Request(r))) => r,
        Ok(_) => return Handled::Retry("the request isn't in the mirror yet".into()),
        Err(e) => return Handled::Retry(e.to_string()),
    };
    let edit = |c: RequestCard| async move {
        if let Err(e) = client.edit_message(&row.channel, &row.ts, &wire::request(&c)).await {
            tracing::debug!(error = %e, "updating a Discord request card");
        }
    };
    if r.status != agent_proto::RequestStatus::Pending {
        let by = r.resolved_by.clone().unwrap_or_else(|| "someone".into());
        edit(cards::answered(&r, &by, &cards::answer_text(&r))).await;
        let _ = super::set_card(code, &row.id, None, Some("answered"), None);
        return Handled::Done;
    }
    let picked: Vec<(String, Vec<String>)> = match &r.body {
        agent_proto::RequestBody::Questions { questions } => questions
            .iter()
            .filter_map(|q| super::mark(code, &pick_key(card, &q.id)).and_then(|v| serde_json::from_str(&v).ok()).map(|v| (q.id.clone(), v)))
            .collect(),
        _ => vec![],
    };
    let (response, words) = match cards::response(&r, pick, &picked) {
        Ok(x) => x,
        // Not complete yet (more selects to make): nothing to do until Submit.
        Err(_) => return Handled::Done,
    };
    let cmd = json!({ "type": "request.respond", "request": r.id, "response": response });
    match super::command(code, &b, &format!("discord:answer:{action}"), cmd).await {
        Ok(_) => {
            let _ = super::set_card(code, &row.id, None, Some("answered"), Some(user_id));
            edit(cards::answered(&r, user_name, &words)).await;
            Handled::Done
        }
        Err((true, why)) => Handled::Retry(why),
        Err((false, why)) => Handled::Fail(why),
    }
}

// ── posting ──

fn failure(e: wire::Error) -> Failure {
    Failure { transient: e.transient(), message: e.to_string() }
}

/// Discord as a [`Surface`]: requests are replies to the status message; the status carries a
/// Reply button once it's bound.
pub(super) struct DiscordSurface<'a> {
    pub client: &'a Client,
    pub binding: parking_lot::Mutex<Option<String>>,
}

impl DiscordSurface<'_> {
    fn status_payload(&self, card: &StatusCard) -> Value {
        let mut p = wire::status(card);
        if let Some(b) = self.binding.lock().clone() {
            let reply = json!({ "type": 1, "components": [{ "type": 2, "style": 2, "label": "Reply", "custom_id": format!("{STEER}{b}") }] });
            match p["components"].as_array_mut() {
                Some(rows) if rows.len() < 5 => rows.push(reply),
                Some(_) => {}
                None => p["components"] = json!([reply]),
            }
        }
        p
    }
}

#[async_trait::async_trait]
impl Surface for DiscordSurface<'_> {
    async fn post_status(&self, card: &StatusCard, channel: &str, thread: Option<&str>) -> Result<String, Failure> {
        let mut p = self.status_payload(card);
        if let Some(t) = thread {
            p["message_reference"] = json!({ "message_id": t, "fail_if_not_exists": false });
        }
        self.client.create_message(channel, &p).await.map(|p| p.id).map_err(failure)
    }

    async fn update_status(&self, card: &StatusCard, channel: &str, ts: &str) -> Result<(), Failure> {
        self.client.edit_message(channel, ts, &self.status_payload(card)).await.map(|_| ()).map_err(failure)
    }

    async fn post_request(&self, card: &RequestCard, channel: &str, thread: &str) -> Result<String, Failure> {
        let mut p = wire::request(card);
        p["message_reference"] = json!({ "message_id": thread, "fail_if_not_exists": false });
        self.client.create_message(channel, &p).await.map(|p| p.id).map_err(failure)
    }

    async fn update_request(&self, card: &RequestCard, channel: &str, ts: &str) -> Result<(), Failure> {
        self.client.edit_message(channel, ts, &wire::request(card)).await.map(|_| ()).map_err(failure)
    }

    fn with_binding(&self, _card: &mut StatusCard, binding: &str) {
        *self.binding.lock() = Some(binding.to_string());
    }
}

// ── steps ──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Post,
    Status,
    Notify,
}

struct DiscordStep {
    kind: Kind,
    code: Weak<Code>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    guild: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    machine: Option<String>,
    #[serde(default)]
    thread: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl DiscordStep {
    fn kind_name(&self) -> &'static str {
        match self.kind {
            Kind::Post => "discord.post",
            Kind::Status => "discord.status",
            Kind::Notify => "discord.notify",
        }
    }
}

#[async_trait::async_trait]
impl StepKind for DiscordStep {
    fn info(&self) -> StepInfo {
        let s = |d: &str| json!({ "type": "string", "description": d });
        let guild = s("The server (guild id); unset: the event's, or the owner's only one.");
        let (title, description, props, required, output): (&str, &str, Value, Value, &[&str]) = match self.kind {
            Kind::Post => (
                "Post to Discord",
                "Post a message to a channel, or DM a user.",
                json!({ "guild": guild, "channel": s("Channel id; unset: the event's."), "user": s("A user id to DM instead."), "text": s("The message (a template).") }),
                json!(["text"]),
                &["channel", "id"],
            ),
            Kind::Status => (
                "Live status in Discord",
                "Follow an agent thread in Discord: a status message that updates, buttons and selects for its requests, and a Reply button to steer it.",
                json!({ "guild": guild, "machine": s("The agent's machine."), "thread": s("The agent thread."), "channel": s("Unset: the event's."), "message": s("Reply to this message.") }),
                json!(["machine", "thread"]),
                &["channel", "ts", "status", "title", "change.url"],
            ),
            Kind::Notify => (
                "Notify in Discord",
                "Tell a channel or person about an agent, with buttons to answer what it waits on.",
                json!({ "guild": guild, "channel": s("A channel id."), "user": s("Or a user id to DM."), "machine": s("Unset: the agent event's."), "thread": s("Unset: the agent event's.") }),
                json!([]),
                &["channel", "ts", "status"],
            ),
        };
        StepInfo {
            kind: self.kind_name().into(),
            title: title.into(),
            description: description.into(),
            config: json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false }),
            output: output.iter().map(|s| s.to_string()).collect(),
            integration: Some(SOURCE.into()),
            needs_setup: false,
        }
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        let kind = self.kind_name();
        let c: Config = serde_json::from_value(if config.is_null() { json!({}) } else { config.clone() }).map_err(|e| format!("{kind} config: {e}"))?;
        let empty = |v: &Option<String>| v.as_deref().is_none_or(|s| s.trim().is_empty());
        match self.kind {
            Kind::Post if empty(&c.text) => Err("discord.post needs text".into()),
            Kind::Status if empty(&c.machine) || empty(&c.thread) => Err("discord.status needs machine and thread".into()),
            Kind::Notify if empty(&c.channel) && empty(&c.user) => Err("discord.notify needs a channel or a user".into()),
            _ => Ok(()),
        }
    }

    fn default_retry(&self) -> Retry {
        Retry { attempts: 4, backoff_s: 15 }
    }

    fn default_timeout(&self) -> Duration {
        if self.kind == Kind::Status { Duration::from_secs(48 * 3600) } else { Duration::from_secs(10 * 60) }
    }

    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        not_set_up(&self.code, owner)
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let code = ctx.code.clone();
        let kind = self.kind_name();
        let Some((_, client)) = code.discord() else { return StepOutcome::Fail("Discord isn't configured on this server (ROLL_DISCORD_*)".into()) };
        let rendered = match ctx.render_json(&ctx.step().config.clone()) {
            Ok(v) => v,
            Err(e) => return StepOutcome::Fail(e),
        };
        let c: Config = match serde_json::from_value(if rendered.is_null() { json!({}) } else { rendered }) {
            Ok(c) => c,
            Err(e) => return StepOutcome::Fail(format!("{kind} config: {e}")),
        };
        let ev = ctx.context()["event"].clone();
        let from_discord = ev["source"] == SOURCE;
        let evs = |k: &str| if from_discord { ev["data"][k].as_str().filter(|s| !s.is_empty()).map(str::to_string) } else { None };
        let guild = c.guild.clone().filter(|g| !g.is_empty()).or_else(|| evs("guild"));
        let install = match super::install_for_step(&code, &ctx.owner().id, Platform::Discord, guild.as_deref()) {
            Ok(i) => i,
            Err(e) => return StepOutcome::Fail(e),
        };
        let fail = |e: wire::Error| if e.transient() { StepOutcome::Retry(e.to_string()) } else { StepOutcome::Fail(e.to_string()) };
        let channel = match (c.user.clone().filter(|u| !u.is_empty()), c.channel.clone().filter(|c| !c.is_empty()).or_else(|| evs("channel"))) {
            (Some(u), _) => match client.dm_channel(&u).await {
                Ok(ch) => ch,
                Err(e) => return fail(e),
            },
            (None, Some(ch)) => ch,
            (None, None) => return StepOutcome::Fail(format!("{kind} has no channel (and the event none)")),
        };
        match self.kind {
            Kind::Post => {
                let once = format!("discord-post:{}", ctx.idempotency_key());
                if let Some(done) = super::mark(&code, &once).and_then(|m| serde_json::from_str::<Value>(&m).ok()) {
                    return StepOutcome::Done(done);
                }
                let text: String = c.text.unwrap_or_default().chars().take(2000).collect();
                match client.create_message(&channel, &json!({ "content": text, "allowed_mentions": { "parse": [] } })).await {
                    Ok(p) => {
                        let out = json!({ "channel": p.channel, "id": p.id, "ts": p.id });
                        super::set_mark(&code, &once, &out.to_string());
                        StepOutcome::Done(out)
                    }
                    Err(e) => fail(e),
                }
            }
            Kind::Status | Kind::Notify => {
                let (machine, thread) = follow::target(ctx, c.machine, c.thread);
                let f = Follow { machine, thread, channel, thread_ts: c.message.filter(|m| !m.is_empty()), ts: None, once: self.kind == Kind::Notify };
                follow::step(ctx, &DiscordSurface { client, binding: Default::default() }, &install, &f).await
            }
        }
    }
}
