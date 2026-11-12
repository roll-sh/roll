//! What Slack sends us, parsed from the raw (already verified) body:
//!
//! - The Events API posts JSON envelopes: a one-off `url_verification` challenge when the
//!   request URL is set, then `event_callback`s. Slack retries a delivery it didn't get a 2xx for
//!   within 3 seconds, with the same `event_id`: dedupe on it.
//! - Slash commands post `application/x-www-form-urlencoded` fields.
//! - Interactivity (buttons, selects, modal submits) posts a form with one `payload` field
//!   holding JSON. Messages' stateful elements (selects, inputs) arrive in `state.values`,
//!   keyed by block id and action id.
//!
//! Event and interaction types roll doesn't act on parse to `Other` so the endpoint still
//! answers 2xx.

use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
// One per request and matched on at once: boxing the event would only add noise.
#[allow(clippy::large_enum_variant)]
pub enum Envelope {
    /// Answer with the challenge, verbatim.
    UrlVerification { challenge: String },
    Callback {
        team_id: String,
        event_id: String,
        event_time: i64,
        event: Event,
    },
    /// Another envelope type (`app_rate_limited`, …).
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    AppMention(MessageEvent),
    /// A message in a channel or DM the bot is in. Ignore the bot's own (`bot_id`) and edits
    /// and joins (`subtype`) unless they matter.
    Message(MessageEvent),
    ReactionAdded(Reaction),
    Other(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct MessageEvent {
    pub user: Option<String>,
    /// Set when a bot (roll included) posted it.
    pub bot_id: Option<String>,
    /// `message_changed`, `channel_join`, `bot_message`, …; `None` for a person's plain message.
    pub subtype: Option<String>,
    #[serde(default)]
    pub channel: String,
    /// `channel`, `group`, `im` or `mpim`.
    pub channel_type: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub ts: String,
    /// The thread's root `ts` when it is a reply (or the root itself).
    pub thread_ts: Option<String>,
}

/// A reaction on a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaction {
    pub user: String,
    /// The emoji's name, without colons.
    pub reaction: String,
    pub channel: String,
    pub ts: String,
    /// Who wrote the message reacted to.
    pub item_user: Option<String>,
}

pub fn parse_envelope(body: &[u8]) -> Result<Envelope, String> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(rename = "type")]
        kind: String,
        challenge: Option<String>,
        team_id: Option<String>,
        event_id: Option<String>,
        event_time: Option<i64>,
        event: Option<Value>,
    }
    let raw: Raw = serde_json::from_slice(body).map_err(|e| format!("slack envelope: {e}"))?;
    match raw.kind.as_str() {
        "url_verification" => Ok(Envelope::UrlVerification {
            challenge: raw.challenge.ok_or("url_verification without challenge")?,
        }),
        "event_callback" => Ok(Envelope::Callback {
            team_id: raw.team_id.ok_or("event_callback without team_id")?,
            event_id: raw.event_id.ok_or("event_callback without event_id")?,
            event_time: raw.event_time.unwrap_or_default(),
            event: parse_event(raw.event.ok_or("event_callback without event")?)?,
        }),
        other => Ok(Envelope::Other(other.to_owned())),
    }
}

fn parse_event(v: Value) -> Result<Event, String> {
    let kind = v
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let message = |v: Value| {
        serde_json::from_value::<MessageEvent>(v).map_err(|e| format!("slack {kind} event: {e}"))
    };
    match kind.as_str() {
        "app_mention" => Ok(Event::AppMention(message(v)?)),
        "message" => Ok(Event::Message(message(v)?)),
        "reaction_added" => {
            #[derive(Deserialize)]
            struct Item {
                #[serde(rename = "type")]
                kind: String,
                channel: Option<String>,
                ts: Option<String>,
            }
            #[derive(Deserialize)]
            struct Raw {
                user: String,
                reaction: String,
                item_user: Option<String>,
                item: Item,
            }
            let raw: Raw =
                serde_json::from_value(v).map_err(|e| format!("slack reaction_added: {e}"))?;
            match (raw.item.kind.as_str(), raw.item.channel, raw.item.ts) {
                ("message", Some(channel), Some(ts)) => Ok(Event::ReactionAdded(Reaction {
                    user: raw.user,
                    reaction: raw.reaction,
                    channel,
                    ts,
                    item_user: raw.item_user,
                })),
                _ => Ok(Event::Other(kind)),
            }
        }
        _ => Ok(Event::Other(kind)),
    }
}

/// A slash command (`/roll fix the flaky test`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    pub team_id: String,
    pub channel_id: String,
    pub user_id: String,
    pub user_name: String,
    /// `/roll`.
    pub command: String,
    /// Everything after the command.
    pub text: String,
    pub trigger_id: String,
}

pub fn parse_command(body: &[u8]) -> Result<SlashCommand, String> {
    let mut c = SlashCommand {
        team_id: String::new(),
        channel_id: String::new(),
        user_id: String::new(),
        user_name: String::new(),
        command: String::new(),
        text: String::new(),
        trigger_id: String::new(),
    };
    for (k, v) in url::form_urlencoded::parse(body) {
        let slot = match &*k {
            "team_id" => &mut c.team_id,
            "channel_id" => &mut c.channel_id,
            "user_id" => &mut c.user_id,
            "user_name" => &mut c.user_name,
            "command" => &mut c.command,
            "text" => &mut c.text,
            "trigger_id" => &mut c.trigger_id,
            _ => continue,
        };
        *slot = v.into_owned();
    }
    for (name, v) in [
        ("team_id", &c.team_id),
        ("channel_id", &c.channel_id),
        ("user_id", &c.user_id),
        ("command", &c.command),
    ] {
        if v.is_empty() {
            return Err(format!("slash command without {name}"));
        }
    }
    Ok(c)
}

/// A stateful element's value: a select's chosen option(s) or a text input's text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selected {
    pub block_id: String,
    pub action_id: String,
    pub values: Vec<String>,
}

/// The element that was clicked or changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionHit {
    pub action_id: String,
    pub block_id: String,
    /// A button's value.
    pub value: Option<String>,
    /// A select's chosen option values.
    pub values: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockActions {
    pub team_id: String,
    pub user_id: String,
    pub user_name: String,
    /// The message's channel; `None` for actions in a modal.
    pub channel_id: Option<String>,
    pub message_ts: Option<String>,
    pub thread_ts: Option<String>,
    /// Opens a modal for the next 3 seconds.
    pub trigger_id: String,
    pub actions: Vec<ActionHit>,
    /// Every stateful element in the message, as the person left it.
    pub state: Vec<Selected>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewSubmission {
    pub team_id: String,
    pub user_id: String,
    pub callback_id: String,
    /// Ours, set when the modal was opened; echoed back as it was.
    pub private_metadata: String,
    pub state: Vec<Selected>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interaction {
    BlockActions(BlockActions),
    ViewSubmission(ViewSubmission),
    Other(String),
}

#[derive(Deserialize, Default)]
struct Named {
    #[serde(default)]
    id: String,
    username: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct Opt {
    value: String,
}

/// The shape shared by an action and a `state.values` entry.
#[derive(Deserialize, Default)]
struct Element {
    #[serde(default)]
    action_id: String,
    #[serde(default)]
    block_id: String,
    value: Option<String>,
    selected_option: Option<Opt>,
    selected_options: Option<Vec<Opt>>,
}

impl Element {
    fn chosen(&self) -> Vec<String> {
        match (&self.selected_option, &self.selected_options) {
            (Some(o), _) => vec![o.value.clone()],
            (None, Some(os)) => os.iter().map(|o| o.value.clone()).collect(),
            (None, None) => Vec::new(),
        }
    }
}

pub fn parse_interaction(body: &[u8]) -> Result<Interaction, String> {
    let payload = url::form_urlencoded::parse(body)
        .find(|(k, _)| k == "payload")
        .map(|(_, v)| v.into_owned())
        .ok_or("interaction without payload")?;
    #[derive(Deserialize)]
    struct Container {
        message_ts: Option<String>,
        thread_ts: Option<String>,
    }
    #[derive(Deserialize)]
    struct MessageRef {
        ts: Option<String>,
        thread_ts: Option<String>,
    }
    #[derive(Deserialize)]
    struct State {
        #[serde(default)]
        values: serde_json::Map<String, Value>,
    }
    #[derive(Deserialize)]
    struct View {
        #[serde(default)]
        callback_id: String,
        #[serde(default)]
        private_metadata: String,
        state: Option<State>,
    }
    #[derive(Deserialize)]
    struct Raw {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        team: Option<Named>,
        #[serde(default)]
        user: Named,
        channel: Option<Named>,
        container: Option<Container>,
        message: Option<MessageRef>,
        #[serde(default)]
        trigger_id: String,
        #[serde(default)]
        actions: Vec<Element>,
        state: Option<State>,
        view: Option<View>,
    }
    let raw: Raw = serde_json::from_str(&payload).map_err(|e| format!("slack interaction: {e}"))?;
    let team_id = raw.team.map(|t| t.id).unwrap_or_default();
    if raw.user.id.is_empty() {
        return Err("interaction without user".into());
    }
    match raw.kind.as_str() {
        "block_actions" => {
            let (container_ts, container_thread) = raw
                .container
                .map(|c| (c.message_ts, c.thread_ts))
                .unwrap_or_default();
            let (message_ts, message_thread) =
                raw.message.map(|m| (m.ts, m.thread_ts)).unwrap_or_default();
            Ok(Interaction::BlockActions(BlockActions {
                team_id,
                user_name: raw.user.username.or(raw.user.name).unwrap_or_default(),
                user_id: raw.user.id,
                channel_id: raw.channel.map(|c| c.id).filter(|c| !c.is_empty()),
                message_ts: message_ts.or(container_ts),
                thread_ts: message_thread.or(container_thread),
                trigger_id: raw.trigger_id,
                actions: raw
                    .actions
                    .into_iter()
                    .map(|a| ActionHit {
                        values: a.chosen(),
                        action_id: a.action_id,
                        block_id: a.block_id,
                        value: a.value,
                    })
                    .collect(),
                state: raw.state.map(|s| selected(s.values)).unwrap_or_default(),
            }))
        }
        "view_submission" => {
            let view = raw.view.ok_or("view_submission without view")?;
            Ok(Interaction::ViewSubmission(ViewSubmission {
                team_id,
                user_id: raw.user.id,
                callback_id: view.callback_id,
                private_metadata: view.private_metadata,
                state: view.state.map(|s| selected(s.values)).unwrap_or_default(),
            }))
        }
        other => Ok(Interaction::Other(other.to_owned())),
    }
}

/// `state.values`: `{block_id: {action_id: element}}`, flattened. Elements with nothing chosen
/// and nothing typed are kept with no values, so a skipped question is visible.
fn selected(values: serde_json::Map<String, Value>) -> Vec<Selected> {
    let mut out = Vec::new();
    for (block_id, actions) in values {
        let Value::Object(actions) = actions else {
            continue;
        };
        for (action_id, element) in actions {
            let e: Element = serde_json::from_value(element).unwrap_or_default();
            let values = match (&e.value, e.chosen()) {
                (Some(text), chosen) if chosen.is_empty() => vec![text.clone()],
                (_, chosen) => chosen,
            };
            out.push(Selected {
                block_id: block_id.clone(),
                action_id,
                values,
            });
        }
    }
    out
}

/// Escape text for mrkdwn: `&`, `<` and `>` become entities, so user text can't form links,
/// mentions or `<!channel>` pings.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// `text` without any mention of the bot (`<@U123>` or `<@U123|roll>`), trimmed.
pub fn strip_mention(text: &str, bot_user_id: &str) -> String {
    let needle = format!("<@{bot_user_id}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(&needle) {
        let after = &rest[i + needle.len()..];
        let end = match after.chars().next() {
            Some('>') => Some(1),
            Some('|') => after.find('>').map(|j| j + 1),
            _ => None,
        };
        match end {
            Some(end) => {
                out.push_str(&rest[..i]);
                rest = &after[end..];
            }
            // `<@U1234>` while the bot is `<@U123`: not a mention of us.
            None => {
                out.push_str(&rest[..i + needle.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn callback(event: Value) -> Vec<u8> {
        json!({
            "token": "ignored",
            "team_id": "T061EG9R6",
            "api_app_id": "A0PNCHHK2",
            "event": event,
            "type": "event_callback",
            "event_id": "Ev0PV52K25",
            "event_time": 1515449522,
            "authorizations": [{"team_id": "T061EG9R6", "user_id": "U0LAN0Z89", "is_bot": true}]
        })
        .to_string()
        .into_bytes()
    }

    fn form_payload(v: Value) -> Vec<u8> {
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("payload", &v.to_string())
            .finish()
            .into_bytes()
    }

    #[test]
    fn url_verification() {
        let body = br#"{"token":"x","challenge":"3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P","type":"url_verification"}"#;
        assert_eq!(
            parse_envelope(body).unwrap(),
            Envelope::UrlVerification {
                challenge: "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P".into()
            }
        );
        assert_eq!(
            parse_envelope(
                br#"{"type":"app_rate_limited","team_id":"T1","minute_rate_limited":1518467820}"#
            )
            .unwrap(),
            Envelope::Other("app_rate_limited".into())
        );
        assert!(parse_envelope(b"not json").is_err());
    }

    #[test]
    fn app_mention_in_a_thread() {
        let body = callback(json!({
            "type": "app_mention",
            "user": "U061F7AUR",
            "text": "<@U0LAN0Z89> fix the flaky test",
            "ts": "1515449522.000016",
            "thread_ts": "1515449000.000001",
            "channel": "C123ABC456",
            "event_ts": "1515449522000016"
        }));
        let Envelope::Callback {
            team_id,
            event_id,
            event_time,
            event,
        } = parse_envelope(&body).unwrap()
        else {
            panic!("not a callback");
        };
        assert_eq!(
            (team_id.as_str(), event_id.as_str(), event_time),
            ("T061EG9R6", "Ev0PV52K25", 1515449522)
        );
        let Event::AppMention(m) = event else {
            panic!("{event:?}")
        };
        assert_eq!(m.user.as_deref(), Some("U061F7AUR"));
        assert_eq!(m.thread_ts.as_deref(), Some("1515449000.000001"));
        assert_eq!(m.channel, "C123ABC456");
        assert_eq!(strip_mention(&m.text, "U0LAN0Z89"), "fix the flaky test");
    }

    #[test]
    fn messages_from_bots_and_with_subtypes() {
        let bot = callback(json!({
            "type": "message", "subtype": "bot_message", "bot_id": "B123", "text": "hi",
            "ts": "1.2", "channel": "C1", "channel_type": "channel"
        }));
        let Envelope::Callback {
            event: Event::Message(m),
            ..
        } = parse_envelope(&bot).unwrap()
        else {
            panic!()
        };
        assert_eq!(m.bot_id.as_deref(), Some("B123"));
        assert_eq!(m.subtype.as_deref(), Some("bot_message"));
        assert_eq!(m.user, None);
        // An edit has no top-level text or user.
        let edit = callback(json!({
            "type": "message", "subtype": "message_changed", "channel": "D1", "channel_type": "im",
            "hidden": true, "message": {"type": "message", "text": "new"}, "ts": "1.3"
        }));
        let Envelope::Callback {
            event: Event::Message(m),
            ..
        } = parse_envelope(&edit).unwrap()
        else {
            panic!()
        };
        assert_eq!(m.subtype.as_deref(), Some("message_changed"));
        assert_eq!(m.channel_type.as_deref(), Some("im"));
        assert_eq!(m.text, "");
    }

    #[test]
    fn reactions_on_messages_only() {
        let on_message = callback(json!({
            "type": "reaction_added", "user": "U024BE7LH", "reaction": "thumbsup",
            "item_user": "U0G9QF9C6",
            "item": {"type": "message", "channel": "C0G9QF9GZ", "ts": "1360782400.498405"},
            "event_ts": "1360782804.083113"
        }));
        let Envelope::Callback { event, .. } = parse_envelope(&on_message).unwrap() else {
            panic!()
        };
        assert_eq!(
            event,
            Event::ReactionAdded(Reaction {
                user: "U024BE7LH".into(),
                reaction: "thumbsup".into(),
                channel: "C0G9QF9GZ".into(),
                ts: "1360782400.498405".into(),
                item_user: Some("U0G9QF9C6".into()),
            })
        );
        let on_file = callback(json!({
            "type": "reaction_added", "user": "U1", "reaction": "eyes",
            "item": {"type": "file", "file": "F1"}, "event_ts": "1.1"
        }));
        let Envelope::Callback { event, .. } = parse_envelope(&on_file).unwrap() else {
            panic!()
        };
        assert_eq!(event, Event::Other("reaction_added".into()));
    }

    #[test]
    fn slash_command_form() {
        let body = b"token=gIkuvaNzQIHg97ATvDxqgjtO&team_id=T0001&team_domain=example&enterprise_id=E0001&channel_id=C2147483705&channel_name=test&user_id=U2147483697&user_name=Steve&command=%2Froll&text=fix+the+%3Cflaky%3E+test&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2F1234%2F5678&trigger_id=13345224609.738474920.8088930838d88f008e0&api_app_id=A123456";
        let c = parse_command(body).unwrap();
        assert_eq!(c.command, "/roll");
        assert_eq!(c.text, "fix the <flaky> test");
        assert_eq!(c.user_name, "Steve");
        assert_eq!(c.trigger_id, "13345224609.738474920.8088930838d88f008e0");
        assert!(
            parse_command(b"team_id=T1&user_id=U1&command=%2Froll")
                .unwrap_err()
                .contains("channel_id")
        );
    }

    #[test]
    fn block_actions_with_a_button_and_select_state() {
        let body = form_payload(json!({
            "type": "block_actions",
            "team": {"id": "T9TK3CUKW", "domain": "example"},
            "user": {"id": "UA8RXUSPL", "username": "jtorrance", "name": "jtorrance", "team_id": "T9TK3CUKW"},
            "api_app_id": "AABA1ABCD",
            "trigger_id": "12321423423.333649436676.d8c1bb837935619ccad0f624c448ffb3",
            "container": {"type": "message", "message_ts": "1548261231.000200", "channel_id": "CBR2V3XEX", "is_ephemeral": false, "thread_ts": "1548261000.000100"},
            "channel": {"id": "CBR2V3XEX", "name": "review-updates"},
            "message": {"type": "message", "ts": "1548261231.000200", "thread_ts": "1548261000.000100", "text": "x", "blocks": []},
            "state": {"values": {
                "roll.qb.scope": {"roll.q.scope": {"type": "static_select", "selected_option": {"text": {"type": "plain_text", "text": "Crate"}, "value": "crate"}}},
                "roll.qb.files": {"roll.q.files": {"type": "multi_static_select", "selected_options": [{"value": "a"}, {"value": "b"}]}},
                "roll.qb.why": {"roll.q.why": {"type": "plain_text_input", "value": "because"}},
                "roll.qb.empty": {"roll.q.empty": {"type": "static_select", "selected_option": null}}
            }},
            "actions": [{
                "action_id": "roll.submit", "block_id": "roll.submit",
                "text": {"type": "plain_text", "text": "Submit"}, "value": "req_1",
                "style": "primary", "type": "button", "action_ts": "1548426417.840180"
            }]
        }));
        let Interaction::BlockActions(a) = parse_interaction(&body).unwrap() else {
            panic!()
        };
        assert_eq!(a.team_id, "T9TK3CUKW");
        assert_eq!(
            (a.user_id.as_str(), a.user_name.as_str()),
            ("UA8RXUSPL", "jtorrance")
        );
        assert_eq!(a.channel_id.as_deref(), Some("CBR2V3XEX"));
        assert_eq!(a.message_ts.as_deref(), Some("1548261231.000200"));
        assert_eq!(a.thread_ts.as_deref(), Some("1548261000.000100"));
        assert_eq!(
            a.actions,
            vec![ActionHit {
                action_id: "roll.submit".into(),
                block_id: "roll.submit".into(),
                value: Some("req_1".into()),
                values: vec![],
            }]
        );
        let get = |b: &str| {
            a.state
                .iter()
                .find(|s| s.block_id == b)
                .unwrap()
                .values
                .clone()
        };
        assert_eq!(get("roll.qb.scope"), vec!["crate"]);
        assert_eq!(get("roll.qb.files"), vec!["a", "b"]);
        assert_eq!(get("roll.qb.why"), vec!["because"]);
        assert!(get("roll.qb.empty").is_empty());
    }

    #[test]
    fn select_action_carries_its_values() {
        let body = form_payload(json!({
            "type": "block_actions", "team": {"id": "T1"}, "user": {"id": "U1", "name": "ada"},
            "trigger_id": "t", "container": {"type": "view", "view_id": "V1"},
            "actions": [{"type": "static_select", "action_id": "roll.q.scope", "block_id": "roll.qb.scope",
                         "selected_option": {"value": "crate"}}]
        }));
        let Interaction::BlockActions(a) = parse_interaction(&body).unwrap() else {
            panic!()
        };
        assert_eq!(a.user_name, "ada");
        assert_eq!(a.channel_id, None);
        assert_eq!(a.actions[0].values, vec!["crate"]);
        assert_eq!(a.actions[0].value, None);
    }

    #[test]
    fn view_submission() {
        let body = form_payload(json!({
            "type": "view_submission",
            "team": {"id": "T1", "domain": "x"},
            "user": {"id": "U1", "username": "ada"},
            "view": {
                "id": "V1", "type": "modal", "callback_id": "roll.answer",
                "private_metadata": "{\"req\":\"r1\"}",
                "state": {"values": {"roll.text": {"roll.text": {"type": "plain_text_input", "value": "line one\nline two"}}}}
            }
        }));
        assert_eq!(
            parse_interaction(&body).unwrap(),
            Interaction::ViewSubmission(ViewSubmission {
                team_id: "T1".into(),
                user_id: "U1".into(),
                callback_id: "roll.answer".into(),
                private_metadata: "{\"req\":\"r1\"}".into(),
                state: vec![Selected {
                    block_id: "roll.text".into(),
                    action_id: "roll.text".into(),
                    values: vec!["line one\nline two".into()],
                }],
            })
        );
        let closed = form_payload(json!({"type": "view_closed", "user": {"id": "U1"}}));
        assert_eq!(
            parse_interaction(&closed).unwrap(),
            Interaction::Other("view_closed".into())
        );
        assert!(parse_interaction(b"x=1").is_err());
    }

    #[test]
    fn escape_and_strip_mention() {
        assert_eq!(
            escape("<script>&<!channel>"),
            "&lt;script&gt;&amp;&lt;!channel&gt;"
        );
        assert_eq!(
            strip_mention("<@U1> hi <@U1|roll> there", "U1"),
            "hi  there"
        );
        assert_eq!(strip_mention("<@U12> hi", "U1"), "<@U12> hi");
        assert_eq!(
            strip_mention("hi <@U1|unterminated", "U1"),
            "hi <@U1|unterminated"
        );
        assert_eq!(strip_mention("  <@U1>  ", "U1"), "");
    }
}
