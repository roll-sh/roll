//! Slack (docs/code/chatops.md, "Slack"): the `slack` trigger (`/roll …`, mentions of the bot,
//! a watched channel's messages, a reaction on a message), the actions Slack sends (handled from
//! `chat_actions`: triggers, steers, answers), and the steps that post, edit, reply, DM, follow a
//! thread and notify.
//!
//! Who may drive roll from a workspace is the install's: its installer, its members, or anyone
//! when the owner opened it ([`Install::allows`]). Events nobody may act on, and chatter no
//! automation listens for, run nothing.

use super::cards;
use super::follow::{self, Failure, Follow, Surface};
use super::{Handled, Install, Platform};
use crate::Code;
use crate::agents::automations::{NewEvent, OwnerRef, Registry, StepContext, StepKind, StepOutcome, TriggerSource};
use agent_proto::automation::{AutomationEvent, EventInfo, LogLevel, Retry, StepInfo, TriggerInfo};
use code_chat::card::{RequestCard, StatusCard};
use code_chat::slack::{self as wire, Client, Envelope, Event, Interaction, Message};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Weak};
use std::time::Duration;

pub const SOURCE: &str = "slack";
/// The reaction that hands a message to roll unless a trigger names others.
pub const DEFAULT_REACTION: &str = "robot_face";
/// The modal for a free-text answer.
const ANSWER_MODAL: &str = "roll.answer";

pub(super) fn register(code: &Arc<Code>, r: &Registry) {
    let me = Arc::downgrade(code);
    r.register_trigger(Arc::new(SlackTrigger(me.clone())));
    for kind in [Kind::Post, Kind::Update, Kind::Reply, Kind::Dm, Kind::Status, Kind::Notify] {
        r.register_step(Arc::new(SlackStep { kind, code: me.clone() }));
    }
}

fn not_set_up(code: &Weak<Code>, owner: &OwnerRef) -> bool {
    let Some(code) = code.upgrade() else { return true };
    code.slack().is_none() || super::installs(&code, &owner.id, Platform::Slack).map(|i| i.is_empty()).unwrap_or(true)
}

// ── the trigger ──

struct SlackTrigger(Weak<Code>);

#[derive(Debug, Default, Deserialize)]
struct TriggerConfig {
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    channels: Vec<String>,
    #[serde(default)]
    reactions: Vec<String>,
}

impl TriggerSource for SlackTrigger {
    fn info(&self) -> TriggerInfo {
        let ev = |kind: &str, title: &str, fields: &[&str]| EventInfo { kind: kind.into(), title: title.into(), fields: fields.iter().map(|f| f.to_string()).collect() };
        TriggerInfo {
            source: SOURCE.into(),
            title: "Slack".into(),
            description: "/roll, a mention of the roll bot, a message in a watched channel, or a reaction on a message.".into(),
            events: vec![
                ev("command", "/roll <prompt>", &["prompt", "user", "user_name", "channel", "team"]),
                ev("mention", "The roll bot is mentioned", &["prompt", "text", "user", "channel", "thread_ts", "team"]),
                ev("message", "A message in a watched channel", &["text", "user", "channel", "thread_ts", "in_thread", "team"]),
                ev("reaction", "A reaction on a message", &["reaction", "text", "user", "author", "channel", "thread_ts", "team"]),
            ],
            config: json!({
                "type": "object",
                "properties": {
                    "team": { "type": "string", "description": "The workspace (team id); unset: the owner's only one." },
                    "channels": { "type": "array", "items": { "type": "string" }, "description": "Channel ids to listen in; unset: any channel the bot is in. Needed for message." },
                    "reactions": { "type": "array", "items": { "type": "string" }, "description": "Reaction names (no colons); unset: robot_face." }
                }
            }),
            needs_setup: false,
        }
    }

    fn check(&self, owner: &OwnerRef, events: &[String], config: &Value) -> Result<(), String> {
        let c: TriggerConfig = serde_json::from_value(if config.is_null() { json!({}) } else { config.clone() }).map_err(|e| format!("slack trigger config: {e}"))?;
        if let Some(k) = events.iter().find(|k| !["command", "mention", "message", "reaction"].contains(&k.as_str())) {
            return Err(format!("slack has no event {k}"));
        }
        if events.iter().any(|k| k == "message") && c.channels.is_empty() {
            return Err("a message trigger needs channels to watch".into());
        }
        if not_set_up(&self.0, owner) {
            return Err("Slack isn't connected: connect it in the agents settings".into());
        }
        Ok(())
    }

    fn matches(&self, config: &Value, event: &AutomationEvent) -> bool {
        let c: TriggerConfig = serde_json::from_value(config.clone()).unwrap_or_default();
        let d = &event.data;
        let s = |k: &str| d[k].as_str().unwrap_or_default();
        if c.team.as_deref().is_some_and(|t| !t.is_empty() && t != s("team")) {
            return false;
        }
        if !c.channels.is_empty() && !c.channels.iter().any(|ch| ch == s("channel")) {
            return false;
        }
        if event.kind == "reaction" {
            let wanted = if c.reactions.is_empty() { vec![DEFAULT_REACTION.to_string()] } else { c.reactions.clone() };
            return wanted.iter().any(|r| r.trim_matches(':') == s("reaction"));
        }
        true
    }

    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        not_set_up(&self.0, owner)
    }
}

// ── actions Slack sent ──

fn failure(e: wire::Error) -> Failure {
    Failure { transient: e.transient(), message: e.to_string() }
}

fn handled(e: wire::Error) -> Handled {
    if e.transient() { Handled::Retry(e.to_string()) } else { Handled::Fail(e.to_string()) }
}

/// Handle one recorded Slack action (`slack.event`, `slack.command`, `slack.interaction`).
pub(super) async fn handle(code: &Arc<Code>, install: &Install, action: &str, kind: &str, payload: &str) -> Handled {
    let Some((_, client)) = code.slack() else { return Handled::Retry("Slack isn't configured on this node".into()) };
    let Some(token) = install.token(code) else { return Handled::Fail("the install's token can't be opened (ROLL_SECRET_KEY)".into()) };
    let body = payload.as_bytes();
    match kind {
        "slack.event" => match wire::parse_envelope(body) {
            Ok(Envelope::Callback { event_id, event, team_id, .. }) => on_event(code, client, &token, install, &team_id, &event_id, event).await,
            Ok(_) => Handled::Done,
            Err(e) => Handled::Fail(e),
        },
        "slack.command" => match wire::parse_command(body) {
            Ok(c) => {
                let prompt = c.text.trim().to_string();
                if prompt.is_empty() {
                    let _ = client.post_ephemeral(&token, &c.channel_id, &c.user_id, "Tell roll what to do: `/roll fix the flaky login test`.").await;
                    return Handled::Done;
                }
                let data = json!({ "team": c.team_id, "channel": c.channel_id, "user": c.user_id, "user_name": c.user_name, "text": prompt, "prompt": prompt, "thread_ts": null });
                trigger(code, client, &token, install, "command", &format!("command:{}", c.trigger_id), data, &c.channel_id, &c.user_id, None).await
            }
            Err(e) => Handled::Fail(e),
        },
        "slack.interaction" => match wire::parse_interaction(body) {
            Ok(Interaction::BlockActions(b)) => {
                let Some(hit) = b.actions.iter().find(|a| a.value.as_deref().and_then(cards::parse_value).is_some()) else { return Handled::Done };
                let value = hit.value.clone().unwrap_or_default();
                let Some((card, pick)) = cards::parse_value(&value) else { return Handled::Done };
                let channel = b.channel_id.clone().unwrap_or_default();
                if pick == "t" {
                    // A free-text answer: a modal, then its submission answers.
                    let view = wire::blocks::text_modal(ANSWER_MODAL, card, "Answer", "Your answer");
                    return match client.open_view(&token, &b.trigger_id, &view).await {
                        Ok(()) => Handled::Done,
                        Err(e) => handled(e),
                    };
                }
                let picked: Vec<(String, Vec<String>)> = b
                    .state
                    .iter()
                    .filter_map(|s| s.action_id.strip_prefix("roll.q.").map(|q| (q.to_string(), s.values.clone())))
                    .collect();
                answer(code, client, &token, install, action, card, pick, &picked, &b.user_id, &channel).await
            }
            Ok(Interaction::ViewSubmission(v)) if v.callback_id == ANSWER_MODAL => {
                let text: Vec<String> = v.state.iter().filter(|s| s.action_id == "roll.text").flat_map(|s| s.values.clone()).collect();
                let card = v.private_metadata.clone();
                let Ok(Some(row)) = super::card_by_id(code, &card) else { return Handled::Done };
                let qid = match current_request(code, &row) {
                    Some(agent_proto::Request { body: agent_proto::RequestBody::Questions { questions }, .. }) => questions.iter().find(|q| q.options.is_empty()).map(|q| q.id.clone()),
                    _ => None,
                };
                let picked = qid.map(|q| vec![(q, text)]).unwrap_or_default();
                answer(code, client, &token, install, action, &card, "t", &picked, &v.user_id, &row.channel).await
            }
            Ok(_) => Handled::Done,
            Err(e) => Handled::Fail(e),
        },
        _ => Handled::Fail(format!("unknown Slack action {kind}")),
    }
}

async fn on_event(code: &Arc<Code>, client: &Client, token: &str, install: &Install, team: &str, event_id: &str, event: Event) -> Handled {
    match event {
        Event::AppMention(m) | Event::Message(m) if m.bot_id.is_some() || m.user.as_deref() == Some(install.bot_user_id.as_str()) => Handled::Done,
        Event::Message(m) if m.subtype.is_some() => Handled::Done,
        Event::AppMention(m) => {
            let user = m.user.clone().unwrap_or_default();
            let prompt = wire::strip_mention(&m.text, &install.bot_user_id);
            if let Some(b) = bound(code, install, &m.channel, m.thread_ts.as_deref()) {
                return steer(code, client, token, install, &b, &user, &prompt, &m.channel, &m.ts).await;
            }
            let thread_ts = m.thread_ts.clone().unwrap_or_else(|| m.ts.clone());
            let data = json!({ "team": team, "channel": m.channel, "user": user, "text": m.text, "prompt": prompt, "ts": m.ts, "thread_ts": thread_ts });
            trigger(code, client, token, install, "mention", event_id, data, &m.channel, &user, Some(&m.ts)).await
        }
        Event::Message(m) => {
            let user = m.user.clone().unwrap_or_default();
            if let Some(b) = bound(code, install, &m.channel, m.thread_ts.as_deref()) {
                let text = wire::strip_mention(&m.text, &install.bot_user_id);
                return steer(code, client, token, install, &b, &user, &text, &m.channel, &m.ts).await;
            }
            // A mention is the mention event's.
            if m.text.contains(&format!("<@{}", install.bot_user_id)) {
                return Handled::Done;
            }
            let data = json!({
                "team": team, "channel": m.channel, "user": user, "text": m.text, "ts": m.ts,
                "thread_ts": m.thread_ts.clone().unwrap_or_else(|| m.ts.clone()), "in_thread": m.thread_ts.as_ref().is_some_and(|t| *t != m.ts),
            });
            trigger(code, client, token, install, "message", event_id, data, &m.channel, &user, None).await
        }
        Event::ReactionAdded(r) => {
            if !super::anyone_listens(code, &install.owner_id, SOURCE, "reaction") || r.user == install.bot_user_id {
                return Handled::Done;
            }
            let fetched = match client.fetch_message(token, &r.channel, &r.ts).await {
                Ok(m) => m,
                Err(e) if e.transient() => return Handled::Retry(e.to_string()),
                Err(_) => None,
            };
            let (text, thread_ts) = fetched.map(|m| (m.text, m.thread_ts)).unwrap_or_default();
            let data = json!({
                "team": team, "channel": r.channel, "user": r.user, "reaction": r.reaction, "ts": r.ts, "text": text,
                "author": r.item_user, "thread_ts": thread_ts.unwrap_or_else(|| r.ts.clone()),
            });
            trigger(code, client, token, install, "reaction", event_id, data, &r.channel, &r.user, Some(&r.ts)).await
        }
        Event::Other(t) if t == "app_uninstalled" || t == "tokens_revoked" => match super::delete_install(code, &install.id) {
            Ok(()) => Handled::Done,
            Err(e) => Handled::Retry(e.to_string()),
        },
        Event::Other(_) => Handled::Done,
    }
}

fn bound(code: &Code, install: &Install, channel: &str, thread_ts: Option<&str>) -> Option<super::Binding> {
    super::binding_at(code, &install.id, channel, thread_ts?).ok().flatten()
}

/// Record a trigger event for the owner's automations, if the user may and anyone listens.
#[allow(clippy::too_many_arguments)]
async fn trigger(code: &Arc<Code>, client: &Client, token: &str, install: &Install, kind: &str, id: &str, data: Value, channel: &str, user: &str, ack: Option<&str>) -> Handled {
    let loud = kind == "command" || kind == "mention";
    if !install.allows(user) {
        if loud {
            let _ = client.post_ephemeral(token, channel, user, "Only people the roll owner allows can use roll here.").await;
        }
        return Handled::Done;
    }
    if !super::anyone_listens(code, &install.owner_id, SOURCE, kind) {
        if loud {
            let _ = client
                .post_ephemeral(token, channel, user, "No automation answers this yet. In roll, add one from the template \"/roll in Slack\".")
                .await;
        }
        return Handled::Done;
    }
    let summary = match kind {
        "command" | "mention" => format!("{kind}: {}", data["prompt"].as_str().unwrap_or_default()),
        "reaction" => format!(":{}: on a message", data["reaction"].as_str().unwrap_or_default()),
        _ => format!("message in {channel}"),
    };
    let e = NewEvent { source: SOURCE.into(), kind: kind.into(), id: format!("{}:{id}", install.team_id), data, summary, automation: None };
    match code.automation_event(&install.owner_id, e) {
        Ok(_) => {
            if let Some(ts) = ack {
                let _ = client.add_reaction(token, channel, ts, "eyes").await;
            }
            Handled::Done
        }
        Err(e) => Handled::Retry(e.to_string()),
    }
}

/// A reply in a bound thread: sent to the agent thread as a steer.
#[allow(clippy::too_many_arguments)]
async fn steer(code: &Arc<Code>, client: &Client, token: &str, install: &Install, b: &super::Binding, user: &str, text: &str, channel: &str, ts: &str) -> Handled {
    if text.trim().is_empty() {
        return Handled::Done;
    }
    if !install.allows(user) {
        let _ = client.post_ephemeral(token, channel, user, "Only people the roll owner allows can steer this agent.").await;
        return Handled::Done;
    }
    // A mention in a thread comes as two events (the mention and the message): one steer.
    let key = format!("slack:steer:{}:{channel}:{ts}", install.id);
    let text = format!("{text}\n\n(from <@{user}> in Slack)");
    let cmd = json!({ "type": "message.send", "thread": b.thread, "text": text, "attachments": [], "mode": "steer" });
    match super::command(code, b, &key, cmd).await {
        Ok(_) => {
            if super::first_time(code, &key).unwrap_or(true) {
                let _ = client.add_reaction(token, channel, ts, "eyes").await;
            }
            Handled::Done
        }
        Err((true, why)) => Handled::Retry(why),
        Err((false, why)) => {
            let _ = client.post_ephemeral(token, channel, user, &format!("roll couldn't pass that on: {why}")).await;
            Handled::Fail(why)
        }
    }
}

fn current_request(code: &Code, row: &super::CardRow) -> Option<agent_proto::Request> {
    let b = super::binding_by_id(code, &row.binding_id).ok()??;
    match crate::agents::store::mirrored(code.db.sql(), &b.machine_id, agent_proto::EntityKind::Request, &row.request).ok()?? {
        agent_proto::Entity::Request(r) => Some(r),
        _ => None,
    }
}

/// A click on a request card: answer the request (as the binding's automation) and show who did.
#[allow(clippy::too_many_arguments)]
async fn answer(code: &Arc<Code>, client: &Client, token: &str, install: &Install, action: &str, card: &str, pick: &str, picked: &[(String, Vec<String>)], user: &str, channel: &str) -> Handled {
    let Ok(Some(row)) = super::card_by_id(code, card) else { return Handled::Done };
    let Ok(Some(b)) = super::binding_by_id(code, &row.binding_id) else { return Handled::Done };
    // A card is answered only from the workspace it was posted to.
    if b.install_id != install.id {
        return Handled::Fail("a card from another install".into());
    }
    let say = |text: String| async move {
        let _ = client.post_ephemeral(token, channel, user, &text).await;
    };
    if !install.allows(user) {
        say("Only people the roll owner allows can answer this.".into()).await;
        return Handled::Done;
    }
    let Some(r) = current_request(code, &row) else { return Handled::Retry("the request isn't in the mirror yet".into()) };
    if r.status != agent_proto::RequestStatus::Pending {
        let by = r.resolved_by.clone().unwrap_or_else(|| "someone".into());
        let done = cards::answered(&r, &by, &cards::answer_text(&r));
        let _ = client.update_message(token, &row.channel, &row.ts, &request_message(&row.channel, &done, None)).await;
        let _ = super::set_card(code, &row.id, None, Some("answered"), None);
        say("That was already answered.".into()).await;
        return Handled::Done;
    }
    let (response, words) = match cards::response(&r, pick, picked) {
        Ok(x) => x,
        Err(why) => {
            say(why).await;
            return Handled::Done;
        }
    };
    let cmd = json!({ "type": "request.respond", "request": r.id, "response": response });
    match super::command(code, &b, &format!("slack:answer:{action}"), cmd).await {
        Ok(_) => {
            let done = cards::answered(&r, &format!("<@{user}>"), &words);
            let _ = super::set_card(code, &row.id, None, Some("answered"), Some(user));
            if let Err(e) = client.update_message(token, &row.channel, &row.ts, &request_message(&row.channel, &done, None)).await {
                tracing::debug!(error = %e, "updating an answered Slack card");
            }
            Handled::Done
        }
        Err((true, why)) => Handled::Retry(why),
        Err((false, why)) => {
            say(format!("roll couldn't answer: {why}")).await;
            Handled::Fail(why)
        }
    }
}

// ── posting ──

fn status_message(channel: &str, card: &StatusCard, thread: Option<&str>) -> Message {
    Message { channel: channel.into(), text: card.text(), blocks: Some(wire::blocks::status(card)), thread_ts: thread.map(str::to_string) }
}

fn request_message(channel: &str, card: &RequestCard, thread: Option<&str>) -> Message {
    Message { channel: channel.into(), text: card.text(), blocks: Some(wire::blocks::request(card)), thread_ts: thread.map(str::to_string) }
}

/// Slack as a [`Surface`].
pub(super) struct SlackSurface<'a> {
    pub client: &'a Client,
    pub token: String,
}

#[async_trait::async_trait]
impl Surface for SlackSurface<'_> {
    async fn post_status(&self, card: &StatusCard, channel: &str, thread: Option<&str>) -> Result<String, Failure> {
        self.client.post_message(&self.token, &status_message(channel, card, thread)).await.map(|p| p.ts).map_err(failure)
    }

    async fn update_status(&self, card: &StatusCard, channel: &str, ts: &str) -> Result<(), Failure> {
        self.client.update_message(&self.token, channel, ts, &status_message(channel, card, None)).await.map(|_| ()).map_err(failure)
    }

    async fn post_request(&self, card: &RequestCard, channel: &str, thread: &str) -> Result<String, Failure> {
        self.client.post_message(&self.token, &request_message(channel, card, Some(thread))).await.map(|p| p.ts).map_err(failure)
    }

    async fn update_request(&self, card: &RequestCard, channel: &str, ts: &str) -> Result<(), Failure> {
        self.client.update_message(&self.token, channel, ts, &request_message(channel, card, None)).await.map(|_| ()).map_err(failure)
    }
}

// ── steps ──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Post,
    Update,
    Reply,
    Dm,
    Status,
    Notify,
}

struct SlackStep {
    kind: Kind,
    code: Weak<Code>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    blocks: Option<Value>,
    #[serde(default)]
    machine: Option<String>,
    #[serde(default)]
    thread: Option<String>,
}

impl SlackStep {
    fn kind_name(&self) -> &'static str {
        match self.kind {
            Kind::Post => "slack.post",
            Kind::Update => "slack.update",
            Kind::Reply => "slack.reply",
            Kind::Dm => "slack.dm",
            Kind::Status => "slack.status",
            Kind::Notify => "slack.notify",
        }
    }

    fn schema(&self) -> Value {
        let str_ = |d: &str| json!({ "type": "string", "description": d });
        let team = str_("The workspace (team id); unset: the event's, or the owner's only one.");
        let text = str_("The message (a template; Slack mrkdwn).");
        let blocks = json!({ "type": "array", "description": "Block Kit blocks (strings in them are templates); text is the fallback." });
        let (props, required) = match self.kind {
            Kind::Post => (json!({ "team": team, "channel": str_("Channel id."), "text": text, "blocks": blocks, "thread_ts": str_("Post in this thread.") }), json!(["channel", "text"])),
            Kind::Update => (json!({ "team": team, "channel": str_("Channel id."), "ts": str_("The message to edit (an earlier step's ts)."), "text": text, "blocks": blocks }), json!(["channel", "ts", "text"])),
            Kind::Reply => (json!({ "team": team, "channel": str_("Unset: the event's."), "thread_ts": str_("Unset: the event's thread."), "text": text, "blocks": blocks }), json!(["text"])),
            Kind::Dm => (json!({ "team": team, "user": str_("Slack user id (U…)."), "text": text, "blocks": blocks }), json!(["user", "text"])),
            Kind::Status => (
                json!({
                    "team": team, "machine": str_("The agent's machine (steps.<start>.machine)."), "thread": str_("The agent thread (steps.<start>.thread)."),
                    "channel": str_("Unset: the event's."), "thread_ts": str_("Unset: the event's thread; none: the status starts one."), "ts": str_("Edit this status message instead of posting one.")
                }),
                json!(["machine", "thread"]),
            ),
            Kind::Notify => (
                json!({ "team": team, "to": str_("A channel id, or a user id (U…) for a DM."), "machine": str_("Unset: the agent event's."), "thread": str_("Unset: the agent event's."), "text": str_("A line above the card (a template).") }),
                json!(["to"]),
            ),
        };
        json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false })
    }
}

#[async_trait::async_trait]
impl StepKind for SlackStep {
    fn info(&self) -> StepInfo {
        let (title, description, output): (&str, &str, &[&str]) = match self.kind {
            Kind::Post => ("Post to Slack", "Post a message (text or Block Kit) to a channel or thread.", &["channel", "ts", "thread_ts"]),
            Kind::Update => ("Edit a Slack message", "Change a message an earlier step posted.", &["channel", "ts"]),
            Kind::Reply => ("Reply in the Slack thread", "Reply in the thread the event came from.", &["channel", "ts", "thread_ts"]),
            Kind::Dm => ("DM a Slack user", "Send a direct message.", &["channel", "ts"]),
            Kind::Status => (
                "Live status in Slack",
                "Follow an agent thread in Slack: one status message that updates (running, steps, done with the PR), buttons for its approvals and questions, and replies in the thread steer it. Ends when the thread's work does.",
                &["channel", "ts", "thread_ts", "status", "title", "change.url", "failed"],
            ),
            Kind::Notify => (
                "Notify in Slack",
                "Tell a channel or a person about an agent (waiting on you, failed, done), with buttons to answer what it waits on.",
                &["channel", "ts", "status", "title"],
            ),
        };
        StepInfo {
            kind: self.kind_name().into(),
            title: title.into(),
            description: description.into(),
            config: self.schema(),
            output: output.iter().map(|s| s.to_string()).collect(),
            integration: Some(SOURCE.into()),
            needs_setup: false,
        }
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        let kind = self.kind_name();
        let c: Config = serde_json::from_value(if config.is_null() { json!({}) } else { config.clone() }).map_err(|e| format!("{kind} config: {e}"))?;
        let need = |v: &Option<String>, what: &str| if v.as_deref().is_none_or(|s| s.trim().is_empty()) { Err(format!("{kind} needs {what}")) } else { Ok(()) };
        match self.kind {
            Kind::Post => need(&c.channel, "channel").and(need(&c.text, "text")),
            Kind::Update => need(&c.channel, "channel").and(need(&c.ts, "ts")).and(need(&c.text, "text")),
            Kind::Reply => need(&c.text, "text"),
            Kind::Dm => need(&c.user, "user").and(need(&c.text, "text")),
            Kind::Status => need(&c.machine, "machine").and(need(&c.thread, "thread")),
            Kind::Notify => need(&c.to, "to"),
        }
    }

    fn default_retry(&self) -> Retry {
        Retry { attempts: 4, backoff_s: 15 }
    }

    fn default_timeout(&self) -> Duration {
        match self.kind {
            // Agents can work for hours, and wait on people for longer.
            Kind::Status => Duration::from_secs(48 * 3600),
            _ => Duration::from_secs(10 * 60),
        }
    }

    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        not_set_up(&self.code, owner)
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let code = ctx.code.clone();
        let kind = self.kind_name();
        let Some((_, client)) = code.slack() else { return StepOutcome::Fail("Slack isn't configured on this server (ROLL_SLACK_*)".into()) };
        let rendered = match ctx.render_json(&ctx.step().config.clone()) {
            Ok(v) => v,
            Err(e) => return StepOutcome::Fail(e),
        };
        let c: Config = match serde_json::from_value(if rendered.is_null() { json!({}) } else { rendered }) {
            Ok(c) => c,
            Err(e) => return StepOutcome::Fail(format!("{kind} config: {e}")),
        };
        let ev = ctx.context()["event"]["data"].clone();
        let evs = |k: &str| ev[k].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        let from_slack = ctx.context()["event"]["source"] == SOURCE;
        let team = c.team.clone().filter(|t| !t.is_empty()).or_else(|| if from_slack { evs("team") } else { None });
        let install = match super::install_for_step(&code, &ctx.owner().id, Platform::Slack, team.as_deref()) {
            Ok(i) => i,
            Err(e) => return StepOutcome::Fail(e),
        };
        let Some(token) = install.token(&code) else { return StepOutcome::Fail("the Slack install's token can't be opened (ROLL_SECRET_KEY)".into()) };
        let text = c.text.clone().unwrap_or_default();
        let blocks = c.blocks.clone().filter(|b| !b.is_null());
        match self.kind {
            Kind::Post | Kind::Reply | Kind::Dm => {
                // A post is made once: an attempt that runs again finds what the last one posted.
                let once = format!("post:{}", ctx.idempotency_key());
                if let Some(done) = super::mark(&code, &once).and_then(|m| serde_json::from_str::<Value>(&m).ok()) {
                    return StepOutcome::Done(done);
                }
                let (channel, thread_ts) = match self.kind {
                    Kind::Post => (c.channel.unwrap_or_default(), c.thread_ts.filter(|t| !t.is_empty())),
                    Kind::Reply => {
                        let Some(channel) = c.channel.filter(|s| !s.is_empty()).or_else(|| if from_slack { evs("channel") } else { None }) else {
                            return StepOutcome::Fail("slack.reply has no channel (and the event none)".into());
                        };
                        (channel, c.thread_ts.filter(|s| !s.is_empty()).or_else(|| if from_slack { evs("thread_ts").or_else(|| evs("ts")) } else { None }))
                    }
                    _ => match client.open_dm(&token, c.user.as_deref().unwrap_or_default()).await {
                        Ok(ch) => (ch, None),
                        Err(e) => return outcome(e),
                    },
                };
                let m = Message { channel, text, blocks, thread_ts: thread_ts.clone() };
                match client.post_message(&token, &m).await {
                    Ok(p) => {
                        let out = json!({ "channel": p.channel, "ts": p.ts, "thread_ts": thread_ts.unwrap_or(p.ts.clone()) });
                        super::set_mark(&code, &once, &out.to_string());
                        StepOutcome::Done(out)
                    }
                    Err(e) => outcome(e),
                }
            }
            Kind::Update => {
                let (channel, ts) = (c.channel.unwrap_or_default(), c.ts.unwrap_or_default());
                match client.update_message(&token, &channel, &ts, &Message { channel: channel.clone(), text, blocks, thread_ts: None }).await {
                    Ok(p) => StepOutcome::Done(json!({ "channel": p.channel, "ts": p.ts })),
                    Err(e) => outcome(e),
                }
            }
            Kind::Status => {
                let (machine, thread) = follow::target(ctx, c.machine, c.thread);
                let Some(channel) = c.channel.filter(|s| !s.is_empty()).or_else(|| if from_slack { evs("channel") } else { None }) else {
                    return StepOutcome::Fail("slack.status has no channel (and the event none)".into());
                };
                let thread_ts = c.thread_ts.filter(|s| !s.is_empty()).or_else(|| if from_slack { evs("thread_ts") } else { None });
                let f = Follow { machine, thread, channel, thread_ts, ts: c.ts, once: false };
                follow::step(ctx, &SlackSurface { client, token }, &install, &f).await
            }
            Kind::Notify => {
                let (machine, thread) = follow::target(ctx, c.machine, c.thread);
                let to = c.to.unwrap_or_default();
                let channel = if to.starts_with('U') || to.starts_with('W') {
                    match client.open_dm(&token, &to).await {
                        Ok(ch) => ch,
                        Err(e) => return outcome(e),
                    }
                } else {
                    to
                };
                if !text.trim().is_empty() && super::first_time(&code, &format!("notify-text:{}", ctx.idempotency_key())).unwrap_or(true) {
                    if let Err(e) = client.post_message(&token, &Message { channel: channel.clone(), text: text.clone(), blocks: None, thread_ts: None }).await {
                        ctx.log(LogLevel::Warn, format!("the notice's text: {e}"));
                    }
                }
                let f = Follow { machine, thread, channel, thread_ts: None, ts: None, once: true };
                follow::step(ctx, &SlackSurface { client, token }, &install, &f).await
            }
        }
    }
}

fn outcome(e: wire::Error) -> StepOutcome {
    if e.transient() { StepOutcome::Retry(e.to_string()) } else { StepOutcome::Fail(e.to_string()) }
}
