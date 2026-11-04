//! The built-in triggers (docs/code/automations.md, "Triggers"): their catalog entries, and how
//! their events reach the inbox durably:
//!
//! - roll code events are recorded by the writes that queue webhook deliveries (`webhooks.rs`):
//!   [`code_listens`] makes a promised event's outbox row count automations as listeners, and
//!   [`record_code_event`] records the event in the owner's inbox in the same write;
//! - agent events are recorded by the relay's mirror, in the write that stores the daemon's
//!   records ([`record_agent_events`]);
//! - schedules fire in the engine; webhooks and manual runs are recorded by their routes.

use super::registry::{NewEvent, Registry, TriggerSource};
use super::store;
use crate::model::RepoRow;
use agent_proto::automation::{AgentEvent, EventInfo, TriggerInfo};
use agent_proto::{Entity, RequestStatus, RunStatus, Settle, ThreadStatus};
use code_sql::{Conn, params};
use serde_json::{Value, json};
use std::sync::Arc;

struct BuiltIn(TriggerInfo);

impl TriggerSource for BuiltIn {
    fn info(&self) -> TriggerInfo {
        self.0.clone()
    }
}

fn ev(kind: &str, title: &str, fields: &[&str]) -> EventInfo {
    EventInfo { kind: kind.into(), title: title.into(), fields: fields.iter().map(|f| f.to_string()).collect(), sample: Value::Null }
}

fn with(mut e: EventInfo, sample: Value) -> EventInfo {
    e.sample = sample;
    e
}

fn built_in(source: &str, title: &str, description: &str, events: Vec<EventInfo>) -> Arc<BuiltIn> {
    Arc::new(BuiltIn(TriggerInfo { source: source.into(), title: title.into(), description: description.into(), events, config: Value::Null, needs_setup: false }))
}

pub(crate) fn register_built_ins(r: &Registry) {
    r.register_trigger(built_in("manual", "By hand", "Run it from the dashboard, `roll agent automation run`, or the MCP tool.", vec![with(ev("run", "Run", &["input", "by"]), json!({ "by": "acme", "input": { "version": "1.4.0" } }))]));
    r.register_trigger(built_in("schedule", "On a schedule", "A cron expression in a time zone.", vec![with(ev("tick", "Tick", &["at", "scheduled"]), json!({ "at": 1_762_000_000_000_i64, "scheduled": 1_762_000_000_000_i64 }))]));
    r.register_trigger(built_in("interval", "Every so often", "Every so many seconds (at least 60).", vec![ev("tick", "Tick", &["at", "scheduled"])]));
    r.register_trigger(built_in("webhook", "A webhook", "A POST to the automation's own URL, signed with its secret (roll's or GitHub's signature).", vec![with(ev("post", "Post", &["body", "headers"]), json!({ "body": { "status": "green", "ref": "main" }, "headers": { "content-type": "application/json" } }))]));
    let code_fields = ["repo", "change.number", "change.title", "change.state", "change.author", "change.labels", "check.name", "check.status"];
    r.register_trigger(built_in(
        "code",
        "roll code",
        "Events of the owner's repos: changes, checks, reviews, the merge queue, landings, pushes, work items, previews.",
        crate::webhooks::EVENTS
            .iter()
            .map(|e| with(ev(e, e, &code_fields), json!({ "repo": "acme/app", "change": { "number": 12, "title": "Fix the flaky queue test", "state": "open", "author": "ada", "labels": ["bug"], "url": "https://roll.sh/acme/app/changes/12" }, "check": { "name": "test", "status": "failed" } })))
            .collect(),
    ));
    let agent_fields = ["machine", "project", "thread.title", "thread.status", "thread.provider", "run.status", "run.error", "request.body.type"];
    r.register_trigger(built_in(
        "agent",
        "Agents",
        "The owner's agents: threads starting, finishing, failing, waiting on you or settling, requests opening.",
        AgentEvent::ALL
            .iter()
            .map(|e| with(ev(e.kind(), e.kind(), &agent_fields), json!({ "machine": "mch_0123456789abcd", "project": "prj_0123456789abcd", "thread": { "id": "thr_0123456789abcd", "title": "Fix the flaky queue test", "status": "idle", "provider": "codex" }, "run": { "id": "run_0123456789abcd", "status": "completed", "error": null } })))
            .collect(),
    ));
}

// ── roll code ──

/// Whether an enabled automation of the repo's owner listens for `kind` on it (read on `c`).
pub(crate) fn code_listens(c: &Conn<'_>, repo_id: &str, kind: &str) -> code_sql::Result<bool> {
    c.query_row(
        "SELECT EXISTS (SELECT 1 FROM automation_listens l JOIN repos r ON r.owner_id = l.owner_id \
         WHERE r.id = ? AND l.source = 'code' AND (l.kind = ? OR l.kind = '*') AND (l.repo_id = r.id OR l.repo_id = '*'))",
        params![repo_id, kind],
        |r| r.get::<_, bool>(0),
    )
}

/// Record a roll code event in the repo owner's inbox when an automation listens for it, inside
/// the write that queues its webhook deliveries. `id` is the event's own id (its outbox row, a
/// land log position, or a fresh one). Whether it was recorded.
pub(crate) fn record_code_event(c: &Conn<'_>, r: &RepoRow, kind: &str, data: &Value, id: &str) -> code_sql::Result<bool> {
    if !code_listens(c, &r.id, kind)? {
        return Ok(false);
    }
    let mut d = data.clone();
    if let Some(o) = d.as_object_mut() {
        o.insert("repo".into(), json!(r.full_name()));
        o.insert("repo_id".into(), json!(r.id));
    } else {
        d = json!({ "repo": r.full_name(), "repo_id": r.id, "value": d });
    }
    let summary = code_summary(r, kind, &d);
    let e = NewEvent { source: "code".into(), kind: kind.into(), id: id.into(), data: d, summary, automation: None };
    Ok(store::insert_event(c, &r.owner_id, &e)?.is_some())
}

fn code_summary(r: &RepoRow, kind: &str, d: &Value) -> String {
    let n = d.pointer("/change/number").or_else(|| d.get("number")).and_then(Value::as_u64);
    match n {
        Some(n) => format!("{}#{n}: {kind}", r.full_name()),
        None => format!("{}: {kind}", r.full_name()),
    }
}

// ── agents ──

/// Whether the owner has an automation listening for agent events, or a run waiting on one of
/// this machine's threads (read once per mirrored batch).
pub(crate) fn agents_listen(c: &Conn<'_>, owner_id: &str) -> code_sql::Result<bool> {
    c.query_row(
        "SELECT (EXISTS (SELECT 1 FROM automation_listens WHERE owner_id = ?1 AND source = 'agent')) \
         OR (EXISTS (SELECT 1 FROM automation_threads WHERE owner_id = ?1))",
        [owner_id],
        |r| r.get::<_, bool>(0),
    )
}

fn thread_brief(t: &agent_proto::Thread) -> Value {
    json!({
        "id": t.id, "title": t.title, "status": t.status, "provider": t.provider, "model": t.model,
        "project": t.project, "worktree": t.worktree, "change": t.change, "work": t.work, "error": t.error,
        "orchestrator": t.orchestrator, "parent": t.lineage.as_ref().map(|l| l.parent.clone()),
    })
}

/// The agent events one mirrored record makes, given what the relay held before (`old`).
/// (kind, id suffix, data, summary).
pub(crate) fn agent_events(machine: &str, old: Option<&Entity>, new: &Entity, seq: u64) -> Vec<(AgentEvent, String, Value, String)> {
    let mut out = Vec::new();
    match (old, new) {
        (old, Entity::Thread(t)) => {
            let prev = match old {
                Some(Entity::Thread(p)) => Some(p),
                _ => None,
            };
            let base = json!({ "machine": machine, "project": t.project, "thread": thread_brief(t) });
            if prev.is_none() {
                out.push((AgentEvent::ThreadCreated, format!("{}:created", t.id), base.clone(), format!("{} started", t.title)));
            }
            if t.status == ThreadStatus::Waiting && prev.is_none_or(|p| p.status != ThreadStatus::Waiting) {
                out.push((AgentEvent::ThreadWaiting, format!("{}:waiting:{seq}", t.id), base.clone(), format!("{} waits on you", t.title)));
            }
            let settled = |x: &agent_proto::Thread| matches!(x.inbox.settle, Some(Settle::Settled { .. }));
            if settled(t) && prev.is_none_or(|p| !settled(p)) {
                out.push((AgentEvent::ThreadSettled, format!("{}:settled:{seq}", t.id), base, format!("{} settled", t.title)));
            }
        }
        (old, Entity::Run(r)) => {
            let was = match old {
                Some(Entity::Run(p)) => Some(p.status),
                _ => None,
            };
            let kind = match r.status {
                RunStatus::Completed => AgentEvent::ThreadDone,
                RunStatus::Failed => AgentEvent::ThreadFailed,
                _ => return out,
            };
            if was == Some(r.status) {
                return out;
            }
            let data = json!({
                "machine": machine, "thread": { "id": r.thread }, "run": {
                    "id": r.id, "ordinal": r.ordinal, "status": r.status, "error": r.error, "provider": r.provider, "model": r.model,
                    "prompt": r.prompt.chars().take(2000).collect::<String>(), "stat": r.stat,
                } });
            let what = if kind == AgentEvent::ThreadDone { "finished" } else { "failed" };
            out.push((kind, format!("{}:{}", r.id, kind.kind()), data, format!("{} {what} run {}", r.thread, r.ordinal)));
        }
        (old, Entity::Request(q)) if q.status == RequestStatus::Pending && !matches!(old, Some(Entity::Request(_))) => {
            let data = json!({ "machine": machine, "thread": { "id": q.thread }, "run": { "id": q.run }, "request": q });
            out.push((AgentEvent::RequestOpened, format!("{}:opened", q.id), data, format!("{} asks", q.thread)));
        }
        _ => {}
    }
    out
}

/// Record a mirrored record's agent events in the owner's inbox (inside the mirror's write),
/// and wake the runs waiting on its thread. Keys carry ids or the record's seq, so a daemon's
/// replay records nothing twice. Whether anything was recorded or woken.
pub(crate) fn record_agent_events(c: &Conn<'_>, owner_id: &str, machine: &str, old: Option<&Entity>, new: &Entity, seq: u64) -> code_sql::Result<bool> {
    let mut any = false;
    if let Some(thread) = new.thread() {
        any |= crate::Code::nudge_runs_of_thread(c, machine, thread.as_str())? > 0;
    }
    for (kind, id, data, summary) in agent_events(machine, old, new, seq) {
        let listens: bool = c.query_row(
            "SELECT EXISTS (SELECT 1 FROM automation_listens WHERE owner_id = ? AND source = 'agent' AND kind = ?)",
            params![owner_id, kind.kind()],
            |r| r.get(0),
        )?;
        if !listens {
            continue;
        }
        let e = NewEvent { source: "agent".into(), kind: kind.kind().into(), id: format!("{machine}:{id}"), data, summary, automation: None };
        any |= store::insert_event(c, owner_id, &e)?.is_some();
    }
    Ok(any)
}

// ── inbound webhooks ──

/// How old a signed timestamp may be.
pub const REPLAY_WINDOW_S: i64 = 300;

/// Check an inbound webhook's signature; its idempotency key. `Err` says why it's refused.
pub(crate) fn verify(secret: &str, headers: &axum::http::HeaderMap, body: &[u8], now_s: i64) -> Result<String, String> {
    use hmac::{Hmac, Mac};
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim);
    let eq = |a: &str, b: &str| a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0;
    if let Some(sig) = h("x-roll-signature-256") {
        let ts: i64 = h("x-roll-timestamp").and_then(|t| t.parse().ok()).ok_or("X-Roll-Timestamp is missing")?;
        if (now_s - ts).abs() > REPLAY_WINDOW_S {
            return Err("the timestamp is too old (or in the future): sign it at sending time".into());
        }
        if !eq(sig, &crate::review::apps::signature(secret, ts, body)) {
            return Err("the signature doesn't match".into());
        }
        return Ok(match h("x-roll-delivery").filter(|d| !d.is_empty() && d.len() <= 128) {
            Some(d) => format!("d:{d}"),
            None => format!("s:{sig}"),
        });
    }
    if let Some(sig) = h("x-hub-signature-256") {
        let delivery = h("x-github-delivery").filter(|d| !d.is_empty() && d.len() <= 128).ok_or("X-GitHub-Delivery is missing")?;
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).map_err(|e| e.to_string())?;
        mac.update(body);
        let want = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        if !eq(sig, &want) {
            return Err("the signature doesn't match".into());
        }
        return Ok(format!("gh:{delivery}"));
    }
    Err("unsigned: send X-Roll-Signature-256 and X-Roll-Timestamp (or GitHub's X-Hub-Signature-256)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn hm(pairs: &[(&'static str, String)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn signatures() {
        let body = br#"{"a":1}"#;
        let sig = crate::review::apps::signature("rlh_s", 1000, body);
        let ok = hm(&[("x-roll-signature-256", sig.clone()), ("x-roll-timestamp", "1000".into()), ("x-roll-delivery", "d1".into())]);
        assert_eq!(verify("rlh_s", &ok, body, 1100), Ok("d:d1".into()));
        assert!(verify("rlh_s", &ok, body, 1000 + REPLAY_WINDOW_S + 1).is_err(), "too old");
        assert!(verify("other", &ok, body, 1100).is_err());
        assert!(verify("rlh_s", &ok, b"{}", 1100).is_err());
        let no_delivery = hm(&[("x-roll-signature-256", sig.clone()), ("x-roll-timestamp", "1000".into())]);
        assert_eq!(verify("rlh_s", &no_delivery, body, 1000), Ok(format!("s:{sig}")));
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"rlh_s").unwrap();
        mac.update(body);
        let gh = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let ghh = hm(&[("x-hub-signature-256", gh), ("x-github-delivery", "g1".into())]);
        assert_eq!(verify("rlh_s", &ghh, body, 0), Ok("gh:g1".into()));
        assert!(verify("rlh_s", &HeaderMap::new(), body, 0).is_err());
    }

    #[test]
    fn agent_events_fire_on_transitions_only() {
        let thread: agent_proto::Thread = serde_json::from_value(json!({
            "id": "thr_1", "project": "prj_1", "title": "Fix", "provider": "codex", "model": null, "runtime_mode": "full_access",
            "interaction_mode": "default", "cwd": "/w", "status": "running", "run_count": 1, "pending_requests": 0, "created_at": 1, "updated_at": 1
        }))
        .unwrap();
        let created = agent_events("mch_1", None, &Entity::Thread(thread.clone()), 1);
        assert_eq!(created.iter().map(|e| e.0).collect::<Vec<_>>(), vec![AgentEvent::ThreadCreated]);
        let waiting = agent_proto::Thread { status: ThreadStatus::Waiting, ..thread.clone() };
        let ev = agent_events("mch_1", Some(&Entity::Thread(thread.clone())), &Entity::Thread(waiting.clone()), 2);
        assert_eq!(ev.iter().map(|e| e.0).collect::<Vec<_>>(), vec![AgentEvent::ThreadWaiting]);
        assert!(agent_events("mch_1", Some(&Entity::Thread(waiting.clone())), &Entity::Thread(waiting), 3).is_empty());
    }
}
