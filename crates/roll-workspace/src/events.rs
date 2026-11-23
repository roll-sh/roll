//! A project's events feed (docs/workspace.md, "Events"): who joined, left or changed role, which
//! groups hold a role, and invites, in order after a cursor. roll MCP serves it as
//! `workspace.event`, the way roll secrets' feed is `secrets.event`.
//!
//! An event is written in the change's own write ([`emit`]), so its position is taken there: the
//! project's head row (`project_event_heads`) incremented under its row lock (on Postgres a second
//! writer waits for it, or loses a serializable race and runs again; on SQLite there's one writer).
//! Rows therefore commit in `seq` order, and a follower past 7 has already been able to see 6. Never
//! a Postgres sequence: those commit out of order, and a follower would skip one forever. `data`
//! holds ids, handles, group names, roles and reasons, and always `actor`: never a token, a code, or
//! an email (an invite's address is shown only to whoever sees the project's invites).

use crate::{Error, Result, Scope, Workspace, owner_of};
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS project_event_heads (
    project_id TEXT PRIMARY KEY,
    seq INTEGER NOT NULL
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS project_events (
    project_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    subject TEXT,
    data TEXT NOT NULL,
    PRIMARY KEY (project_id, seq)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS project_events_subject ON project_events (project_id, subject, seq);
";

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Someone became a member: the project's maker, or an invite accepted.
    MemberAdded,
    /// Someone left, was removed, or their account was deleted (`reason`: `removed`, `left`,
    /// `offboarded`).
    MemberRemoved,
    MemberRoleChanged,
    /// A member's account can't sign in any more (an admin or SCIM disabled it): they keep their
    /// membership, which counts for nothing until it's enabled again.
    MemberDeactivated,
    /// A group was given a role on the project, or its role changed.
    GroupRoleSet,
    /// A group's role was taken away, or the group was deleted.
    GroupRoleRemoved,
    InviteSent,
    InviteAccepted,
    /// An invite stopped working before it was used (`reason`).
    InviteRevoked,
}

pub const KINDS: &[Kind] = &[
    Kind::MemberAdded,
    Kind::MemberRemoved,
    Kind::MemberRoleChanged,
    Kind::MemberDeactivated,
    Kind::GroupRoleSet,
    Kind::GroupRoleRemoved,
    Kind::InviteSent,
    Kind::InviteAccepted,
    Kind::InviteRevoked,
];

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::MemberAdded => "member.added",
            Kind::MemberRemoved => "member.removed",
            Kind::MemberRoleChanged => "member.role_changed",
            Kind::MemberDeactivated => "member.deactivated",
            Kind::GroupRoleSet => "group.role_set",
            Kind::GroupRoleRemoved => "group.role_removed",
            Kind::InviteSent => "invite.sent",
            Kind::InviteAccepted => "invite.accepted",
            Kind::InviteRevoked => "invite.revoked",
        }
    }
}

/// Record `kind` in `project`'s feed, in the caller's write: the next `seq`, taken from the
/// project's head under its row lock. `subject` is the person, group or invite it's about.
pub(crate) fn emit(c: &Conn<'_>, project: &str, at: i64, kind: Kind, subject: &str, actor: &str, data: Value) -> Result<i64> {
    let Value::Object(mut data) = data else { return Err(Error::Internal("an event's data is an object".into())) };
    c.claim(&format!("workspace-events:{project}"))?;
    let take = |c: &Conn<'_>| -> Result<Option<i64>> { Ok(c.query("UPDATE project_event_heads SET seq = seq + 1 WHERE project_id = ? RETURNING seq", [project], |r| r.get(0))?.pop()) };
    let seq = match take(c)? {
        Some(s) => s,
        None => {
            c.execute("INSERT INTO project_event_heads (project_id, seq) VALUES (?, 0) ON CONFLICT (project_id) DO NOTHING", [project])?;
            take(c)?.ok_or_else(|| Error::Internal("the events head vanished".into()))?
        }
    };
    data.insert("actor".into(), actor.into());
    c.execute(
        "INSERT INTO project_events (project_id, seq, at, kind, subject, data) VALUES (?, ?, ?, ?, ?, ?)",
        params![project, seq, at, kind.as_str(), subject, Value::Object(data).to_string()],
    )?;
    Ok(seq)
}

/// A project's events go with it for good (it isn't kept any more).
pub(crate) fn forget(c: &Conn<'_>, project: &str) -> Result<()> {
    c.execute("DELETE FROM project_events WHERE project_id = ?", [project])?;
    c.execute("DELETE FROM project_event_heads WHERE project_id = ?", [project])?;
    Ok(())
}

/// The kind of the latest event about `subject` in `project`.
pub(crate) fn last_about(c: &Conn<'_>, project: &str, subject: &str) -> Result<Option<String>> {
    Ok(c.query_row("SELECT kind FROM project_events WHERE project_id = ? AND subject = ? ORDER BY seq DESC LIMIT 1", [project, subject], |r| r.get(0)).optional()?)
}

/// `GET …/events?after=&limit=&kinds=&latest=`.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    pub after: Option<i64>,
    pub limit: Option<i64>,
    /// Comma-separated kinds, or a family as `member.*`.
    pub kinds: Option<String>,
    /// `1`: no events, the cursor at the head (a reader that starts from now). Never with `after`.
    pub latest: Option<u8>,
}

#[derive(Debug, Serialize)]
pub struct FeedEvent {
    pub seq: i64,
    pub at: i64,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Serialize)]
pub struct EventPage {
    pub events: Vec<FeedEvent>,
    /// The cursor for the next call (`after`): past every row read, those a filter left out too.
    pub next: i64,
}

pub const LIMIT: i64 = 100;
pub const LIMIT_MAX: i64 = 500;

/// `kinds=` as kinds: each exact, or a family (`member.*`); 400 for one that isn't.
fn parse_kinds(kinds: Option<&str>) -> Result<Vec<Kind>> {
    let Some(kinds) = kinds.map(str::trim).filter(|k| !k.is_empty()) else { return Ok(KINDS.to_vec()) };
    let mut out = Vec::new();
    for k in kinds.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        let matched: Vec<Kind> = match k.strip_suffix(".*") {
            Some(family) => KINDS.iter().copied().filter(|x| x.as_str().split_once('.').is_some_and(|(f, _)| f == family)).collect(),
            None => KINDS.iter().copied().filter(|x| x.as_str() == k).collect(),
        };
        if matched.is_empty() {
            return Err(Error::BadRequest(format!("no event kind {k}: {}", KINDS.iter().map(|x| x.as_str()).collect::<Vec<_>>().join(", "))));
        }
        for m in matched {
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    Ok(out)
}

impl Workspace {
    /// `project`'s events after a cursor, in commit order, for whoever `scope` lets see it (a
    /// project they don't reach is a 404, as its members are). An invite's address only when
    /// `emails` (the caller manages the project's members).
    pub fn events(&self, scope: Scope<'_>, project: &str, q: EventsQuery, emails: bool) -> Result<EventPage> {
        let latest = match (q.latest, q.after) {
            (None, _) => false,
            (Some(1), None) => true,
            (Some(1), Some(_)) => return Err(Error::BadRequest("latest=1 starts from now: leave out after".into())),
            (Some(_), _) => return Err(Error::BadRequest("latest is 1 or left out".into())),
        };
        let kinds = parse_kinds(q.kinds.as_deref())?;
        let limit = q.limit.unwrap_or(LIMIT).clamp(1, LIMIT_MAX);
        let after = q.after.unwrap_or(0).max(0);
        let sql = format!(
            "SELECT seq, at, kind, data FROM project_events WHERE project_id = ? AND seq > ? AND seq <= ? AND kind IN ({}) ORDER BY seq LIMIT ?",
            vec!["?"; kinds.len()].join(", ")
        );
        let (head, rows) = self.db.read(|c| {
            owner_of(c, scope, project)?;
            // The head first: every row at or below it committed with it, so a page that stops
            // short of `limit` may move the cursor to the head, past rows the filter left out.
            let head: i64 = c.query_row("SELECT seq FROM project_event_heads WHERE project_id = ?", [project], |r| r.get(0)).optional()?.unwrap_or(0);
            if latest {
                return Ok::<_, Error>((head, vec![]));
            }
            let mut args = vec![code_sql::Value::Text(project.into()), code_sql::Value::Int(after), code_sql::Value::Int(head)];
            args.extend(kinds.iter().map(|k| code_sql::Value::Text(k.as_str().into())));
            args.push(code_sql::Value::Int(limit));
            let rows: Vec<(i64, i64, String, String)> = c.query(&sql, &args, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            Ok((head, rows))
        })?;
        if latest {
            return Ok(EventPage { events: vec![], next: head });
        }
        let next = match rows.last() {
            Some((seq, ..)) if rows.len() as i64 == limit => *seq,
            _ => head.max(after),
        };
        let events = rows
            .into_iter()
            .map(|(seq, at, kind, data)| {
                let mut data: Value = serde_json::from_str(&data).unwrap_or_else(|_| Value::Object(Map::new()));
                if !emails && let Some(o) = data.as_object_mut() {
                    o.remove("email");
                }
                FeedEvent { seq, at, kind, data }
            })
            .collect();
        Ok(EventPage { events, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse_exactly_or_by_family() {
        assert_eq!(parse_kinds(None).unwrap().len(), KINDS.len());
        assert_eq!(parse_kinds(Some("invite.*, member.role_changed")).unwrap(), vec![Kind::InviteSent, Kind::InviteAccepted, Kind::InviteRevoked, Kind::MemberRoleChanged]);
        assert!(parse_kinds(Some("member")).is_err());
        assert!(parse_kinds(Some("nope.*")).is_err());
    }
}
