//! The `linear` trigger source: issues, comments and agent sessions from the owner's workspace
//! ([`super::webhook`] records them). Its config narrows which events start runs, before the
//! automation's own filters.

use super::SOURCE;
use super::store;
use crate::Code;
use crate::agents::automations::{OwnerRef, props, TriggerSource};
use agent_proto::automation::{AutomationEvent, EventInfo, TriggerInfo};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Weak;

pub const EVENTS: [&str; 7] =
    ["issue.created", "issue.updated", "issue.removed", "comment.created", "comment.updated", "agent_session.created", "agent_session.prompted"];

/// A `linear` trigger's config. Every list that's set must have the event's value in it (names
/// and keys compare case-insensitively).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Team keys (`ENG`), names or ids.
    #[serde(default)]
    pub teams: Vec<String>,
    /// Project names or ids.
    #[serde(default)]
    pub projects: Vec<String>,
    /// The issue has one of these labels.
    #[serde(default)]
    pub labels: Vec<String>,
    /// An update added one of these labels.
    #[serde(default)]
    pub label_added: Vec<String>,
    /// The issue's state (a name, `In Progress`, or a type, `started`) is one of these.
    #[serde(default)]
    pub states: Vec<String>,
    /// An update moved the issue into one of these states (names or types).
    #[serde(default)]
    pub state_changed_to: Vec<String>,
    /// The assignee (`roll` for roll's own user, `none` for nobody, or a name, email or id).
    #[serde(default)]
    pub assignees: Vec<String>,
    /// Priorities: 0 to 4, or `none`, `urgent`, `high`, `medium`, `low`.
    #[serde(default)]
    pub priorities: Vec<Value>,
    /// Words the title or description holds (case-insensitive), or a comment's body.
    #[serde(default)]
    pub matches: Option<String>,
    /// Comments: only those that mention roll.
    #[serde(default)]
    pub mentions_roll: bool,
    /// Also events roll itself caused (its own comments, its own updates). Off: an automation
    /// that comments doesn't trigger itself.
    #[serde(default)]
    pub include_roll: bool,
    /// `agent_session.prompted`: also replies a run already showing the session takes.
    #[serde(default)]
    pub include_attached: bool,
}

pub fn priority_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().filter(|p| (0..=4).contains(p)),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "none" | "no priority" => Some(0),
            "urgent" => Some(1),
            "high" => Some(2),
            "medium" | "normal" => Some(3),
            "low" => Some(4),
            n => n.parse().ok().filter(|p| (0..=4).contains(p)),
        },
        _ => None,
    }
}

fn any_eq(list: &[String], vals: &[&str]) -> bool {
    list.iter().any(|w| vals.iter().any(|v| !v.is_empty() && w.trim().eq_ignore_ascii_case(v)))
}

fn s<'a>(v: &'a Value, path: &[&str]) -> &'a str {
    path.iter().fold(v, |v, k| &v[*k]).as_str().unwrap_or_default()
}

impl Config {
    /// Whether an event's data passes.
    pub fn passes(&self, kind: &str, d: &Value) -> bool {
        if !self.include_roll && d["by_roll"].as_bool() == Some(true) {
            return false;
        }
        if kind == "agent_session.prompted" && !self.include_attached && d["attached"].as_bool() == Some(true) {
            return false;
        }
        let issue = &d["issue"];
        if !self.teams.is_empty() && !any_eq(&self.teams, &[s(issue, &["team", "key"]), s(issue, &["team", "name"]), s(issue, &["team", "id"])]) {
            return false;
        }
        if !self.projects.is_empty() && !any_eq(&self.projects, &[s(issue, &["project", "name"]), s(issue, &["project", "id"])]) {
            return false;
        }
        if !self.labels.is_empty() {
            let names: Vec<&str> = issue["labels"].as_array().map(|ls| ls.iter().flat_map(|l| [s(l, &["name"]), s(l, &["id"])]).collect()).unwrap_or_default();
            if !any_eq(&self.labels, &names) {
                return false;
            }
        }
        if !self.label_added.is_empty() {
            let ids = d["changes"]["labels_added_ids"].as_array().into_iter().flatten();
            let added: Vec<&str> = d["changes"]["labels_added"].as_array().into_iter().flatten().chain(ids).filter_map(Value::as_str).collect();
            if !any_eq(&self.label_added, &added) {
                return false;
            }
        }
        if !self.states.is_empty() && !any_eq(&self.states, &[s(issue, &["state", "name"]), s(issue, &["state", "type"]), s(issue, &["state", "id"])]) {
            return false;
        }
        if !self.state_changed_to.is_empty() {
            let st = &d["changes"]["state"];
            if !st.is_object() || !any_eq(&self.state_changed_to, &[s(st, &["to"]), s(st, &["to_type"]), s(st, &["to_id"])]) {
                return false;
            }
        }
        if !self.assignees.is_empty() {
            let a = &issue["assignee"];
            let ok = self.assignees.iter().any(|w| match w.trim().to_ascii_lowercase().as_str() {
                "none" | "nobody" | "unassigned" => !a.is_object(),
                // roll as the assignee, or as the agent the issue is delegated to.
                "roll" => d["roll_user"].as_str().is_some_and(|u| s(a, &["id"]) == u || s(issue, &["delegate", "id"]) == u),
                _ => any_eq(std::slice::from_ref(w), &[s(a, &["id"]), s(a, &["name"]), s(a, &["email"])]),
            });
            if !ok {
                return false;
            }
        }
        if !self.priorities.is_empty() {
            let p = issue["priority"].as_i64();
            if !self.priorities.iter().filter_map(priority_of).any(|w| Some(w) == p) {
                return false;
            }
        }
        if let Some(m) = self.matches.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            let m = m.to_lowercase();
            let hay = format!("{}\n{}\n{}\n{}", s(issue, &["title"]), s(issue, &["description"]), s(d, &["comment", "body"]), s(d, &["prompt"])).to_lowercase();
            if !hay.contains(&m) {
                return false;
            }
        }
        if self.mentions_roll && kind.starts_with("comment.") && d["mentions_roll"].as_bool() != Some(true) {
            return false;
        }
        true
    }
}

pub struct LinearTrigger {
    pub code: Weak<Code>,
}

/// The kinds of workflow state, which match a state of that kind in any team.
pub const STATE_TYPES: &[(&str, &str)] =
    &[("triage", "Triage"), ("backlog", "Backlog"), ("unstarted", "Unstarted"), ("started", "Started"), ("completed", "Completed"), ("canceled", "Canceled")];

/// Linear's priorities, as `priority_of` reads them.
pub const PRIORITIES: &[(&str, &str)] = &[("0", "No priority"), ("1", "Urgent"), ("2", "High"), ("3", "Medium"), ("4", "Low")];

pub fn config_schema() -> Value {
    use crate::agents::lookup::Pick;
    let states = Pick { parent: Some("teams"), extras: STATE_TYPES, extras_group: Some("Any team"), ..Pick::from(SOURCE, "states") };
    let labels = Pick { parent: Some("teams"), ..Pick::from(SOURCE, "labels") };
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": props!({
            "teams": Pick::from(SOURCE, "teams").many("Teams", "Only issues of these teams"),
            "projects": Pick::from(SOURCE, "projects").many("Projects", "Only issues in these projects"),
            "labels": labels.many("Labels", "The issue has one of these labels"),
            "label_added": labels.many("Label added", "An update added one of these labels"),
            "states": states.many("States", "The issue is in one of these states (a kind of state matches it in any team)"),
            "state_changed_to": states.many("State changed to", "An update moved it into one of these states"),
            "assignees": Pick { extras: &[("roll", "roll"), ("none", "No one")], ..Pick::from(SOURCE, "users") }.many("Assignees", "Assigned to one of these (roll: roll is assigned or delegated)"),
            "priorities": Pick { extras: PRIORITIES, ..Pick::default() }.many("Priorities", "The issue has one of these priorities"),
            "matches": { "type": "string", "description": "Words in the title, description or comment (case-insensitive)" },
            "mentions_roll": { "type": "boolean", "description": "Comments: only those that mention roll" },
            "include_roll": { "type": "boolean", "description": "Also events roll caused itself" },
            "include_attached": { "type": "boolean", "description": "Session replies a running automation already takes" },
        }),
    })
}

/// A realistic `event.data` of each kind (the webhook's shape), for dry runs and autocomplete.
pub fn sample(kind: &str) -> Value {
    let issue = json!({
        "id": "9cfb4d1e-1b5c-4c4a-9d55-3f0f3b8a2f10", "identifier": "ENG-12", "number": 12, "title": "Login fails with SSO",
        "description": "Signing in with Okta loops back to the login page.", "url": "https://linear.app/acme/issue/ENG-12",
        "priority": 2, "priority_label": "High", "team": { "id": "team-eng", "key": "ENG", "name": "Engineering" },
        "state": { "id": "state-todo", "name": "Todo", "type": "unstarted" }, "labels": [{ "id": "label-bug", "name": "Bug" }],
        "assignee": { "id": "user-ada", "name": "Ada Lovelace", "email": "ada@acme.dev" }, "delegate": null,
        "project": { "id": "project-auth", "name": "Auth" }, "creator_id": "user-ada",
    });
    let base = json!({
        "issue": issue, "repo": "app", "thread": null, "by_roll": false, "roll_user": "user-roll",
        "actor": { "id": "user-ada", "name": "Ada Lovelace", "type": "user" },
        "workspace": { "id": "org-acme", "name": "Acme", "url_key": "acme" },
    });
    let mut d = base;
    match kind {
        "issue.updated" => {
            d["changes"] = json!({ "fields": ["stateId", "labelIds"], "state": { "from": "state-backlog", "to": "Todo", "to_type": "unstarted" }, "labels_added": ["roll"], "labels_removed": [] });
        }
        "comment.created" | "comment.updated" => {
            d["comment"] = json!({ "id": "comment-1", "body": "@roll can you take this?", "url": "https://linear.app/acme/issue/ENG-12#comment-1", "parent_id": null, "user": { "id": "user-ada", "name": "Ada Lovelace" } });
            d["mentions_roll"] = json!(true);
            d["in_session"] = json!(false);
        }
        "agent_session.created" | "agent_session.prompted" => {
            d["session"] = json!({ "id": "session-1", "url": "https://linear.app/acme/agent/session-1", "status": "pending" });
            d["comment"] = json!({ "id": "comment-1", "body": "@roll please fix this" });
            d["prompt"] = json!(if kind == "agent_session.created" { "@roll please fix this" } else { "Use the new SSO client" });
            d["prompt_context"] = json!("<issue identifier=\"ENG-12\"><title>Login fails with SSO</title></issue>");
            d["user"] = json!({ "id": "user-ada", "name": "Ada Lovelace" });
            d["attached"] = json!(false);
            if kind == "agent_session.prompted" {
                d["signal"] = Value::Null;
            }
        }
        _ => {}
    }
    d
}

fn info() -> TriggerInfo {
    let issue = ["issue.identifier", "issue.title", "issue.team.key", "issue.state.name", "issue.state.type", "issue.labels", "issue.assignee.name", "issue.priority", "issue.project.name", "repo", "thread.thread"];
    let ev = |kind: &str, title: &str, extra: &[&str]| EventInfo {
        kind: kind.into(),
        title: title.into(),
        fields: issue.iter().chain(extra).map(|s| s.to_string()).collect(),
        sample: sample(kind),
    };
    TriggerInfo {
        source: SOURCE.into(),
        title: "Linear".into(),
        description: "Issues, comments, and roll assigned or mentioned as an agent in the owner's Linear workspace".into(),
        events: vec![
            ev("issue.created", "Issue created", &["actor.name"]),
            ev("issue.updated", "Issue updated", &["changes.fields", "changes.state.to", "changes.labels_added", "changes.assignee.to", "changes.priority.to", "actor.name"]),
            ev("issue.removed", "Issue removed", &[]),
            ev("comment.created", "Comment created", &["comment.body", "comment.user.name", "mentions_roll"]),
            ev("comment.updated", "Comment edited", &["comment.body", "mentions_roll"]),
            ev("agent_session.created", "roll assigned or mentioned", &["session.id", "prompt", "prompt_context", "user.name"]),
            ev("agent_session.prompted", "A reply to roll in its session", &["session.id", "prompt", "signal", "user.name", "attached"]),
        ],
        config: config_schema(),
        needs_setup: false,
    }
}

impl TriggerSource for LinearTrigger {
    fn info(&self) -> TriggerInfo {
        info()
    }

    fn check(&self, owner: &OwnerRef, events: &[String], config: &Value) -> Result<(), String> {
        for e in events {
            if !EVENTS.contains(&e.as_str()) {
                return Err(format!("Linear has no {e:?} event (it has {})", EVENTS.join(", ")));
            }
        }
        let c: Config = if config.is_null() { Config::default() } else { serde_json::from_value(config.clone()).map_err(|e| format!("trigger.config: {e}"))? };
        if let Some(p) = c.priorities.iter().find(|p| priority_of(p).is_none()) {
            return Err(format!("trigger.config.priorities: {p} isn't 0–4 or none, urgent, high, medium, low"));
        }
        let Some(code) = self.code.upgrade() else { return Ok(()) };
        let conn = store::connection(code.db.sql(), &owner.id).map_err(|e| e.to_string())?;
        let Some(conn) = conn else { return Err("connect Linear first (the owner's settings)".into()) };
        let sessions = events.is_empty() || events.iter().any(|e| e.starts_with("agent_session."));
        if sessions && events.iter().any(|e| e.starts_with("agent_session.")) && !conn.agent() {
            return Err("agent sessions need roll connected as a Linear app (OAuth), not an API key".into());
        }
        Ok(())
    }

    fn matches(&self, config: &Value, event: &AutomationEvent) -> bool {
        let c: Config = if config.is_null() { Config::default() } else { serde_json::from_value(config.clone()).unwrap_or_default() };
        c.passes(&event.kind, &event.data)
    }

    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        self.code.upgrade().is_some_and(|code| !matches!(store::connection(code.db.sql(), &owner.id), Ok(Some(_))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_event() -> Value {
        json!({
            "issue": {
                "title": "Crash on login", "description": "Stack trace attached",
                "team": { "id": "t1", "key": "ENG", "name": "Engineering" },
                "project": { "id": "p1", "name": "Auth" },
                "state": { "id": "s2", "name": "In Progress", "type": "started" },
                "labels": [{ "id": "l1", "name": "Bug" }],
                "assignee": { "id": "u1", "name": "Ada", "email": "ada@acme.dev" },
                "delegate": { "id": "app-user" },
                "priority": 1,
            },
            "changes": { "labels_added": ["roll"], "state": { "to": "In Progress", "to_type": "started", "to_id": "s2" } },
            "roll_user": "app-user",
            "by_roll": false,
        })
    }

    fn cfg(v: Value) -> Config {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn every_set_condition_must_hold() {
        let d = issue_event();
        assert!(Config::default().passes("issue.updated", &d));
        for ok in [
            json!({ "teams": ["eng"] }),
            json!({ "teams": ["Engineering"], "projects": ["auth"] }),
            json!({ "labels": ["bug"] }),
            json!({ "label_added": ["ROLL"] }),
            json!({ "states": ["started"] }),
            json!({ "state_changed_to": ["in progress"] }),
            json!({ "assignees": ["ada@acme.dev"] }),
            json!({ "assignees": ["roll"] }),
            json!({ "priorities": ["urgent"] }),
            json!({ "priorities": [1] }),
            json!({ "matches": "LOGIN" }),
        ] {
            assert!(cfg(ok.clone()).passes("issue.updated", &d), "{ok}");
        }
        for no in [
            json!({ "teams": ["OPS"] }),
            json!({ "labels": ["feature"] }),
            json!({ "label_added": ["bug"] }),
            json!({ "states": ["completed"] }),
            json!({ "assignees": ["none"] }),
            json!({ "priorities": ["low", 0] }),
            json!({ "matches": "signup" }),
            json!({ "teams": ["ENG"], "labels": ["feature"] }),
        ] {
            assert!(!cfg(no.clone()).passes("issue.updated", &d), "{no}");
        }
    }

    #[test]
    fn roll_s_own_events_and_attached_replies_are_left_out_unless_asked() {
        let mut d = issue_event();
        d["by_roll"] = json!(true);
        assert!(!Config::default().passes("issue.updated", &d));
        assert!(cfg(json!({ "include_roll": true })).passes("issue.updated", &d));
        let r = json!({ "attached": true });
        assert!(!Config::default().passes("agent_session.prompted", &r));
        assert!(cfg(json!({ "include_attached": true })).passes("agent_session.prompted", &r));
        assert!(Config::default().passes("agent_session.prompted", &json!({ "attached": false })));
    }

    #[test]
    fn comments_can_require_a_mention() {
        let c = cfg(json!({ "mentions_roll": true }));
        assert!(c.passes("comment.created", &json!({ "mentions_roll": true })));
        assert!(!c.passes("comment.created", &json!({ "mentions_roll": false })));
        assert!(c.passes("issue.created", &json!({})), "only comments have mentions");
    }

    #[test]
    fn priorities_take_numbers_and_names() {
        assert_eq!(priority_of(&json!(0)), Some(0));
        assert_eq!(priority_of(&json!("High")), Some(2));
        assert_eq!(priority_of(&json!("normal")), Some(3));
        assert_eq!(priority_of(&json!("4")), Some(4));
        assert_eq!(priority_of(&json!(5)), None);
        assert_eq!(priority_of(&json!("soon")), None);
    }

    #[test]
    fn unknown_config_fields_are_refused() {
        assert!(serde_json::from_value::<Config>(json!({ "team": "ENG" })).is_err());
    }
}
