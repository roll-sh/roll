//! Being told (docs/secrets.md, "Events", "Being told"): a person's own subscription to a project's
//! events feed, delivered to a Slack or Discord channel through one of roll code's chat installs
//! they may use, or to their own account's email.
//!
//! A subscription follows the feed as a follower does: a stored cursor, always the `next` the feed
//! answered (or an event's `seq` once that event is delivered), read with [`Secrets::events`] as the
//! person is now, so every event is asked of their rights at that moment, exactly as the feed asks
//! for a reader. Someone who can no longer read the project (removed, left, can't sign in) gets
//! nothing, and their subscription is disabled. One worker per node ([`Secrets::sweep_notifications`])
//! claims a subscription in a write, and each delivery in another, before it sends: the send is
//! outside every write. A failed send is retried a bounded number of times, then skipped, and the
//! subscription shows the error. roll-secrets has no HTTP client, mailer or chat client: rolld's
//! [`Notifier`] sends. A message holds kinds and names (environment, folder, keys, actor), never a
//! value or anything made from one.

use crate::access::{Action, Caller, Scope, is_person};
use crate::events::{EventsQuery, FeedEvent};
use crate::{Error, Result, Secrets, audit, names};
use code_sql::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS notifications (
    id TEXT PRIMARY KEY,
    project_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    kinds TEXT NOT NULL,
    environment TEXT,
    channel TEXT NOT NULL,
    install TEXT,
    target TEXT,
    cursor INTEGER NOT NULL,
    state TEXT NOT NULL,
    error TEXT,
    error_at INTEGER,
    tries INTEGER NOT NULL,
    running_until INTEGER NOT NULL,
    retry_at INTEGER NOT NULL,
    delivered INTEGER NOT NULL,
    delivered_at INTEGER,
    created_at INTEGER NOT NULL
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS notifications_project ON notifications (project_id, user_id);
";

/// How a claim holds; a node that dies holding one leaves it due when it lapses.
const HOLD_MS: i64 = 2 * 60_000;
/// Tries per event before it's skipped and the subscription shows the error; the waits between.
const RETRY_MS: [i64; 2] = [60_000, 5 * 60_000];
/// Events a subscription is sent per pass: a burst arrives over a few passes, not at once.
const PER_PASS: i64 = 20;
/// Subscriptions per person per project.
const MAX_PER_PERSON: i64 = 20;
/// The longest message sent.
const MESSAGE_MAX: usize = 1_500;

/// Where a person's notifications go: rolld's, over roll code's chat installs and roll accounts'
/// mail. Blocking calls, from the worker's blocking thread.
pub trait Notifier: Send + Sync {
    /// Whether `user` may post through chat install `install` on `platform` (`slack`, `discord`) now:
    /// an install of their own roll code owner, as chat ops lets only the people the install names
    /// drive roll. `Err` is "can't tell now" (retried).
    fn may_use(&self, user: &str, platform: &str, install: &str) -> Result<bool>;
    /// Whether mail goes out at all.
    fn mail_on(&self) -> bool;
    /// Post `text` (plain, no markup or mentions) to `channel` through `install`.
    fn chat(&self, install: &str, channel: &str, text: &str) -> Result<()>;
    /// Mail `user`'s own account address: never one a caller names.
    fn mail(&self, user: &str, subject: &str, text: &str) -> Result<()>;
}

/// Where a subscription delivers: `{slack: {install, channel}}`, `{discord: {install, channel}}`
/// or `{email: "own"}` (the person's own account address, never another).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Channel {
    Slack { install: String, channel: String },
    Discord { install: String, channel: String },
    Email(Own),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Own {
    Own,
}

impl Channel {
    fn platform(&self) -> &'static str {
        match self {
            Channel::Slack { .. } => "slack",
            Channel::Discord { .. } => "discord",
            Channel::Email(_) => "email",
        }
    }

    fn chat(&self) -> Option<(&'static str, &str, &str)> {
        match self {
            Channel::Slack { install, channel } => Some(("slack", install, channel)),
            Channel::Discord { install, channel } => Some(("discord", install, channel)),
            Channel::Email(_) => None,
        }
    }

    fn of(platform: &str, install: Option<String>, target: Option<String>) -> Channel {
        let (install, channel) = (install.unwrap_or_default(), target.unwrap_or_default());
        match platform {
            "slack" => Channel::Slack { install, channel },
            "discord" => Channel::Discord { install, channel },
            _ => Channel::Email(Own::Own),
        }
    }
}

/// `POST …/notifications`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewNotification {
    /// Kinds, or families as `rotation.*`; none (or empty) is every kind.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Only this environment's events (the person must read it).
    #[serde(default)]
    pub environment: Option<String>,
    pub channel: Channel,
}

#[derive(Debug, Serialize)]
pub struct Notification {
    pub id: String,
    pub project: String,
    /// Whose it is (`usr_…`).
    pub user: String,
    /// As asked; empty is every kind.
    pub kinds: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    pub channel: Channel,
    /// `active`, or `disabled` (`error` says why: they can no longer read the project, or use the
    /// install).
    pub state: String,
    /// The last delivery given up on, or why it was disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_at: Option<i64>,
    /// Events delivered so far, and when the last went.
    pub delivered: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<i64>,
    pub created_at: i64,
}

const COLUMNS: &str = "id, project_id, user_id, kinds, environment, channel, install, target, state, error, error_at, delivered, delivered_at, created_at";

fn row(r: &code_sql::Row<'_>) -> code_sql::Result<Notification> {
    let kinds: String = r.get(3)?;
    let channel: String = r.get(5)?;
    Ok(Notification {
        id: r.get(0)?,
        project: r.get(1)?,
        user: r.get(2)?,
        kinds: serde_json::from_str(&kinds).unwrap_or_default(),
        environment: r.get(4)?,
        channel: Channel::of(&channel, r.get(6)?, r.get(7)?),
        state: r.get(8)?,
        error: r.get(9)?,
        error_at: r.get(10)?,
        delivered: r.get(11)?,
        delivered_at: r.get(12)?,
        created_at: r.get(13)?,
    })
}

/// A chat channel's id as the platforms give them (Slack `C0123ABCD`, a Discord snowflake).
fn channel_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 40 || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(Error::BadRequest("channel is the platform's channel id (letters and digits, e.g. C0123ABCD)".into()));
    }
    Ok(())
}

impl Secrets {
    /// rolld's sender. Set once, at startup; without it nothing is delivered and none can be made.
    pub fn set_notifier(&self, n: Arc<dyn Notifier>) {
        let _ = self.notifier.set(n);
    }

    fn notifier(&self) -> Result<&Arc<dyn Notifier>> {
        self.notifier.get().ok_or_else(|| Error::BadRequest("notifications aren't on this server".into()))
    }

    /// The person a caller is (their session, their own token, an admin's session), or 400: a
    /// subscription is a person's own.
    fn subscriber(caller: &Caller) -> Result<String> {
        let user = caller.made_by();
        if !is_person(user) {
            return Err(Error::BadRequest("notifications are a person's own: sign in, or use a token of your own".into()));
        }
        Ok(user.to_string())
    }

    /// `GET …/notifications`: the caller's own, or every one in the project for its admins.
    pub fn notifications(&self, project: &str, caller: &Caller) -> Result<Vec<Notification>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        if caller.allows(Action::Admin, &Scope::whole(project)) {
            return self.db.read(|c| Ok(c.query(&format!("SELECT {COLUMNS} FROM notifications WHERE project_id = ? ORDER BY created_at"), [project], row)?));
        }
        let user = Self::subscriber(caller)?;
        self.db.read(|c| Ok(c.query(&format!("SELECT {COLUMNS} FROM notifications WHERE project_id = ? AND user_id = ? ORDER BY created_at"), [project, user.as_str()], row)?))
    }

    /// `POST …/notifications`: the caller's own, from now on (its cursor at the feed's head).
    pub fn create_notification(&self, project: &str, n: NewNotification, caller: &Caller) -> Result<Notification> {
        let user = Self::subscriber(caller)?;
        caller.need(Action::Read, &Scope::project(project))?;
        if let Some(e) = &n.environment {
            names::slug(e)?;
        }
        let kinds: Vec<String> = n.kinds.iter().map(|k| k.trim().to_string()).filter(|k| !k.is_empty()).collect();
        let joined = (!kinds.is_empty()).then(|| kinds.join(","));
        // The feed checks the kinds, the environment and the caller, and gives the head: the cursor.
        let head = self.events(project, EventsQuery { kinds: joined, environment: n.environment.clone(), latest: Some(1), ..Default::default() }, caller)?.next;
        let notifier = self.notifier()?;
        match n.channel.chat() {
            Some((platform, install, channel)) => {
                channel_id(channel)?;
                if install.is_empty() || install.len() > 64 {
                    return Err(Error::BadRequest("install is the chat install's id".into()));
                }
                if !notifier.may_use(&user, platform, install)? {
                    return Err(Error::Forbidden(format!("{install} isn't a {platform} install of yours: connect one under your integrations")));
                }
            }
            None if !notifier.mail_on() => return Err(Error::BadRequest("email isn't set up on this server".into())),
            None => {}
        }
        let now = self.db.now_ms()?;
        let id = names::new_id("ntf", 12);
        let (install, target) = match n.channel.chat() {
            Some((_, i, c)) => (Some(i.to_string()), Some(c.to_string())),
            None => (None, None),
        };
        let platform = n.channel.platform();
        let what = format!("{} to {platform}{}", if kinds.is_empty() { "every event".to_string() } else { kinds.join(", ") }, n.environment.as_deref().map(|e| format!(" ({e})")).unwrap_or_default());
        self.db.write(|c| {
            let mine: i64 = c.query_row("SELECT COUNT(*) FROM notifications WHERE project_id = ? AND user_id = ?", [project, user.as_str()], |r| r.get(0))?;
            if mine >= MAX_PER_PERSON {
                return Err(Error::Conflict("conflict", format!("a person has at most {MAX_PER_PERSON} notifications in a project")));
            }
            c.execute(
                "INSERT INTO notifications (id, project_id, user_id, kinds, environment, channel, install, target, cursor, state, error, error_at, tries, running_until, retry_at, delivered, delivered_at, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', NULL, NULL, 0, 0, 0, 0, NULL, ?)",
                params![id, project, user, serde_json::to_string(&kinds).expect("JSON"), n.environment, platform, install, target, head, now],
            )?;
            audit::record(c, self.keys()?, project, caller, "notification.create", n.environment.as_deref(), None, &[], &format!("{user} is told of {what}"), now)?;
            Ok(c.query_row(&format!("SELECT {COLUMNS} FROM notifications WHERE id = ?"), [&id], row)?)
        })
    }

    /// `DELETE …/notifications/{id}`: the caller's own, or any in the project for its admins.
    pub fn delete_notification(&self, project: &str, id: &str, caller: &Caller) -> Result<()> {
        caller.need(Action::Read, &Scope::project(project))?;
        let admin = caller.allows(Action::Admin, &Scope::whole(project));
        let me = caller.made_by().to_string();
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            let owner: Option<String> = c.query_row("SELECT user_id FROM notifications WHERE id = ? AND project_id = ?", [id, project], |r| r.get(0)).optional()?;
            match owner {
                Some(u) if admin || u == me => {
                    c.execute("DELETE FROM notifications WHERE id = ?", [id])?;
                    audit::record(c, self.keys()?, project, caller, "notification.delete", None, None, &[], &format!("{u} is no longer told ({id})"), now)
                }
                _ => Err(Error::NotFound(format!("no notification {id}"))),
            }
        })
    }

    /// A member's notifications in `project`, disabled once they no longer read it (members.rs).
    pub(crate) fn disable_notifications_of(&self, project: &str, user: &str, why: &str) -> Result<()> {
        let now = self.db.now_ms()?;
        self.db.write(|c| c.execute("UPDATE notifications SET state = 'disabled', error = ?, error_at = ? WHERE project_id = ? AND user_id = ? AND state = 'active'", params![why, now, project, user]))?;
        Ok(())
    }

    /// One pass of the worker: each due subscription's next events, as its person may read them now,
    /// delivered. Returns (delivered, failed).
    pub fn sweep_notifications(&self) -> Result<(usize, usize)> {
        let Some(notifier) = self.notifier.get().cloned() else { return Ok((0, 0)) };
        let now = self.db.now_ms()?;
        type Due = (String, String, String, String, Option<String>, String, Option<String>, Option<String>, i64, i64);
        let due: Vec<Due> = self.db.read(|c| {
            c.query(
                "SELECT n.id, n.project_id, n.user_id, n.kinds, n.environment, n.channel, n.install, n.target, n.cursor, n.tries FROM notifications n
                 WHERE n.state = 'active' AND n.running_until <= ? AND n.retry_at <= ?
                 AND EXISTS (SELECT 1 FROM audit_heads h WHERE h.project_id = n.project_id AND h.seq > n.cursor) LIMIT 200",
                params![now, now],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?)),
            )
        })?;
        let (mut sent, mut failed) = (0, 0);
        for (id, project, user, kinds, environment, platform, install, target, cursor, tries) in due {
            let mut hold = now + HOLD_MS;
            if self.db.write(|c| c.execute("UPDATE notifications SET running_until = ? WHERE id = ? AND running_until <= ? AND cursor = ? AND state = 'active'", params![hold, id, now, cursor]))? != 1 {
                continue;
            }
            let release = |hold: i64| -> Result<usize> { Ok(self.db.write(|c| c.execute("UPDATE notifications SET running_until = 0 WHERE id = ? AND running_until = ?", params![id, hold]))?) };
            let disable = |hold: i64, why: String| -> Result<usize> {
                let at = self.db.now_ms()?;
                Ok(self.db.write(|c| c.execute("UPDATE notifications SET state = 'disabled', error = ?, error_at = ?, running_until = 0 WHERE id = ? AND running_until = ?", params![why, at, id, hold]))?)
            };
            // Who they are now: an account that can no longer sign in gets nothing.
            let person = match self.person(&user) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    disable(hold, format!("{user} can no longer sign in"))?;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(notification = %id, "looking up a subscriber failed: {e}");
                    release(hold)?;
                    failed += 1;
                    continue;
                }
            };
            let caller = self.person_caller(&user, person.admin)?;
            let kinds: Vec<String> = serde_json::from_str(&kinds).unwrap_or_default();
            let q = EventsQuery { after: Some(cursor), limit: Some(PER_PASS), kinds: (!kinds.is_empty()).then(|| kinds.join(",")), environment: environment.clone(), latest: None };
            // As the feed answers them: every event asked of their rights now.
            let page = match self.events(&project, q, &caller) {
                Ok(p) => p,
                Err(Error::Forbidden(_) | Error::NotFound(_)) => {
                    disable(hold, format!("{user} may no longer read {}", environment.as_deref().unwrap_or("the project")))?;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(notification = %id, "reading a subscription's events failed: {e}");
                    release(hold)?;
                    failed += 1;
                    continue;
                }
            };
            let channel = Channel::of(&platform, install, target);
            if let Some((platform, install, _)) = channel.chat()
                && !page.events.is_empty()
            {
                match notifier.may_use(&user, platform, install) {
                    Ok(true) => {}
                    Ok(false) => {
                        disable(hold, format!("{user} may no longer use {platform} install {install}"))?;
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(notification = %id, "checking a chat install failed: {e}");
                        release(hold)?;
                        failed += 1;
                        continue;
                    }
                }
            }
            let name = self.workspace.get(roll_workspace::Scope::All, &project).map(|p| p.name).unwrap_or_else(|_| project.clone());
            let (mut at, mut stopped) = (cursor, false);
            for e in &page.events {
                // The delivery's claim: still ours, the cursor where we left it.
                let next_hold = self.db.now_ms()? + HOLD_MS;
                if self.db.write(|c| c.execute("UPDATE notifications SET running_until = ? WHERE id = ? AND running_until = ? AND cursor = ? AND state = 'active'", params![next_hold, id, hold, at]))? != 1 {
                    stopped = true;
                    break;
                }
                hold = next_hold;
                let m = message(&name, &project, e, self.web_url.as_deref());
                let res = match channel.chat() {
                    Some((_, install, target)) => notifier.chat(install, target, &m.text),
                    None => notifier.mail(&user, &m.subject, &m.text),
                };
                let t = self.db.now_ms()?;
                match res {
                    Ok(()) => {
                        self.db.write(|c| c.execute("UPDATE notifications SET cursor = ?, tries = 0, delivered = delivered + 1, delivered_at = ? WHERE id = ? AND running_until = ?", params![e.seq, t, id, hold]))?;
                        at = e.seq;
                        sent += 1;
                    }
                    Err(err) => {
                        failed += 1;
                        let why = format!("sending {} (event {}) failed: {err}", e.kind, e.seq);
                        tracing::info!(notification = %id, "{why}");
                        let tries = tries + 1;
                        if tries as usize > RETRY_MS.len() {
                            // Given up: past this event, and the subscription shows why.
                            self.db.write(|c| c.execute("UPDATE notifications SET cursor = ?, tries = 0, error = ?, error_at = ?, running_until = 0 WHERE id = ? AND running_until = ?", params![e.seq, why, t, id, hold]))?;
                        } else {
                            let wait = RETRY_MS[tries as usize - 1];
                            self.db.write(|c| c.execute("UPDATE notifications SET tries = ?, error = ?, error_at = ?, retry_at = ?, running_until = 0 WHERE id = ? AND running_until = ?", params![tries, why, t, t + wait, id, hold]))?;
                        }
                        stopped = true;
                        break;
                    }
                }
            }
            if !stopped {
                // Everything read is delivered: the cursor is the feed's `next`, exactly.
                self.db.write(|c| c.execute("UPDATE notifications SET cursor = ?, running_until = 0 WHERE id = ? AND running_until = ?", params![page.next.max(at), id, hold]))?;
            }
        }
        Ok((sent, failed))
    }
}

/// What a person is told of one event: names only, never a value or a hash of one.
pub(crate) struct Message {
    pub subject: String,
    pub text: String,
}

/// The message for `e` in project `name` (`project` its id): the kind, the environment, the folder
/// and keys, the actor, how it came about, and a link to the Events tab.
pub(crate) fn message(name: &str, project: &str, e: &FeedEvent, web: Option<&str>) -> Message {
    let d = &e.data;
    let s = |k: &str| d.get(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    let list = |v: Option<&Value>| v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()).unwrap_or_default();
    let place = match &e.environment {
        Some(env) => format!("{name}/{env}"),
        None => name.to_string(),
    };
    let mut lines = vec![format!("roll secrets: {} in {place}", e.kind)];
    // Keys, by folder when the change spans folders.
    match d.get("folders").and_then(Value::as_object) {
        Some(folders) => {
            for (folder, keys) in folders {
                lines.push(format!("{folder}: {}", list(Some(keys)).join(", ")));
            }
        }
        None => {
            let mut keys: Vec<String> = e.key.iter().cloned().collect();
            keys.extend(list(d.get("keys")).into_iter().filter(|k| Some(k) != e.key.as_ref()));
            let deleted = list(d.get("deleted"));
            let folder = s("path").filter(|p| *p != "/");
            if !keys.is_empty() || folder.is_some() {
                lines.push(format!("{}{}", folder.map(|f| format!("{f}: ")).unwrap_or_default(), keys.join(", ")));
            }
            if !deleted.is_empty() {
                lines.push(format!("deleted: {}", deleted.join(", ")));
            }
        }
    }
    let mut how = vec![];
    if let Some(a) = s("actor") {
        how.push(format!("by {a}"));
    }
    for k in ["via", "reason", "state", "repo", "sync"] {
        if let Some(v) = s(k) {
            how.push(format!("{k} {v}"));
        }
    }
    if !how.is_empty() {
        lines.push(how.join(" · "));
    }
    let link = format!("/dashboard/projects/{project}/secrets?view=events");
    lines.push(match web {
        Some(w) => format!("{}{link}", w.trim_end_matches('/')),
        None => link,
    });
    let mut text = lines.join("\n");
    if text.len() > MESSAGE_MAX {
        let mut cut = MESSAGE_MAX;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push('…');
    }
    Message { subject: format!("roll secrets: {} in {place}", e.kind), text }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_message_names_keys_folders_and_the_actor_and_links_the_tab() {
        let e = FeedEvent { seq: 7, at: 0, kind: "secret.changed".into(), environment: Some("production".into()), key: None, data: json!({ "actor": "usr_ada", "via": "write", "folders": { "/api": ["DB_URL"], "/db": ["PASS"] } }) };
        let m = message("shop", "prj_1", &e, Some("https://roll.example/"));
        assert_eq!(m.text, "roll secrets: secret.changed in shop/production\n/api: DB_URL\n/db: PASS\nby usr_ada · via write\nhttps://roll.example/dashboard/projects/prj_1/secrets?view=events");
    }

    #[test]
    fn channels_read_as_the_contract_says() {
        let c: Channel = serde_json::from_value(json!({ "slack": { "install": "ci_1", "channel": "C01" } })).unwrap();
        assert_eq!(c.chat(), Some(("slack", "ci_1", "C01")));
        assert_eq!(serde_json::from_value::<Channel>(json!({ "email": "own" })).unwrap(), Channel::Email(Own::Own));
        assert!(serde_json::from_value::<Channel>(json!({ "email": "someone@example.com" })).is_err());
    }
}
