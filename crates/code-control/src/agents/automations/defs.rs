//! Automations as the owner defines them: the save-time check of a spec (every template parses,
//! every kind and source exists and takes its config, filters are well-formed, schedules
//! parse), saving, enabling, deleting, and the view the API and the stream show.

use super::registry::OwnerRef;
use super::schedule::Schedule;
use super::{filter, store, template};
use crate::{Code, Error, Result};
use agent_proto::StreamEvent;
use agent_proto::automation::{Automation, AutomationId, AutomationSpec, RunBrief, Trigger, WebhookIngress, limits};
use code_sql::{Conn, OptionalExtension, Row, params};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Inbound webhook secrets start with this.
pub const HOOK_PREFIX: &str = "rlh_";

/// An automation's row.
#[derive(Clone, Debug)]
pub struct AutomationRow {
    pub id: String,
    pub owner_id: String,
    pub spec: AutomationSpec,
    pub enabled: bool,
    pub revision: u64,
    pub cursor: u64,
    pub created_at: i64,
    pub updated_at: i64,
    pub created_by: String,
    pub deleted_at: Option<i64>,
    pub hook_secret: Option<String>,
    pub hook_hint: Option<String>,
    pub next_at: Option<i64>,
    pub last_run: Option<RunBrief>,
}

pub(crate) const COLS: &str = "id, owner_id, spec, enabled, revision, cursor, created_at, updated_at, created_by, deleted_at, hook_secret, hook_hint, next_at, last_run";

pub(crate) fn row(r: &Row<'_>) -> code_sql::Result<Option<AutomationRow>> {
    let spec: String = r.get(2)?;
    let Ok(spec) = serde_json::from_str::<AutomationSpec>(&spec) else { return Ok(None) };
    Ok(Some(AutomationRow {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        spec,
        enabled: r.get::<_, i64>(3)? != 0,
        revision: r.get::<_, i64>(4)?.max(0) as u64,
        cursor: r.get::<_, i64>(5)?.max(0) as u64,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
        created_by: r.get(8)?,
        deleted_at: r.get(9)?,
        hook_secret: r.get(10)?,
        hook_hint: r.get(11)?,
        next_at: r.get(12)?,
        last_run: r.get::<_, Option<String>>(13)?.and_then(|s| serde_json::from_str(&s).ok()),
    }))
}

pub(crate) fn get_on(c: &Conn<'_>, id: &str) -> code_sql::Result<Option<AutomationRow>> {
    Ok(c.query_row(&format!("SELECT {COLS} FROM automations WHERE id = ?"), [id], row).optional()?.flatten())
}

pub(crate) fn get(db: &code_sql::Db, id: &str) -> code_sql::Result<Option<AutomationRow>> {
    Ok(db.query_row(&format!("SELECT {COLS} FROM automations WHERE id = ?"), [id], row).optional()?.flatten())
}

/// The owner's automations, oldest first (deleted ones left out).
pub(crate) fn list(db: &code_sql::Db, owner_id: &str) -> code_sql::Result<Vec<AutomationRow>> {
    Ok(db.query(&format!("SELECT {COLS} FROM automations WHERE owner_id = ? AND deleted_at IS NULL ORDER BY created_at, id"), [owner_id], row)?.into_iter().flatten().collect())
}

/// Changed (or deleted) after `cursor`, for a stream catching up.
pub(crate) fn after(db: &code_sql::Db, owner_id: &str, cursor: u64, limit: i64) -> code_sql::Result<Vec<AutomationRow>> {
    Ok(db
        .query(&format!("SELECT {COLS} FROM automations WHERE owner_id = ? AND cursor > ? ORDER BY cursor LIMIT ?"), params![owner_id, cursor as i64, limit], row)?
        .into_iter()
        .flatten()
        .collect())
}

/// What a checked spec listens for, and when it fires next.
pub(crate) struct Checked {
    pub spec: AutomationSpec,
    /// (source, kind or `*`, repo id or `*`).
    pub listens: Vec<(String, String, String)>,
}

fn valid_name(s: &str, max: usize) -> bool {
    !s.is_empty() && s.chars().count() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// The save-time check. The message of an `Invalid` says where (`steps.2.config.prompt: …`).
pub(crate) fn check(code: &Code, owner: &OwnerRef, spec: &AutomationSpec) -> Result<Checked> {
    let bad = |m: String| Error::Invalid(m);
    let mut spec = spec.clone();
    spec.name = spec.name.trim().to_string();
    if spec.name.is_empty() || spec.name.chars().count() > limits::NAME || spec.name.chars().any(char::is_control) {
        return Err(bad(format!("name: one line of 1 to {} characters", limits::NAME)));
    }
    if spec.description.chars().count() > limits::DESCRIPTION {
        return Err(bad(format!("description: at most {} characters", limits::DESCRIPTION)));
    }
    let reg = &code.automations.registry;
    let mut listens = Vec::new();
    let all = || vec!["*".to_string()];
    match &spec.trigger {
        Trigger::Manual | Trigger::Webhook => {}
        Trigger::Schedule { .. } | Trigger::Interval { .. } => {
            Schedule::of(&spec.trigger).map_err(|e| bad(format!("trigger: {e}")))?;
        }
        Trigger::Code { events, repos } => {
            for e in events {
                if !crate::webhooks::EVENTS.contains(&e.as_str()) {
                    return Err(bad(format!("trigger.events: no roll code event {e:?} ({})", crate::webhooks::EVENTS.join(", "))));
                }
            }
            let mut ids = BTreeSet::new();
            for name in repos {
                let (o, r) = name.split_once('/').unwrap_or((owner.name.as_str(), name.as_str()));
                if !o.eq_ignore_ascii_case(&owner.name) {
                    return Err(bad(format!("trigger.repos: {name} isn't one of {}'s repos", owner.name)));
                }
                let repo = code.find_repo(o, r).map_err(|_| bad(format!("trigger.repos: no repo {name}")))?;
                ids.insert(repo.id);
            }
            let kinds = if events.is_empty() { all() } else { events.clone() };
            let repos: Vec<String> = if ids.is_empty() { all() } else { ids.into_iter().collect() };
            for k in &kinds {
                for r in &repos {
                    listens.push(("code".to_string(), k.clone(), r.clone()));
                }
            }
        }
        Trigger::Agent { events } => {
            if events.is_empty() {
                return Err(bad("trigger.events: at least one agent event".into()));
            }
            let kinds: BTreeSet<&str> = events.iter().map(|e| e.kind()).collect();
            listens.extend(kinds.into_iter().map(|k| ("agent".to_string(), k.to_string(), "*".to_string())));
        }
        Trigger::Integration { source, events, config } => {
            let t = reg.trigger(source).filter(|_| !BUILT_IN_SOURCES.contains(&source.as_str())).ok_or_else(|| bad(format!("trigger.source: no integration {source:?}")))?;
            t.check(owner, events, config).map_err(|e| bad(format!("trigger: {e}")))?;
            let kinds = if events.is_empty() { all() } else { events.clone() };
            listens.extend(kinds.into_iter().map(|k| (source.clone(), k, "*".to_string())));
        }
    }
    filter::check(&spec.filters, "filters").map_err(bad)?;
    if spec.steps.is_empty() || spec.steps.len() > limits::STEPS {
        return Err(bad(format!("steps: 1 to {}", limits::STEPS)));
    }
    let mut ids = BTreeSet::new();
    for (i, s) in spec.steps.iter().enumerate() {
        let at = format!("steps.{i}");
        if s.id.is_empty() || s.id.len() > 32 || !s.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(bad(format!("{at}.id: [a-z0-9_], 1 to 32 characters")));
        }
        if !ids.insert(s.id.as_str()) {
            return Err(bad(format!("{at}.id: {} is used twice", s.id)));
        }
        if s.name.chars().count() > limits::NAME {
            return Err(bad(format!("{at}.name: at most {} characters", limits::NAME)));
        }
        let kind = reg.step(&s.kind).ok_or_else(|| bad(format!("{at}.kind: no step kind {:?}", s.kind)))?;
        filter::check(&s.when, &format!("{at}.when")).map_err(bad)?;
        if let Some(r) = s.retry
            && (r.attempts == 0 || r.attempts > limits::ATTEMPTS || r.backoff_s == 0 || r.backoff_s > 3600)
        {
            return Err(bad(format!("{at}.retry: 1 to {} attempts, a backoff of 1 to 3600 s", limits::ATTEMPTS)));
        }
        if s.timeout_s.is_some_and(|t| t == 0 || t > 7 * 86_400) {
            return Err(bad(format!("{at}.timeout_s: 1 s to 7 days")));
        }
        let cfg_at = format!("{at}.config");
        let secrets_ok = |p: &str| p.strip_prefix(&cfg_at).is_some_and(|rest| kind.secrets_at(&format!("config{rest}")));
        template::check_json(&s.config, &cfg_at, &secrets_ok).map_err(|e| bad(e.0))?;
        kind.check(owner, &s.config).map_err(|e| bad(format!("{cfg_at}: {e}")))?;
    }
    if let Some(r) = spec.rate_limit
        && (r.max == 0 || r.max > 10_000 || r.per_s == 0 || r.per_s > 30 * 86_400)
    {
        return Err(bad("rate_limit: 1 to 10000 runs per 1 s to 30 days".into()));
    }
    if let Some(a) = &spec.run_as
        && !valid_name(a, 64)
    {
        return Err(bad("run_as: an agent name, [A-Za-z0-9-_.] (1 to 64)".into()));
    }
    Ok(Checked { spec, listens })
}

/// The trigger sources the engine keeps itself (not integrations).
pub const BUILT_IN_SOURCES: [&str; 6] = ["manual", "schedule", "interval", "webhook", "code", "agent"];

fn replace_listens(c: &Conn<'_>, id: &str, owner_id: &str, listens: &[(String, String, String)]) -> code_sql::Result<()> {
    c.execute("DELETE FROM automation_listens WHERE automation_id = ?", [id])?;
    for (source, kind, repo) in listens {
        c.execute(
            "INSERT INTO automation_listens (automation_id, owner_id, source, kind, repo_id) VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
            params![id, owner_id, source, kind, repo],
        )?;
    }
    Ok(())
}

/// When a trigger fires next after `now` (`None`: it doesn't fire by itself).
fn next_fire(spec: &AutomationSpec, enabled: bool, now: i64, anchor: i64) -> Option<i64> {
    if !enabled {
        return None;
    }
    Schedule::of(&spec.trigger).ok().flatten().and_then(|s| s.next_after(now, anchor))
}

fn new_hook_secret() -> String {
    format!("{HOOK_PREFIX}{}", crate::agents::api::random_hex(32))
}

fn hint(secret: &str) -> String {
    secret.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect()
}

/// A new webhook secret, sealed for the row: (secret, sealed, hint).
fn sealed_hook_secret(code: &Code, id: &str) -> Result<(String, String, String)> {
    let sealer = code.ai.sealer.as_ref().ok_or_else(|| Error::Unprocessable("webhook triggers need ROLL_SECRET_KEY on the server".into()))?;
    let secret = new_hook_secret();
    let sealed = sealer.seal(&store::secret_scope(id, "webhook"), &secret);
    let h = hint(&secret);
    Ok((secret, sealed, h))
}

/// The inbound webhook secret of an automation, opened.
pub(crate) fn hook_secret(code: &Code, a: &AutomationRow) -> Option<String> {
    let sealed = a.hook_secret.as_deref()?;
    code.ai.sealer.as_ref()?.open(&store::secret_scope(&a.id, "webhook"), sealed).ok()
}

/// What saving did.
pub(crate) enum Saved {
    Done { row: Box<AutomationRow>, webhook_secret: Option<String> },
    /// The revision wasn't the current one.
    Conflict(Box<AutomationRow>),
    NotFound,
    TooMany,
}

/// Create an automation.
pub(crate) async fn create(code: &Code, owner: &OwnerRef, by: &str, checked: Checked) -> Result<Saved> {
    let id = AutomationId(crate::new_id("aut"));
    let hook = match checked.spec.trigger {
        Trigger::Webhook => Some(sealed_hook_secret(code, id.as_str())?),
        _ => None,
    };
    let now = code_sql::now_ms();
    let next_at = next_fire(&checked.spec, checked.spec.enabled, now, now);
    let spec_json = serde_json::to_string(&checked.spec).unwrap_or_default();
    let hub = code.agents.hub(&owner.id);
    let _order = hub.order.lock().await;
    let saved = code.db.sql().write(|c| -> code_sql::Result<_> {
        let n: i64 = c.query_row("SELECT COUNT(*) FROM automations WHERE owner_id = ? AND deleted_at IS NULL", [&owner.id], |r| r.get(0))?;
        if n as usize >= limits::AUTOMATIONS {
            return Ok(Saved::TooMany);
        }
        let cursor = crate::agents::store::take_cursors(c, &owner.id, 1)?;
        c.execute(
            "INSERT INTO automations (id, owner_id, spec, enabled, source, revision, cursor, created_at, updated_at, created_by, hook_secret, hook_hint, next_at) \
             VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?)",
            params![
                id.as_str(),
                &owner.id,
                spec_json,
                checked.spec.enabled as i64,
                checked.spec.trigger.source(),
                cursor as i64,
                now,
                now,
                by,
                hook.as_ref().map(|h| h.1.clone()),
                hook.as_ref().map(|h| h.2.clone()),
                next_at
            ],
        )?;
        if checked.spec.enabled {
            replace_listens(c, id.as_str(), &owner.id, &checked.listens)?;
        }
        Ok(match get_on(c, id.as_str())? {
            Some(row) => Saved::Done { row: Box::new(row), webhook_secret: hook.as_ref().map(|h| h.0.clone()) },
            None => Saved::NotFound,
        })
    })?;
    if let Saved::Done { row, .. } = &saved {
        publish(code, row);
    }
    Ok(saved)
}

/// Replace an automation's definition (over `expect`, when given).
pub(crate) async fn update(code: &Code, owner: &OwnerRef, id: &str, checked: Checked, expect: Option<u64>) -> Result<Saved> {
    let now = code_sql::now_ms();
    let hook = match checked.spec.trigger {
        Trigger::Webhook => Some(sealed_hook_secret(code, id)?),
        _ => None,
    };
    let spec_json = serde_json::to_string(&checked.spec).unwrap_or_default();
    let hub = code.agents.hub(&owner.id);
    let _order = hub.order.lock().await;
    let saved = code.db.sql().write(|c| -> code_sql::Result<_> {
        let Some(cur) = get_on(c, id)?.filter(|a| a.owner_id == owner.id && a.deleted_at.is_none()) else { return Ok(Saved::NotFound) };
        if expect.is_some_and(|e| e != cur.revision) {
            return Ok(Saved::Conflict(Box::new(cur)));
        }
        // The same schedule keeps its next firing; a new one starts from now.
        let next_at = if cur.spec.trigger == checked.spec.trigger && cur.enabled == checked.spec.enabled && cur.next_at.is_some() {
            cur.next_at
        } else {
            next_fire(&checked.spec, checked.spec.enabled, now, cur.created_at)
        };
        // A webhook keeps its secret; one that becomes a webhook gets one; others drop theirs.
        let keep = matches!(cur.spec.trigger, Trigger::Webhook) && cur.hook_secret.is_some();
        let (secret, sealed, h) = match (&hook, keep) {
            (Some(_), true) => (None, cur.hook_secret.clone(), cur.hook_hint.clone()),
            (Some((s, sealed, h)), false) => (Some(s.clone()), Some(sealed.clone()), Some(h.clone())),
            (None, _) => (None, None, None),
        };
        let cursor = crate::agents::store::take_cursors(c, &owner.id, 1)?;
        c.execute(
            "UPDATE automations SET spec = ?, enabled = ?, source = ?, revision = revision + 1, cursor = ?, updated_at = ?, hook_secret = ?, hook_hint = ?, next_at = ? WHERE id = ?",
            params![spec_json, checked.spec.enabled as i64, checked.spec.trigger.source(), cursor as i64, now, sealed, h, next_at, id],
        )?;
        replace_listens(c, id, &owner.id, if checked.spec.enabled { &checked.listens } else { &[] })?;
        Ok(match get_on(c, id)? {
            Some(row) => Saved::Done { row: Box::new(row), webhook_secret: secret },
            None => Saved::NotFound,
        })
    })?;
    if let Saved::Done { row, .. } = &saved {
        publish(code, row);
    }
    Ok(saved)
}

/// Turn an automation on or off. `None`: there's no such automation.
pub(crate) async fn set_enabled(code: &Code, owner: &OwnerRef, id: &str, enabled: bool) -> Result<Option<AutomationRow>> {
    let Some(cur) = get(code.db.sql(), id)?.filter(|a| a.owner_id == owner.id && a.deleted_at.is_none()) else { return Ok(None) };
    let mut spec = cur.spec.clone();
    spec.enabled = enabled;
    let checked = check(code, owner, &spec)?;
    match update(code, owner, id, checked, None).await? {
        Saved::Done { row, .. } => Ok(Some(*row)),
        _ => Ok(None),
    }
}

/// A new inbound webhook secret for a webhook automation.
pub(crate) async fn rotate_hook_secret(code: &Code, owner: &OwnerRef, id: &str) -> Result<Saved> {
    let (secret, sealed, h) = sealed_hook_secret(code, id)?;
    let hub = code.agents.hub(&owner.id);
    let _order = hub.order.lock().await;
    let saved = code.db.sql().write(|c| -> code_sql::Result<_> {
        let Some(cur) = get_on(c, id)?.filter(|a| a.owner_id == owner.id && a.deleted_at.is_none()) else { return Ok(Saved::NotFound) };
        if !matches!(cur.spec.trigger, Trigger::Webhook) {
            return Ok(Saved::NotFound);
        }
        let cursor = crate::agents::store::take_cursors(c, &owner.id, 1)?;
        c.execute(
            "UPDATE automations SET hook_secret = ?, hook_hint = ?, cursor = ?, updated_at = ? WHERE id = ?",
            params![sealed, h, cursor as i64, code_sql::now_ms(), id],
        )?;
        Ok(match get_on(c, id)? {
            Some(row) => Saved::Done { row: Box::new(row), webhook_secret: Some(secret.clone()) },
            None => Saved::NotFound,
        })
    })?;
    if let Saved::Done { row, .. } = &saved {
        publish(code, row);
    }
    Ok(saved)
}

/// Delete an automation: a tombstone (for streams catching up), its listens and secrets gone,
/// its runs going on cancelled. Whether it was there.
pub(crate) async fn delete(code: &Code, owner: &OwnerRef, id: &str) -> Result<bool> {
    let now = code_sql::now_ms();
    let hub = code.agents.hub(&owner.id);
    let _order = hub.order.lock().await;
    let done = code.db.sql().write(|c| -> code_sql::Result<_> {
        let Some(cur) = get_on(c, id)?.filter(|a| a.owner_id == owner.id && a.deleted_at.is_none()) else { return Ok(None) };
        let cursor = crate::agents::store::take_cursors(c, &owner.id, 1)?;
        c.execute(
            "UPDATE automations SET deleted_at = ?, cursor = ?, updated_at = ?, hook_secret = NULL, next_at = NULL, enabled = 0 WHERE id = ?",
            params![now, cursor as i64, now, id],
        )?;
        c.execute("DELETE FROM automation_listens WHERE automation_id = ?", [id])?;
        c.execute("DELETE FROM automation_secrets WHERE automation_id = ?", [id])?;
        let cancelled = super::engine::cancel_runs_of(c, &owner.id, id, "the automation was deleted")?;
        Ok(Some((cur, cursor, cancelled)))
    })?;
    let Some((_, cursor, cancelled)) = done else { return Ok(false) };
    let _ = hub.events.send(Arc::new(StreamEvent::AutomationRemoved { cursor, automation: AutomationId(id.to_string()) }));
    for r in cancelled {
        let _ = hub.events.send(Arc::new(StreamEvent::AutomationRun { cursor: r.cursor, run: Box::new(r) }));
    }
    code.wake_automations(&owner.id);
    Ok(true)
}

/// Set or drop one of an automation's secrets (sealed). `false`: no such automation.
pub(crate) async fn set_secret(code: &Code, owner: &OwnerRef, id: &str, name: &str, value: Option<&str>) -> Result<bool> {
    let sealed = match value {
        Some(v) => {
            let sealer = code.ai.sealer.as_ref().ok_or_else(|| Error::Unprocessable("automation secrets need ROLL_SECRET_KEY on the server".into()))?;
            Some(sealer.seal(&store::secret_scope(id, name), v))
        }
        None => None,
    };
    let now = code_sql::now_ms();
    let hub = code.agents.hub(&owner.id);
    let _order = hub.order.lock().await;
    let row = code.db.sql().write(|c| -> code_sql::Result<_> {
        let Some(_) = get_on(c, id)?.filter(|a| a.owner_id == owner.id && a.deleted_at.is_none()) else { return Ok(Err(false)) };
        match &sealed {
            Some(s) => {
                let n: i64 = c.query_row("SELECT COUNT(*) FROM automation_secrets WHERE automation_id = ? AND name <> ?", [id, name], |r| r.get(0))?;
                if n as usize >= limits::SECRETS {
                    return Ok(Err(true));
                }
                c.execute(
                    "INSERT INTO automation_secrets (automation_id, name, sealed, updated_at) VALUES (?, ?, ?, ?) \
                     ON CONFLICT (automation_id, name) DO UPDATE SET sealed = excluded.sealed, updated_at = excluded.updated_at",
                    params![id, name, s, now],
                )?;
            }
            None => {
                c.execute("DELETE FROM automation_secrets WHERE automation_id = ? AND name = ?", [id, name])?;
            }
        }
        let cursor = crate::agents::store::take_cursors(c, &owner.id, 1)?;
        c.execute("UPDATE automations SET cursor = ?, updated_at = ? WHERE id = ?", params![cursor as i64, now, id])?;
        Ok(Ok(get_on(c, id)?))
    })?;
    match row {
        Ok(Some(r)) => {
            publish(code, &r);
            Ok(true)
        }
        Ok(None) | Err(false) => Ok(false),
        Err(true) => Err(Error::Unprocessable(format!("an automation has at most {} secrets", limits::SECRETS))),
    }
}

/// The names of an automation's secrets.
pub(crate) fn secret_names(db: &code_sql::Db, id: &str) -> code_sql::Result<Vec<String>> {
    db.query("SELECT name FROM automation_secrets WHERE automation_id = ? ORDER BY name", [id], |r| r.get(0))
}

/// The automation as the API shows it.
pub(crate) fn view(code: &Code, a: &AutomationRow) -> Automation {
    let owner = crate::agents::store::owner_name(code.db.sql(), &a.owner_id).ok().flatten().unwrap_or_default();
    let webhook = matches!(a.spec.trigger, Trigger::Webhook).then(|| WebhookIngress {
        url: format!("{}/v1/code/agents/{}/automations/{}/webhook", code.config.api_url.trim_end_matches('/'), owner, a.id),
        secret_hint: a.hook_hint.clone().unwrap_or_default(),
    });
    let mut spec = a.spec.clone();
    spec.enabled = a.enabled;
    Automation {
        id: AutomationId(a.id.clone()),
        spec,
        revision: a.revision,
        cursor: a.cursor,
        created_at: a.created_at.max(0) as u64,
        updated_at: a.updated_at.max(0) as u64,
        created_by: a.created_by.clone(),
        secrets: secret_names(code.db.sql(), &a.id).unwrap_or_default(),
        webhook,
        next_at: a.next_at.map(|t| t.max(0) as u64),
        last_run: a.last_run.clone(),
    }
}

/// Send an automation's change to the owner's streams. Callers hold the order lock across the
/// write that took its cursor and this.
pub(crate) fn publish(code: &Code, a: &AutomationRow) {
    let ev = StreamEvent::Automation { cursor: a.cursor, automation: Box::new(view(code, a)) };
    let _ = code.agents.hub(&a.owner_id).events.send(Arc::new(ev));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_are_the_last_four() {
        assert_eq!(hint("rlh_0123456789abcdef"), "cdef");
        assert!(new_hook_secret().starts_with(HOOK_PREFIX));
    }
}
