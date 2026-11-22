//! A project's events feed (docs/secrets.md, "Events"): what an agent or a script follows instead
//! of polling health or the audit log (a leaked value, a failing rotation, a request waiting).
//! An event is an audit row that carries one (`audit.event`, `audit.event_data`), written in the
//! row's own INSERT, so its position is the row's `seq`: the project's chain head, taken in the
//! same write under the head's row lock (audit.rs, `append`). Two writers queue on that lock, so
//! rows commit in `seq` order and a reader past 7 has seen 6. Never a Postgres sequence: those
//! commit out of order. `data` holds names, ids, counts and reasons: never a value, or anything
//! made from one.

use crate::access::{Action, Caller, Scope};
use crate::{Error, Result, Secrets, from_json};
use code_sql::{Conn, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

pub(crate) const SCHEMA: &str = "
CREATE INDEX IF NOT EXISTS audit_events ON audit (project_id, seq) WHERE event IS NOT NULL;
CREATE TABLE IF NOT EXISTS event_marks (
    project_id TEXT NOT NULL,
    mark TEXT NOT NULL,
    at INTEGER NOT NULL,
    PRIMARY KEY (project_id, mark)
) WITHOUT ROWID;
";

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A stored value turned up in a roll code repository (a change or a scan), or a placeholder
    /// in pushed code (GitHub's secret scanning).
    SecretLeaked,
    /// `roll secrets scan`, or roll MCP's credential guard, matched stored values.
    ScanFinding,
    /// A managed key's scheduled rotation failed, once per failing run.
    RotationFailed,
    /// A change request or a secret request opened.
    RequestPending,
    /// A change request approved, rejected or closed; a secret request answered or cancelled.
    RequestDecided,
    SyncFailed,
    DecoyUsed,
    /// Whoever chose a reference, set up placeholders or made a sync can't reach it any more.
    MakerUnreachable,
    PlaceholderEchoed,
    PlaceholderWrongConsumer,
    PlaceholderWrongHost,
    ProductionUnprotected,
    /// A standard environment deleted (kept for the recovery window), or recovered within it.
    EnvDeleted,
    EnvRecovered,
    ProjectDeleted,
    ProjectRecovered,
}

pub const KINDS: &[Kind] = &[
    Kind::SecretLeaked,
    Kind::ScanFinding,
    Kind::RotationFailed,
    Kind::RequestPending,
    Kind::RequestDecided,
    Kind::SyncFailed,
    Kind::DecoyUsed,
    Kind::MakerUnreachable,
    Kind::PlaceholderEchoed,
    Kind::PlaceholderWrongConsumer,
    Kind::PlaceholderWrongHost,
    Kind::ProductionUnprotected,
    Kind::EnvDeleted,
    Kind::EnvRecovered,
    Kind::ProjectDeleted,
    Kind::ProjectRecovered,
];

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::SecretLeaked => "secret.leaked",
            Kind::ScanFinding => "scan.finding",
            Kind::RotationFailed => "rotation.failed",
            Kind::RequestPending => "request.pending",
            Kind::RequestDecided => "request.decided",
            Kind::SyncFailed => "sync.failed",
            Kind::DecoyUsed => "decoy.used",
            Kind::MakerUnreachable => "maker.unreachable",
            Kind::PlaceholderEchoed => "placeholder.echoed",
            Kind::PlaceholderWrongConsumer => "placeholder.wrong_consumer",
            Kind::PlaceholderWrongHost => "placeholder.wrong_host",
            Kind::ProductionUnprotected => "production.unprotected",
            Kind::EnvDeleted => "env.deleted",
            Kind::EnvRecovered => "env.recovered",
            Kind::ProjectDeleted => "project.deleted",
            Kind::ProjectRecovered => "project.recovered",
        }
    }
}

/// An audit row's event: its kind and what it adds to the row (which already names the
/// environment, folder, keys and actor).
#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub kind: Kind,
    pub data: Value,
}

impl Event {
    pub fn new(kind: Kind, data: Value) -> Event {
        Event { kind, data }
    }
}

/// How long a [`fresh`] mark holds: the same leak or unreachable maker is an event once a day.
const MARK_MS: i64 = 86_400_000;

/// Whether `mark` is new for `project` within a day, taken in the caller's write (two nodes never
/// both see it new): for events that would otherwise repeat on every check or use.
pub(crate) fn fresh(c: &Conn<'_>, project: &str, mark: &str, now: i64) -> Result<bool> {
    c.execute("DELETE FROM event_marks WHERE project_id = ? AND at < ?", params![project, now - MARK_MS])?;
    Ok(c.execute("INSERT INTO event_marks (project_id, mark, at) VALUES (?, ?, ?) ON CONFLICT (project_id, mark) DO NOTHING", params![project, mark, now])? == 1)
}

/// `GET …/events?after=&limit=&kinds=&environment=&latest=`.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    pub after: Option<i64>,
    pub limit: Option<i64>,
    /// Comma-separated kinds, or a family as `request.*`.
    pub kinds: Option<String>,
    pub environment: Option<String>,
    /// `1`: no events, the cursor at the head (a reader that starts from now). Never with `after`.
    pub latest: Option<u8>,
}

#[derive(Debug, Serialize)]
pub struct FeedEvent {
    pub seq: i64,
    pub at: i64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// The key, when the event is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub data: Value,
}

#[derive(Debug, Serialize)]
pub struct EventPage {
    pub events: Vec<FeedEvent>,
    /// The cursor for the next call (`after`).
    pub next: i64,
}

pub const LIMIT: i64 = 100;
pub const LIMIT_MAX: i64 = 500;
/// Rows read at once, and pages read at most per call: a filter that leaves most rows out still
/// answers promptly, its `next` past what it read.
const PAGE: i64 = 500;
const PAGES: usize = 20;

/// `kinds=` as kinds: each exact, or a family (`request.*`); 400 for one that isn't.
fn parse_kinds(kinds: Option<&str>) -> Result<Vec<Kind>> {
    let Some(kinds) = kinds.map(str::trim).filter(|k| !k.is_empty()) else { return Ok(KINDS.to_vec()) };
    let asked: Vec<&str> = kinds.split(',').map(str::trim).filter(|k| !k.is_empty()).collect();
    if asked.len() > 30 {
        return Err(Error::BadRequest("at most 30 kinds".into()));
    }
    let mut out = Vec::new();
    for k in asked {
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

impl Secrets {
    /// A project's events after a cursor, in commit order, as the caller may see them: only the
    /// environments it may read (asked of its grant row by row, as the audit log is), decoys' only
    /// for those who see decoys. `kinds` and `environment` filter here, not in the reader. A
    /// deleted project's feed stays readable through its recovery window, so a follower sees
    /// `project.deleted` (and `project.recovered`).
    pub fn events(&self, project: &str, q: EventsQuery, caller: &Caller) -> Result<EventPage> {
        caller.need(Action::Read, &Scope::project(project))?;
        if let Some(e) = &q.environment {
            caller.need(Action::Read, &Scope::env(project, e))?;
        }
        let latest = match (q.latest, q.after) {
            (None, _) => false,
            (Some(1), None) => true,
            (Some(1), Some(_)) => return Err(Error::BadRequest("latest=1 starts from now: leave out after".into())),
            (Some(_), _) => return Err(Error::BadRequest("latest is 1 or left out".into())),
        };
        let mut kinds = parse_kinds(q.kinds.as_deref())?;
        let limit = q.limit.unwrap_or(LIMIT).clamp(1, LIMIT_MAX);
        let after = q.after.unwrap_or(0).max(0);
        // Its secrets' row: a project in its recovery window (never purged) still answers.
        let state: Option<Option<i64>> = self.db.read(|c| c.query_row("SELECT purged_at FROM projects WHERE project_id = ?", [project], |r| r.get(0)).optional())?;
        match state {
            Some(None) => {}
            Some(Some(_)) => return Err(Error::NotFound(format!("no project {project}"))),
            // Nothing recorded yet: a workspace project with no events (or none at all: 404).
            None => {
                self.workspace.get(roll_workspace::Scope::All, project).map_err(|e| match e {
                    roll_workspace::Error::NotFound(m) => Error::NotFound(m),
                    e => Error::Internal(e.to_string()),
                })?;
                return Ok(EventPage { events: vec![], next: after });
            }
        }
        if latest {
            let head: Option<i64> = self.db.read(|c| c.query_row("SELECT seq FROM audit_heads WHERE project_id = ?", [project], |r| r.get(0)).optional())?;
            return Ok(EventPage { events: vec![], next: head.unwrap_or(0) });
        }
        if !self.sees_decoys(project, caller) {
            kinds.retain(|k| *k != Kind::DecoyUsed);
        }
        if kinds.is_empty() {
            return Ok(EventPage { events: vec![], next: after });
        }
        let sql = format!(
            "SELECT seq, at, env, path, keys, actor, event, event_data FROM audit WHERE project_id = ? AND seq > ? AND event IS NOT NULL AND event IN ({}){} ORDER BY seq LIMIT ?",
            vec!["?"; kinds.len()].join(", "),
            if q.environment.is_some() { " AND env = ?" } else { "" },
        );
        let args = |from: i64| {
            let mut a = vec![code_sql::Value::Text(project.into()), code_sql::Value::Int(from)];
            a.extend(kinds.iter().map(|k| code_sql::Value::Text(k.as_str().into())));
            a.extend(q.environment.iter().map(|e| code_sql::Value::Text(e.clone())));
            a.push(code_sql::Value::Int(PAGE));
            a
        };
        type Row = (i64, i64, Option<String>, Option<String>, String, String, String, Option<String>);
        let (mut events, mut next) = (Vec::new(), after);
        for _ in 0..PAGES {
            let args = args(next);
            let rows: Vec<Row> = self.db.read(|c| c.query(&sql, &args, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?))))?;
            let n = rows.len() as i64;
            for (seq, at, env, path, keys, actor, kind, data) in rows {
                next = seq;
                let path_or_root = path.as_deref().unwrap_or("/");
                if env.as_deref().is_some_and(|e| !caller.allows(Action::Read, &Scope::at(project, e, path_or_root))) {
                    continue;
                }
                events.push(feed_event(seq, at, env, path, &keys, actor, kind, data.as_deref()));
                if events.len() as i64 == limit {
                    return Ok(EventPage { events, next });
                }
            }
            if n < PAGE {
                break;
            }
        }
        Ok(EventPage { events, next })
    }
}

impl Secrets {
    /// `maker.unreachable`: whoever made `consumer` (a reference's resource, a placeholder's
    /// consumer, a sync) no longer reaches `env:path`. Once a day per maker and consumer: it's
    /// noticed on every use. In its own write; a failure is logged (the refusal stands anyway).
    pub(crate) fn note_unreachable(&self, project: &str, env: &str, path: &str, maker: &str, consumer: &str, why: &str) {
        let mark = format!("maker:{maker}:{consumer}:{env}:{path}");
        let res = self.keys().and_then(|mac| {
            let now = self.db.now_ms()?;
            // Products poll digests: a read first, so a refused poll writes nothing most times.
            let marked: Option<i64> = self.db.read(|c| c.query_row("SELECT at FROM event_marks WHERE project_id = ? AND mark = ?", params![project, mark], |r| r.get(0)).optional())?;
            if marked.is_some_and(|at| at >= now - MARK_MS) {
                return Ok(());
            }
            self.db.write(|c| {
                if fresh(c, project, &mark, now)? {
                    let event = Event::new(Kind::MakerUnreachable, json!({ "maker": maker, "consumer": consumer, "reason": why }));
                    crate::audit::record_event(c, mac, project, &Caller::consumer(consumer), "secrets.maker_unreachable", Some(env), Some(path), &[], why, now, event)?;
                }
                Ok::<_, Error>(())
            })
        });
        if let Err(e) = res {
            tracing::warn!(project, consumer, "recording an unreachable maker failed: {e}");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn feed_event(seq: i64, at: i64, environment: Option<String>, path: Option<String>, keys: &str, actor: String, kind: String, data: Option<&str>) -> FeedEvent {
    let mut d: Map<String, Value> = match data.and_then(|d| serde_json::from_str(d).ok()) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let keys: Vec<String> = from_json(keys);
    let key = (keys.len() == 1).then(|| keys[0].clone());
    if keys.len() > 1 {
        d.entry("keys").or_insert_with(|| json!(keys));
    }
    if let Some(p) = path {
        d.entry("path").or_insert_with(|| json!(p));
    }
    d.entry("actor").or_insert_with(|| json!(actor));
    FeedEvent { seq, at, kind, environment, key, data: Value::Object(d) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse_exactly_or_by_family() {
        assert_eq!(parse_kinds(None).unwrap().len(), KINDS.len());
        assert_eq!(parse_kinds(Some("request.*, sync.failed")).unwrap(), vec![Kind::RequestPending, Kind::RequestDecided, Kind::SyncFailed]);
        assert!(parse_kinds(Some("request")).is_err());
        assert!(parse_kinds(Some("nope.*")).is_err());
    }
}
