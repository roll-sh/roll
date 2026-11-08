//! Interactions, parsed from the raw (already verified) body. The person is `member.user` in a
//! guild and `user` in a DM. Types roll doesn't act on (autocomplete, …) parse to
//! [`Interaction::Other`] with their type number.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Interaction {
    /// Discord checking the endpoint: answer [`super::respond::pong`].
    Ping,
    /// A slash command (`/roll prompt:…`).
    Command {
        id: String,
        /// Answers and edits this interaction for 15 minutes. A secret: never log it.
        token: String,
        guild_id: Option<String>,
        channel_id: String,
        user_id: String,
        user_name: String,
        name: String,
        /// String, number and boolean options as text; a subcommand's name under `subcommand`.
        options: BTreeMap<String, String>,
    },
    /// A button click or select change on a message.
    Component {
        id: String,
        token: String,
        guild_id: Option<String>,
        channel_id: String,
        user_id: String,
        user_name: String,
        message_id: String,
        custom_id: String,
        /// A select's chosen values; empty for a button.
        values: Vec<String>,
    },
    ModalSubmit {
        id: String,
        token: String,
        guild_id: Option<String>,
        channel_id: String,
        user_id: String,
        user_name: String,
        custom_id: String,
        /// Text inputs by `custom_id`.
        fields: BTreeMap<String, String>,
    },
    Other(u64),
}

#[derive(Deserialize)]
struct User {
    id: String,
    #[serde(default)]
    username: String,
}

#[derive(Deserialize)]
struct Member {
    user: Option<User>,
}

#[derive(Deserialize)]
struct Channel {
    id: String,
}

#[derive(Deserialize)]
struct Message {
    id: String,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(rename = "type")]
    kind: u64,
    #[serde(default)]
    id: String,
    #[serde(default)]
    token: String,
    guild_id: Option<String>,
    channel_id: Option<String>,
    channel: Option<Channel>,
    member: Option<Member>,
    user: Option<User>,
    message: Option<Message>,
    #[serde(default)]
    data: Value,
}

pub fn parse_interaction(body: &[u8]) -> Result<Interaction, String> {
    let raw: Raw = serde_json::from_slice(body).map_err(|e| format!("discord interaction: {e}"))?;
    if raw.kind == 1 {
        return Ok(Interaction::Ping);
    }
    if !matches!(raw.kind, 2 | 3 | 5) {
        return Ok(Interaction::Other(raw.kind));
    }
    let user = raw
        .member
        .and_then(|m| m.user)
        .or(raw.user)
        .ok_or("interaction without a user")?;
    let channel_id = raw
        .channel_id
        .or(raw.channel.map(|c| c.id))
        .ok_or("interaction without a channel")?;
    if raw.id.is_empty() || raw.token.is_empty() {
        return Err("interaction without id or token".into());
    }
    let data = &raw.data;
    let str_of = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_owned);
    Ok(match raw.kind {
        2 => {
            let mut options = BTreeMap::new();
            flatten_options(data.get("options"), &mut options);
            Interaction::Command {
                id: raw.id,
                token: raw.token,
                guild_id: raw.guild_id,
                channel_id,
                user_id: user.id,
                user_name: user.username,
                name: str_of("name").ok_or("command without a name")?,
                options,
            }
        }
        3 => Interaction::Component {
            id: raw.id,
            token: raw.token,
            guild_id: raw.guild_id,
            channel_id,
            user_id: user.id,
            user_name: user.username,
            message_id: raw
                .message
                .map(|m| m.id)
                .ok_or("component without a message")?,
            custom_id: str_of("custom_id").ok_or("component without a custom_id")?,
            values: data
                .get("values")
                .and_then(Value::as_array)
                .map(|vs| {
                    vs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        },
        _ => {
            let mut fields = BTreeMap::new();
            collect_fields(data.get("components"), &mut fields);
            Interaction::ModalSubmit {
                id: raw.id,
                token: raw.token,
                guild_id: raw.guild_id,
                channel_id,
                user_id: user.id,
                user_name: user.username,
                custom_id: str_of("custom_id").ok_or("modal without a custom_id")?,
                fields,
            }
        }
    })
}

/// Command options: type 1 (subcommand) and 2 (group) nest; leaves become text.
fn flatten_options(options: Option<&Value>, out: &mut BTreeMap<String, String>) {
    let Some(options) = options.and_then(Value::as_array) else {
        return;
    };
    for o in options {
        let Some(name) = o.get("name").and_then(Value::as_str) else {
            continue;
        };
        match o.get("type").and_then(Value::as_u64) {
            Some(1) => {
                out.insert("subcommand".into(), name.to_owned());
                flatten_options(o.get("options"), out);
            }
            Some(2) => {
                out.insert("group".into(), name.to_owned());
                flatten_options(o.get("options"), out);
            }
            _ => {
                let value = match o.get("value") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v @ (Value::Number(_) | Value::Bool(_))) => v.to_string(),
                    _ => continue,
                };
                out.insert(name.to_owned(), value);
            }
        }
    }
}

/// Text inputs in a modal submit: inside action rows (`components`) or, in newer modals, label
/// components (`component`).
fn collect_fields(components: Option<&Value>, out: &mut BTreeMap<String, String>) {
    let Some(components) = components.and_then(Value::as_array) else {
        return;
    };
    for c in components {
        if let (Some(id), Some(value)) = (
            c.get("custom_id").and_then(Value::as_str),
            c.get("value").and_then(Value::as_str),
        ) {
            out.insert(id.to_owned(), value.to_owned());
        }
        collect_fields(c.get("components"), out);
        if let Some(inner) = c.get("component") {
            collect_fields(Some(&Value::Array(vec![inner.clone()])), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(v: Value) -> Interaction {
        parse_interaction(v.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn ping_and_unknown() {
        assert_eq!(
            parse(json!({"type": 1, "id": "1", "application_id": "2", "token": "t", "version": 1})),
            Interaction::Ping
        );
        assert_eq!(
            parse(json!({"type": 4, "id": "1", "token": "t"})),
            Interaction::Other(4)
        );
        assert!(parse_interaction(b"{").is_err());
    }

    #[test]
    fn guild_command_with_subcommand() {
        let i = parse(json!({
            "type": 2, "id": "1100", "application_id": "99", "token": "aW50ZXJhY3Rpb24",
            "guild_id": "500", "channel_id": "600",
            "channel": {"id": "600", "type": 0},
            "member": {"user": {"id": "700", "username": "ada", "global_name": "Ada"}, "roles": []},
            "data": {"id": "800", "name": "roll", "type": 1, "options": [
                {"name": "ask", "type": 1, "options": [
                    {"name": "prompt", "type": 3, "value": "fix the flaky test"},
                    {"name": "steps", "type": 4, "value": 3},
                    {"name": "draft", "type": 5, "value": true}
                ]}
            ]}
        }));
        let Interaction::Command {
            guild_id,
            channel_id,
            user_id,
            user_name,
            name,
            options,
            token,
            ..
        } = i
        else {
            panic!("{i:?}")
        };
        assert_eq!(guild_id.as_deref(), Some("500"));
        assert_eq!(
            (channel_id.as_str(), user_id.as_str(), user_name.as_str()),
            ("600", "700", "ada")
        );
        assert_eq!((name.as_str(), token.as_str()), ("roll", "aW50ZXJhY3Rpb24"));
        assert_eq!(options["subcommand"], "ask");
        assert_eq!(options["prompt"], "fix the flaky test");
        assert_eq!(options["steps"], "3");
        assert_eq!(options["draft"], "true");
    }

    #[test]
    fn dm_command_takes_the_top_level_user() {
        let i = parse(json!({
            "type": 2, "id": "1", "token": "t", "channel": {"id": "42", "type": 1},
            "user": {"id": "7", "username": "bob"},
            "data": {"name": "roll", "options": [{"name": "prompt", "type": 3, "value": "hi"}]}
        }));
        let Interaction::Command {
            guild_id,
            channel_id,
            user_id,
            options,
            ..
        } = i
        else {
            panic!()
        };
        assert_eq!(
            (guild_id, channel_id.as_str(), user_id.as_str()),
            (None, "42", "7")
        );
        assert_eq!(options.len(), 1);
    }

    #[test]
    fn component_select_and_button() {
        let select = parse(json!({
            "type": 3, "id": "2", "token": "t2", "guild_id": "500", "channel_id": "600",
            "member": {"user": {"id": "700", "username": "ada"}},
            "message": {"id": "900", "channel_id": "600", "content": ""},
            "data": {"custom_id": "roll.q:req_1:scope", "component_type": 3, "values": ["crate", "docs"]}
        }));
        assert_eq!(
            select,
            Interaction::Component {
                id: "2".into(),
                token: "t2".into(),
                guild_id: Some("500".into()),
                channel_id: "600".into(),
                user_id: "700".into(),
                user_name: "ada".into(),
                message_id: "900".into(),
                custom_id: "roll.q:req_1:scope".into(),
                values: vec!["crate".into(), "docs".into()],
            }
        );
        let button = parse(json!({
            "type": 3, "id": "3", "token": "t3", "channel_id": "600",
            "user": {"id": "7", "username": "bob"}, "message": {"id": "901"},
            "data": {"custom_id": "allow", "component_type": 2}
        }));
        let Interaction::Component {
            values, custom_id, ..
        } = button
        else {
            panic!()
        };
        assert!(values.is_empty());
        assert_eq!(custom_id, "allow");
    }

    #[test]
    fn modal_submit_in_rows_and_labels() {
        let i = parse(json!({
            "type": 5, "id": "4", "token": "t4", "guild_id": "500", "channel_id": "600",
            "member": {"user": {"id": "700", "username": "ada"}},
            "data": {"custom_id": "roll.a:req_1:why", "components": [
                {"type": 1, "components": [{"type": 4, "custom_id": "roll.text", "value": "because"}]},
                {"type": 18, "id": 3, "component": {"type": 4, "custom_id": "extra", "value": "more"}}
            ]}
        }));
        let Interaction::ModalSubmit {
            custom_id, fields, ..
        } = i
        else {
            panic!()
        };
        assert_eq!(custom_id, "roll.a:req_1:why");
        assert_eq!(fields["roll.text"], "because");
        assert_eq!(fields["extra"], "more");
    }

    #[test]
    fn missing_user_is_an_error() {
        let err = parse_interaction(json!({"type": 2, "id": "1", "token": "t", "channel_id": "1", "data": {"name": "roll"}}).to_string().as_bytes()).unwrap_err();
        assert!(err.contains("user"), "{err}");
    }
}
