//! Linear's webhooks (https://linear.app/developers/webhooks,
//! https://linear.app/developers/agent-interaction): verified, turned into automation events, and
//! recorded in the owner's inbox before the 200.
//!
//! - **Where.** `POST /v1/code/linear/webhook` takes the deliveries of roll's OAuth app (one
//!   signing secret for every workspace, `ROLL_LINEAR_WEBHOOK_SECRET`; the workspace is the
//!   payload's `organizationId`). `POST /v1/code/linear/webhook/{hook}` takes a connection's own
//!   webhook (an API key's, made by roll or by hand, with its own secret).
//! - **Checks.** `Linear-Signature` is the hex HMAC-SHA256 of the raw body under the secret,
//!   compared in constant time; the body's `webhookTimestamp` (unix ms, inside what's signed)
//!   must be within [`REPLAY_WINDOW_MS`] of now, so a delivery captured once can't be replayed
//!   later. Within the window a replay is the same event: inbox keys are Linear's own ids
//!   (`linear:issue.created:<issue>`, `linear:comment.created:<comment>`,
//!   `linear:agent_session.created:<session>`, `linear:agent_session.prompted:<activity>`, an
//!   update's id and `updatedAt`), so it runs nothing twice.
//! - **One write.** The events, a session's bookkeeping (its thread, its prompts) and the
//!   delivery's log line are one transaction; a prompt for a session a run is showing wakes that
//!   run in it. Linear wants an answer within 5 seconds: nothing here calls out.


use super::store::{self, Connection, Kind};
use super::{SOURCE, client::LinearError};
use crate::agents::automations::registry::{listens, wake_run};
use crate::agents::automations::{NewEvent, record_event};
use crate::{Code, Error, Result};
use code_sql::Conn;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use std::sync::Arc;

/// How far a delivery's `webhookTimestamp` may be from now.
pub const REPLAY_WINDOW_MS: i64 = 60_000;
/// Largest delivery read.
pub const MAX_BODY: usize = 1 << 20;
/// Longest text kept from an issue's description or a comment.
const TEXT: usize = 16 << 10;

/// Whether `signature` (hex) is the HMAC-SHA256 of `body` under `secret`.
pub fn verify(secret: &str, body: &[u8], signature: &str) -> bool {
    let Ok(sig) = hex::decode(signature.trim()) else { return false };
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

/// The signature Linear would send (tests and the mock server).
pub fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

/// Whether a payload's `webhookTimestamp` is within the window.
pub fn fresh(payload: &Value, now_ms: i64) -> bool {
    payload["webhookTimestamp"].as_f64().is_some_and(|t| (now_ms as f64 - t).abs() <= REPLAY_WINDOW_MS as f64)
}

/// What a delivery did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Received {
    /// Events new to the inbox.
    pub recorded: usize,
    /// Events it carried that were recorded before.
    pub duplicates: usize,
    /// No connection for its workspace, or a kind roll doesn't use.
    pub ignored: Option<String>,
}

/// Which secret a delivery is checked with.
pub enum Endpoint<'a> {
    /// roll's OAuth app's.
    App,
    /// A connection's own (`/webhook/{hook}`).
    Hook(&'a str),
}

/// Check and record one delivery.
pub fn receive(code: &Arc<Code>, at: Endpoint<'_>, delivery: &str, signature: Option<&str>, body: &[u8]) -> Result<Received> {
    let db = code.db.sql();
    let (secret, conn) = match at {
        Endpoint::App => {
            let Some(s) = code.linear.config().webhook_secret else { return Err(Error::NotFound("roll's Linear app has no webhook here (ROLL_LINEAR_WEBHOOK_SECRET)".into())) };
            (s, None)
        }
        Endpoint::Hook(h) => {
            let Some(c) = store::connection_by_hook(db, h)? else { return Err(Error::NotFound("no such Linear webhook".into())) };
            let Some(sealed) = c.hook_secret.as_deref() else {
                store::rejected(db, &c.owner_id, "the webhook's signing secret isn't set in roll yet")?;
                return Err(Error::Unauthorized);
            };
            let s = code.open_linear(&c.owner_id, "hook", sealed).map_err(|e| Error::Unavailable(e.to_string()))?;
            (s, Some(c))
        }
    };
    let reject = |why: &str| -> Result<Received> {
        if let Some(c) = &conn {
            store::rejected(db, &c.owner_id, why)?;
        }
        Err(Error::Unauthorized)
    };
    if !signature.is_some_and(|s| verify(&secret, body, s)) {
        return reject("a delivery's Linear-Signature didn't match the signing secret");
    }
    let Ok(payload) = serde_json::from_slice::<Value>(body) else { return Err(Error::Invalid("a Linear delivery is JSON".into())) };
    if !fresh(&payload, code_sql::now_ms()) {
        return reject("a delivery's webhookTimestamp was over a minute away (a replay, or a clock off)");
    }
    let org = payload["organizationId"].as_str().unwrap_or_default();
    let conn = match conn {
        Some(c) if c.organization_id == org => c,
        Some(_) => return Ok(Received { recorded: 0, duplicates: 0, ignored: Some("another workspace's delivery".into()) }),
        None => match store::connection_by_org(db, org)? {
            Some(c) => c,
            None => return Ok(Received { recorded: 0, duplicates: 0, ignored: Some("no roll owner has this workspace connected".into()) }),
        },
    };
    let kind = payload["type"].as_str().unwrap_or_default().to_string();
    let action = payload["action"].as_str().unwrap_or_default().to_string();
    let log_id = crate::new_id("ldl");
    let delivery = delivery.chars().take(64).collect::<String>();
    let mut ack = None;
    let out = db.write(|c| -> code_sql::Result<(Received, bool)> {
        let mut r = Received { recorded: 0, duplicates: 0, ignored: None };
        let mut summary = String::new();
        if kind == "OAuthApp" && action == "revoked" {
            // The workspace removed roll's app: the connection is over.
            c.execute("DELETE FROM linear_connections WHERE owner_id = ?", [&conn.owner_id])?;
            return Ok((r, false));
        }
        let events = events_of(c, &conn, &payload)?;
        if events.is_empty() {
            r.ignored = Some(format!("{kind} {action}"));
        }
        let mut session_created = false;
        for e in &events {
            if summary.is_empty() {
                summary = e.summary.clone();
            }
            match record_event(c, &conn.owner_id, e)? {
                Some(_) => {
                    r.recorded += 1;
                    session_created |= e.kind == "agent_session.created";
                }
                None => r.duplicates += 1,
            }
        }
        let result = if r.recorded > 0 {
            "recorded"
        } else if r.duplicates > 0 {
            "duplicate"
        } else {
            "ignored"
        };
        let summary = if summary.is_empty() { format!("{kind} {action}") } else { summary };
        store::log_delivery(
            c,
            &conn.owner_id,
            &store::Delivery { id: log_id.clone(), at: code_sql::now_ms(), delivery: delivery.clone(), kind: kind.clone(), action: action.clone(), summary, result: result.into(), events: r.recorded as i64 },
        )?;
        // A session nothing will answer: say so in Linear, or it shows roll as unresponsive.
        let unanswered = session_created && !listens(c, &conn.owner_id, SOURCE, "agent_session.created")?;
        Ok((r, unanswered))
    })?;
    let (r, unanswered) = out;
    if r.recorded > 0 {
        code.wake_automations(&conn.owner_id);
    }
    if unanswered && let Some(session) = payload["agentSession"]["id"].as_str() {
        ack = Some(session.to_string());
    }
    if let Some(session) = ack {
        let code = code.clone();
        let owner = conn.owner_id.clone();
        tokio::spawn(async move {
            let body = "roll has no automation for Linear agent sessions yet. Turn on **Linear agent** in roll's automations, then mention or assign roll again.";
            if let Err(e) = super::steps::post_activity(&code, &owner, &session, json!({ "type": "error", "body": body }), Some(&super::client::uuid_for(&format!("unanswered:{session}"))), false).await {
                tracing::warn!(error = %e, "telling Linear no automation takes its agent session");
            }
        });
    }
    Ok(r)
}

/// The events a payload brings (none for kinds roll doesn't use), with the owner's context: the
/// repo for the issue's team, and the roll thread already working on the issue.
fn events_of(c: &Conn<'_>, conn: &Connection, p: &Value) -> code_sql::Result<Vec<NewEvent>> {
    let kind = p["type"].as_str().unwrap_or_default();
    let action = p["action"].as_str().unwrap_or_default();
    let d = &p["data"];
    let workspace = json!({ "id": conn.organization_id, "name": conn.organization_name, "url_key": conn.url_key });
    let actor = actor_of(&p["actor"]);
    let by_roll = conn.kind == Kind::OAuth && actor["id"].as_str() == Some(conn.user_id.as_str());
    let event = |k: &str, id: String, mut data: Value, summary: String| {
        data["workspace"] = workspace.clone();
        data["roll_user"] = Value::String(conn.user_id.clone());
        NewEvent { source: SOURCE.into(), kind: k.into(), id, data, summary, automation: None }
    };
    let thread_of = |issue_id: &str| -> code_sql::Result<Value> {
        Ok(store::issue_thread(c, &conn.owner_id, issue_id)?.map(|(m, t)| json!({ "machine": m, "thread": t })).unwrap_or(Value::Null))
    };
    Ok(match (kind, action) {
        ("Issue", "create" | "update" | "remove") => {
            let issue = issue_of(d);
            let id = d["id"].as_str().unwrap_or_default();
            let ident = d["identifier"].as_str().unwrap_or(id);
            let (k, key, verb) = match action {
                "create" => ("issue.created", format!("issue.created:{id}"), "created"),
                "update" => ("issue.updated", format!("issue.updated:{id}:{}", d["updatedAt"].as_str().unwrap_or_default()), "updated"),
                _ => ("issue.removed", format!("issue.removed:{id}"), "removed"),
            };
            let mut data = json!({
                "issue": issue,
                "actor": actor,
                "by_roll": by_roll,
                "repo": conn.settings.repo_for(d["team"]["key"].as_str(), d["teamId"].as_str()),
                "thread": thread_of(id)?,
                "url": p["url"],
            });
            if action == "update" {
                data["changes"] = changes_of(d, &p["updatedFrom"]);
            }
            vec![event(k, key, data, format!("{ident} {verb}"))]
        }
        ("Comment", "create" | "update") => {
            let id = d["id"].as_str().unwrap_or_default();
            let body = text(&d["body"]);
            let issue_id = d["issueId"].as_str().or(d["issue"]["id"].as_str()).unwrap_or_default();
            let user_id = d["userId"].as_str().or(d["user"]["id"].as_str());
            let by_roll = by_roll || (conn.kind == Kind::OAuth && user_id == Some(conn.user_id.as_str()));
            let issue = json!({
                "id": issue_id,
                "identifier": d["issue"]["identifier"],
                "title": d["issue"]["title"],
                "url": d["issue"]["url"],
                "team": { "id": d["issue"]["teamId"].as_str().or(d["issue"]["team"]["id"].as_str()), "key": d["issue"]["team"]["key"] },
            });
            let ident = d["issue"]["identifier"].as_str().unwrap_or(issue_id).to_string();
            let (k, key) = if action == "create" {
                ("comment.created", format!("comment.created:{id}"))
            } else {
                ("comment.updated", format!("comment.updated:{id}:{}", d["updatedAt"].as_str().or(d["editedAt"].as_str()).unwrap_or_default()))
            };
            let data = json!({
                "comment": { "id": id, "body": body, "url": p["url"], "parent_id": d["parentId"], "user": user_of(&d["user"], user_id) },
                "issue": issue,
                "mentions_roll": mentions(conn, &body),
                "in_session": store::in_session(c, &conn.owner_id, issue_id, id, d["parentId"].as_str())?,
                "by_roll": by_roll,
                "actor": actor,
                "repo": conn.settings.repo_for(d["issue"]["team"]["key"].as_str(), d["issue"]["teamId"].as_str()),
                "thread": thread_of(issue_id)?,
            });
            vec![event(k, key, data, format!("A comment on {ident}"))]
        }
        ("AgentSessionEvent", "created" | "prompted") => {
            let s = &p["agentSession"];
            let session_id = s["id"].as_str().unwrap_or_default();
            if session_id.is_empty() {
                return Ok(vec![]);
            }
            let issue = issue_of(&s["issue"]);
            let issue_id = s["issueId"].as_str().or(s["issue"]["id"].as_str()).unwrap_or_default();
            let ident = s["issue"]["identifier"].as_str().unwrap_or(issue_id).to_string();
            let comment_id = s["commentId"].as_str().or(s["comment"]["id"].as_str()).or(s["sourceCommentId"].as_str());
            let session = store::ensure_session(c, &conn.owner_id, session_id, issue_id, &ident, comment_id)?;
            let mut data = json!({
                "session": { "id": session_id, "url": s["url"], "status": s["status"] },
                "issue": issue,
                "comment": if s["comment"].is_object() { json!({ "id": s["comment"]["id"], "body": text(&s["comment"]["body"]) }) } else { Value::Null },
                "prompt_context": text_n(&p["promptContext"], 32 << 10),
                "guidance": p["guidance"],
                "user": user_of(&s["creator"], s["creatorId"].as_str()),
                "repo": conn.settings.repo_for(s["issue"]["team"]["key"].as_str(), s["issue"]["teamId"].as_str()),
                "thread": match (&session.machine_id, &session.thread) {
                    (Some(m), Some(t)) => json!({ "machine": m, "thread": t }),
                    _ => Value::Null,
                },
                "attached": false,
            });
            if action == "created" {
                data["prompt"] = Value::String(text(&s["comment"]["body"]));
                vec![event("agent_session.created", format!("agent_session.created:{session_id}"), data, format!("roll was asked on {ident}"))]
            } else {
                let a = &p["agentActivity"];
                let activity_id = a["id"].as_str().unwrap_or_default();
                if activity_id.is_empty() {
                    return Ok(vec![]);
                }
                let body = text(a["content"]["body"].as_str().map(|_| &a["content"]["body"]).unwrap_or(&a["body"]));
                let signal = a["signal"].as_str();
                let value = a["signalMetadata"]["value"].as_str().or(a["signalMetadata"]["option"]["value"].as_str());
                let user = a["user"]["name"].as_str().unwrap_or("someone");
                store::add_prompt(c, session_id, activity_id, &body, signal, value, user)?;
                // A run showing this session takes the prompt itself: wake it.
                let attached = match &session.bridge_run {
                    Some(run) => wake_run(c, run)?,
                    None => false,
                };
                data["prompt"] = Value::String(body);
                data["signal"] = signal.map(Value::from).unwrap_or(Value::Null);
                data["user"] = user_of(&a["user"], a["userId"].as_str());
                data["attached"] = Value::Bool(attached);
                vec![event("agent_session.prompted", format!("agent_session.prompted:{activity_id}"), data, format!("A reply to roll on {ident}"))]
            }
        }
        _ => vec![],
    })
}

fn text(v: &Value) -> String {
    text_n(v, TEXT)
}

fn text_n(v: &Value, n: usize) -> String {
    v.as_str().map(|s| s.chars().take(n).collect()).unwrap_or_default()
}

fn actor_of(a: &Value) -> Value {
    if !a.is_object() {
        return Value::Null;
    }
    json!({ "id": a["id"], "name": a["name"], "type": a["type"], "email": a["email"] })
}

fn user_of(u: &Value, id: Option<&str>) -> Value {
    if u.is_object() {
        json!({ "id": u["id"], "name": u["name"], "email": u["email"] })
    } else {
        id.map(|i| json!({ "id": i })).unwrap_or(Value::Null)
    }
}

/// Whether a comment mentions roll's user (by id, `@name`, or a profile link).
pub fn mentions(conn: &Connection, body: &str) -> bool {
    let b = body.to_lowercase();
    let name = conn.user_name.to_lowercase();
    b.contains(&conn.user_id.to_lowercase()) || (!name.is_empty() && (b.contains(&format!("@{name}")) || b.contains(&format!("/profiles/{name}"))))
}

/// An issue from a webhook's `data` (or an agent session's `issue`), in the shape automations
/// see as `event.data.issue`.
pub fn issue_of(d: &Value) -> Value {
    if !d.is_object() {
        return Value::Null;
    }
    let team_id = d["teamId"].as_str().or(d["team"]["id"].as_str());
    let labels: Vec<Value> = d["labels"]
        .as_array()
        .or(d["labels"]["nodes"].as_array())
        .map(|ls| ls.iter().map(|l| json!({ "id": l["id"], "name": l["name"] })).collect())
        .unwrap_or_default();
    json!({
        "id": d["id"],
        "identifier": d["identifier"],
        "number": d["number"],
        "title": d["title"],
        "description": text(&d["description"]),
        "url": d["url"],
        "priority": d["priority"].as_f64().map(|p| p as i64),
        "priority_label": d["priorityLabel"],
        "team": { "id": team_id, "key": d["team"]["key"], "name": d["team"]["name"] },
        "state": { "id": d["stateId"].as_str().or(d["state"]["id"].as_str()), "name": d["state"]["name"], "type": d["state"]["type"] },
        "labels": labels,
        "label_ids": d["labelIds"],
        "assignee": if d["assignee"].is_object() { json!({ "id": d["assignee"]["id"], "name": d["assignee"]["name"], "email": d["assignee"]["email"] }) } else { d["assigneeId"].as_str().map(|i| json!({ "id": i })).unwrap_or(Value::Null) },
        "delegate": if d["delegate"].is_object() { json!({ "id": d["delegate"]["id"], "name": d["delegate"]["name"] }) } else { d["delegateId"].as_str().map(|i| json!({ "id": i })).unwrap_or(Value::Null) },
        "project": if d["project"].is_object() { json!({ "id": d["project"]["id"], "name": d["project"]["name"] }) } else { d["projectId"].as_str().map(|i| json!({ "id": i })).unwrap_or(Value::Null) },
        "creator_id": d["creatorId"],
    })
}

/// What an update changed (`updatedFrom` holds the old values of what changed).
pub fn changes_of(d: &Value, from: &Value) -> Value {
    let Some(f) = from.as_object() else { return json!({ "fields": [] }) };
    let mut out = json!({ "fields": f.keys().collect::<Vec<_>>() });
    if f.contains_key("stateId") {
        out["state"] = json!({ "from": f["stateId"], "to": d["state"]["name"], "to_id": d["stateId"], "to_type": d["state"]["type"] });
    }
    if let Some(old) = f.get("labelIds").and_then(Value::as_array) {
        let now: Vec<&str> = d["labelIds"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let was: Vec<&str> = old.iter().filter_map(Value::as_str).collect();
        let name = |id: &str| {
            d["labels"].as_array().and_then(|ls| ls.iter().find(|l| l["id"].as_str() == Some(id))).and_then(|l| l["name"].as_str()).unwrap_or(id).to_string()
        };
        out["labels_added"] = now.iter().filter(|i| !was.contains(i)).map(|i| Value::String(name(i))).collect();
        out["labels_removed"] = was.iter().filter(|i| !now.contains(i)).map(|i| Value::String(i.to_string())).collect();
    }
    if f.contains_key("assigneeId") {
        out["assignee"] = json!({ "from": f["assigneeId"], "to": d["assigneeId"] });
    }
    if f.contains_key("delegateId") {
        out["delegate"] = json!({ "from": f["delegateId"], "to": d["delegateId"] });
    }
    if f.contains_key("priority") {
        out["priority"] = json!({ "from": f["priority"].as_f64().map(|p| p as i64), "to": d["priority"].as_f64().map(|p| p as i64) });
    }
    if f.contains_key("title") {
        out["title"] = json!({ "from": f["title"] });
    }
    if f.contains_key("projectId") {
        out["project"] = json!({ "from": f["projectId"], "to": d["projectId"] });
    }
    out
}

impl From<LinearError> for Error {
    fn from(e: LinearError) -> Error {
        match e {
            LinearError::NotConnected => Error::NotFound("Linear isn't connected".into()),
            LinearError::Reauth(_) | LinearError::Unauthorized => Error::Unprocessable(e.to_string()),
            LinearError::Server(m) => Error::Unprocessable(m),
            LinearError::RateLimited(_) | LinearError::Transient(_) => Error::Unavailable(e.to_string()),
            LinearError::Failed(m) => Error::Unprocessable(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        Connection {
            owner_id: "own".into(),
            organization_id: "org".into(),
            organization_name: "Acme".into(),
            url_key: "acme".into(),
            kind: Kind::OAuth,
            token: String::new(),
            refresh: None,
            expires_at: None,
            token_rev: 0,
            scopes: vec![],
            user_id: "app-user".into(),
            user_name: "roll".into(),
            hook_id: "lwh_1".into(),
            hook_secret: None,
            hook_hint: None,
            linear_webhook_id: None,
            settings: Default::default(),
            status: "ok".into(),
            error: None,
            last_delivery_at: None,
            rejected_at: None,
            rejected_why: None,
            created_at: 0,
            created_by: "ada".into(),
        }
    }

    #[test]
    fn signatures_are_hmac_sha256_of_the_body() {
        let body = br#"{"type":"Issue"}"#;
        let sig = sign("s3cret", body);
        assert_eq!(sig.len(), 64);
        assert!(verify("s3cret", body, &sig));
        assert!(verify("s3cret", body, &sig.to_uppercase()), "hex is hex, whatever its case");
        assert!(!verify("other", body, &sig));
        assert!(!verify("s3cret", br#"{"type":"Issue "}"#, &sig));
        assert!(!verify("s3cret", body, "not hex"));
        assert!(!verify("s3cret", body, ""));
    }

    #[test]
    fn deliveries_are_fresh_for_a_minute_either_way() {
        let now = 1_700_000_000_000;
        assert!(fresh(&json!({ "webhookTimestamp": now - 59_000 }), now));
        assert!(fresh(&json!({ "webhookTimestamp": (now + 30_000) as f64 }), now));
        assert!(!fresh(&json!({ "webhookTimestamp": now - 61_000 }), now));
        assert!(!fresh(&json!({ "webhookTimestamp": now + 61_000 }), now));
        assert!(!fresh(&json!({}), now), "no timestamp, no delivery");
        assert!(!fresh(&json!({ "webhookTimestamp": "soon" }), now));
    }

    #[test]
    fn updates_say_what_changed() {
        let d = json!({
            "stateId": "s2", "state": { "name": "In Progress", "type": "started" },
            "labelIds": ["l1", "l2"], "labels": [{ "id": "l1", "name": "Bug" }, { "id": "l2", "name": "roll" }],
            "assigneeId": "u2", "priority": 2.0,
        });
        let c = changes_of(&d, &json!({ "stateId": "s1", "labelIds": ["l1", "l3"], "assigneeId": null, "priority": 4.0 }));
        assert_eq!(c["state"]["to"], "In Progress");
        assert_eq!(c["state"]["to_type"], "started");
        assert_eq!(c["state"]["from"], "s1");
        assert_eq!(c["labels_added"], json!(["roll"]));
        assert_eq!(c["labels_removed"], json!(["l3"]));
        assert_eq!(c["assignee"], json!({ "from": null, "to": "u2" }));
        assert_eq!(c["priority"], json!({ "from": 4, "to": 2 }));
        assert_eq!(changes_of(&d, &Value::Null), json!({ "fields": [] }));
    }

    #[test]
    fn mentions_are_by_name_profile_or_id() {
        let c = conn();
        assert!(mentions(&c, "hey @roll can you look"));
        assert!(mentions(&c, "see https://linear.app/acme/profiles/roll"));
        assert!(mentions(&c, "ping app-user"));
        assert!(!mentions(&c, "rolling along"));
    }

    #[test]
    fn issues_are_normalized_from_webhooks_and_sessions() {
        let i = issue_of(&json!({
            "id": "i1", "identifier": "ENG-1", "title": "T", "priority": 1.0, "teamId": "t1", "team": { "key": "ENG" },
            "stateId": "s1", "state": { "name": "Todo", "type": "unstarted" }, "labels": [{ "id": "l1", "name": "Bug" }],
            "assignee": { "id": "u1", "name": "Ada" },
        }));
        assert_eq!(i["team"], json!({ "id": "t1", "key": "ENG", "name": null }));
        assert_eq!(i["priority"], 1);
        assert_eq!(i["labels"], json!([{ "id": "l1", "name": "Bug" }]));
        assert_eq!(i["assignee"]["name"], "Ada");
        // An agent session's issue has labels as a connection.
        let s = issue_of(&json!({ "id": "i2", "labels": { "nodes": [{ "id": "l2", "name": "roll" }] } }));
        assert_eq!(s["labels"], json!([{ "id": "l2", "name": "roll" }]));
        assert_eq!(issue_of(&Value::Null), Value::Null);
    }
}
