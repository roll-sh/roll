//! Outbound repo webhooks and the commit status API for external CI
//! (docs/code/http-api.md, "Webhooks and commit statuses").
//!
//! - **Webhooks**: a repo's writers register URLs for events (`landed`, `change`, `review`,
//!   `thread`, `check`, `queue`, `work`, `preview`, `push`, plus `ping`). Deliveries go through
//!   the review apps' machinery ([`crate::review::apps`]): the same signature
//!   (`X-Roll-Signature-256` over `{timestamp}.{body}`), SSRF guard, retries with backoff,
//!   delivery log and redelivery. Events reach webhooks through an unbounded channel, so emitting
//!   one never blocks and none are dropped. Landings come from the store's durable land log
//!   instead, read after a cursor persisted here: a burst larger than the broadcast channel, or
//!   a crash between landing and delivery, never loses one (at-least-once).
//! - **Commit statuses** (GitHub's API shape): CI that knows a commit (TeamCity, Jenkins,
//!   Buildkite, GitHub Actions-style runners) posts `{state, context, target_url, description}`
//!   for a git commit or changeset. Each status is recorded for the commit and becomes a check
//!   (named after its context) on every open change whose head it is — which is what land
//!   policies and the merge queue's `required_checks` look at.

use crate::db::{Db, now};
use crate::model::{Change, ChangeState, RepoRow};
use crate::review::apps::check_url;
use crate::{Caller, Code, Error, Result, cs_id, new_id};
use code_store::ChangesetId;
use parking_lot::Mutex;
use code_sql::{OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS repo_webhooks (
    id TEXT PRIMARY KEY,
    repo_id TEXT NOT NULL,
    url TEXT NOT NULL,
    events TEXT NOT NULL,
    secret TEXT NOT NULL,
    active INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    since_seq INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS repo_webhooks_repo ON repo_webhooks (repo_id);
CREATE TABLE IF NOT EXISTS webhook_land_cursor (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    seq INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS commit_statuses (
    repo_id TEXT NOT NULL,
    changeset TEXT NOT NULL,
    context TEXT NOT NULL,
    state TEXT NOT NULL,
    target_url TEXT,
    description TEXT,
    creator TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (repo_id, changeset, context)
);
"#;

/// Events a webhook can subscribe to (`ping` is always sent on request).
pub const EVENTS: [&str; 9] = ["landed", "change", "review", "thread", "check", "queue", "work", "preview", "push"];
const MAX_HOOKS_PER_REPO: usize = 20;
const STATES: [&str; 4] = ["pending", "success", "failure", "error"];
/// Land log rows read at a time.
const LAND_PAGE: usize = 500;
/// How often the land log is read when no landing wakes the reader.
const LAND_POLL: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct HookRow {
    pub id: String,
    pub repo_id: String,
    pub url: String,
    pub events: Vec<String>,
    pub secret: String,
    pub active: bool,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// The land log position when it was created: only later landings are delivered.
    pub since_seq: i64,
}

const HOOK_COLS: &str = "id, repo_id, url, events, secret, active, created_by, created_at, updated_at, since_seq";

fn hook_row(r: &Row<'_>) -> code_sql::Result<HookRow> {
    Ok(HookRow {
        id: r.get(0)?,
        repo_id: r.get(1)?,
        url: r.get(2)?,
        events: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
        secret: r.get(4)?,
        active: r.get::<_, i64>(5)? != 0,
        created_by: r.get(6)?,
        created_at: r.get(7)?,
        updated_at: r.get(8)?,
        since_seq: r.get(9)?,
    })
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Status {
    pub context: String,
    pub state: String,
    pub target_url: Option<String>,
    pub description: Option<String>,
    pub creator: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Db {
    pub fn webhooks(&self, repo_id: &str) -> Result<Vec<HookRow>> {
        Ok(self.sql().query(&format!("SELECT {HOOK_COLS} FROM repo_webhooks WHERE repo_id = ? ORDER BY created_at, id"), [repo_id], hook_row)?)
    }

    pub fn webhook(&self, id: &str) -> Result<Option<HookRow>> {
        Ok(self.sql().query_row(&format!("SELECT {HOOK_COLS} FROM repo_webhooks WHERE id = ?"), [id], hook_row).optional()?)
    }

    pub fn any_webhooks(&self) -> Result<bool> {
        Ok(self.sql().query_row("SELECT EXISTS (SELECT 1 FROM repo_webhooks)", (), |r| r.get::<_, i64>(0))? != 0)
    }

    fn save_webhook(&self, h: &HookRow) -> Result<()> {
        self.sql().execute(
            "INSERT INTO repo_webhooks (id, repo_id, url, events, secret, active, created_by, created_at, updated_at, since_seq) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (id) DO UPDATE SET url = excluded.url, events = excluded.events, secret = excluded.secret, \
             active = excluded.active, updated_at = excluded.updated_at",
            params![h.id, h.repo_id, h.url, serde_json::to_string(&h.events).unwrap_or_default(), h.secret, h.active as i64, h.created_by, h.created_at, h.updated_at, h.since_seq],
        )?;
        Ok(())
    }

    fn delete_webhook(&self, id: &str) -> Result<()> {
        Ok(self.write(|c| {
            c.execute("DELETE FROM repo_webhooks WHERE id = ?", [id])?;
            c.execute("DELETE FROM review_deliveries WHERE app_id = ?", [id])?;
            Ok(())
        })?)
    }

    fn upsert_status(&self, repo_id: &str, cs: &str, s: &Status) -> Result<()> {
        self.sql().execute(
            "INSERT INTO commit_statuses (repo_id, changeset, context, state, target_url, description, creator, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (repo_id, changeset, context) DO UPDATE SET state = excluded.state, \
             target_url = excluded.target_url, description = excluded.description, creator = excluded.creator, updated_at = excluded.updated_at",
            params![repo_id, cs, s.context, s.state, s.target_url, s.description, s.creator, s.created_at, s.updated_at],
        )?;
        Ok(())
    }

    pub fn statuses(&self, repo_id: &str, cs: &str) -> Result<Vec<Status>> {
        Ok(self.sql().query(
            "SELECT context, state, target_url, description, creator, created_at, updated_at FROM commit_statuses \
             WHERE repo_id = ? AND changeset = ? ORDER BY context",
            params![repo_id, cs],
            |r| {
                Ok(Status {
                    context: r.get(0)?,
                    state: r.get(1)?,
                    target_url: r.get(2)?,
                    description: r.get(3)?,
                    creator: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            },
        )?)
    }

    /// Open (or queued) changes whose head is `head`.
    fn changes_at_head(&self, repo_id: &str, head: &str) -> Result<Vec<String>> {
        Ok(self.sql().query(
            "SELECT id FROM changes WHERE repo_id = ? AND head = ? AND state IN ('open', 'queued', 'conflict') ORDER BY number",
            params![repo_id, head],
            |r| r.get(0),
        )?)
    }

    /// How far into the store's land log webhooks have queued deliveries.
    fn land_cursor(&self) -> Result<Option<i64>> {
        Ok(self.sql().query_row("SELECT seq FROM webhook_land_cursor WHERE id = 1", (), |r| r.get(0)).optional()?)
    }

    fn set_land_cursor(&self, seq: i64) -> Result<()> {
        self.sql().execute(
            // Only ever forward (two forwarders may race); `max(a, b)` isn't portable.
            "INSERT INTO webhook_land_cursor (id, seq) VALUES (1, ?) ON CONFLICT (id) DO UPDATE SET \
             seq = CASE WHEN excluded.seq > webhook_land_cursor.seq THEN excluded.seq ELSE webhook_land_cursor.seq END",
            [seq],
        )?;
        Ok(())
    }

    fn delete_repo_hooks(&self, repo_id: &str) -> Result<()> {
        Ok(self.write(|c| {
            c.execute("DELETE FROM review_deliveries WHERE app_id IN (SELECT id FROM repo_webhooks WHERE repo_id = ?)", [repo_id])?;
            c.execute("DELETE FROM repo_webhooks WHERE repo_id = ?", [repo_id])?;
            c.execute("DELETE FROM commit_statuses WHERE repo_id = ?", [repo_id])?;
            Ok(())
        })?)
    }
}

/// An event on its way to webhooks: (repo id, kind, data).
type Pending = (String, &'static str, Value);

/// Webhook fan-out: a channel to one task, and each repo's hooks cached.
#[derive(Default)]
pub struct Webhooks {
    /// Whether any repo has a webhook (so emitting costs nothing otherwise).
    any: AtomicBool,
    loaded: AtomicBool,
    started: AtomicBool,
    tx: Mutex<Option<mpsc::UnboundedSender<Pending>>>,
    cache: Mutex<HashMap<String, Arc<Vec<HookRow>>>>,
}

fn new_secret() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("system randomness");
    format!("rlw_{}", hex::encode(b))
}

fn events_of(v: Option<&Value>) -> Result<Option<Vec<String>>> {
    let Some(v) = v.filter(|v| !v.is_null()) else { return Ok(None) };
    let mut events: Vec<String> = serde_json::from_value(v.clone()).map_err(|_| Error::Invalid("events is a list of event names".into()))?;
    if events.iter().any(|e| e == "*") {
        return Ok(Some(EVENTS.iter().map(|s| s.to_string()).collect()));
    }
    if let Some(bad) = events.iter().find(|e| !EVENTS.contains(&e.as_str())) {
        return Err(Error::Invalid(format!("unknown event {bad}; events are {} (or \"*\")", EVENTS.join(", "))));
    }
    if events.is_empty() {
        return Err(Error::Invalid("events can't be empty".into()));
    }
    events.sort();
    events.dedup();
    Ok(Some(events))
}

fn hook_json(h: &HookRow) -> Value {
    json!({
        "id": h.id,
        "url": h.url,
        "events": h.events,
        "active": h.active,
        "created_by": h.created_by,
        "created_at": h.created_at,
        "updated_at": h.updated_at,
    })
}

fn secret_of(v: Option<&Value>) -> Result<Option<String>> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if (16..=200).contains(&s.trim().len()) => Ok(Some(s.trim().to_string())),
        Some(_) => Err(Error::Invalid("secret is a string of 16-200 characters".into())),
    }
}

impl Code {
    // ── fan-out ──

    /// Pass an event on to the repo's webhooks (cheap when there are none).
    pub(crate) fn webhook_event(&self, repo_id: &str, kind: &'static str, data: &Value) {
        if !self.any_webhooks() || !EVENTS.contains(&kind) {
            return;
        }
        self.start_webhooks();
        if let Some(tx) = self.webhooks.tx.lock().as_ref() {
            let _ = tx.send((repo_id.to_string(), kind, data.clone()));
        }
    }

    /// Whether any repo has a webhook (read from the database once, then kept up to date).
    fn any_webhooks(&self) -> bool {
        if !self.webhooks.loaded.load(Ordering::Relaxed) {
            let any = self.db.any_webhooks().unwrap_or(true);
            self.webhooks.any.store(any, Ordering::Relaxed);
            self.webhooks.loaded.store(true, Ordering::Relaxed);
        }
        self.webhooks.any.load(Ordering::Relaxed)
    }

    /// Start the webhook task (and the landing forwarder); idempotent. Needs a Tokio runtime.
    pub fn start_webhooks(&self) {
        if tokio::runtime::Handle::try_current().is_err() || self.webhooks.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(code) = self.me.upgrade() else {
            self.webhooks.started.store(false, Ordering::SeqCst);
            return;
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<Pending>();
        *self.webhooks.tx.lock() = Some(tx);
        let weak = Arc::downgrade(&code);
        tokio::spawn(async move {
            while let Some((repo, kind, data)) = rx.recv().await {
                let Some(code) = weak.upgrade() else { return };
                if let Err(e) = code.fan_out(&repo, kind, data) {
                    tracing::warn!(repo = %repo, event = kind, error = %e, "queueing webhook deliveries");
                }
            }
        });
        // Landings come from the store's land log, not `emit`. The broadcast channel only wakes
        // the reader up; what gets delivered is whatever the log holds after the cursor.
        let mut landed = code.store.subscribe_all();
        let weak = Arc::downgrade(&code);
        tokio::spawn(async move {
            loop {
                let Some(code) = weak.upgrade() else { return };
                if let Err(e) = code.forward_landings() {
                    tracing::warn!(error = %e, "forwarding landings to webhooks");
                }
                drop(code);
                // Wake up on the next landing (or a lag, which just means "read the log"), or
                // every few seconds as a safety net.
                match tokio::time::timeout(LAND_POLL, landed.recv()).await {
                    Ok(Ok(l)) => {
                        if let Some(code) = weak.upgrade() {
                            code.remember_landing(&l);
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) | Err(_) => {}
                    Ok(Err(_)) => return,
                }
                // Coalesce whatever else is already waiting into the same read.
                while let Ok(l) = landed.try_recv() {
                    if let Some(code) = weak.upgrade() {
                        code.remember_landing(&l);
                    }
                }
            }
        });
    }

    /// Note which change a landing came from while it's fresh (the log only has changesets).
    fn remember_landing(&self, l: &code_store::Landed) {
        if let Some(c) = self.landing_changes.get(&(l.repo.clone(), l.source)) {
            self.landing_changes.put((l.repo.clone(), l.changeset), c);
        }
    }

    /// Queue `landed` deliveries for everything in the land log after the cursor, page by page,
    /// moving the cursor after each page (at-least-once: a crash mid-page re-queues that page).
    fn forward_landings(&self) -> Result<()> {
        let head = self.store.land_log_head()?;
        let Some(mut cursor) = self.db.land_cursor()? else {
            // First run: start at the present, never replay history from before webhooks existed.
            self.db.set_land_cursor(head)?;
            return Ok(());
        };
        if !self.any_webhooks() {
            if head > cursor {
                self.db.set_land_cursor(head)?;
            }
            return Ok(());
        }
        loop {
            let page = self.store.land_log(cursor, LAND_PAGE)?;
            let Some((last, _)) = page.last() else { return Ok(()) };
            let last = *last;
            for (seq, l) in &page {
                // Straight to delivery (not through the channel): the cursor only moves once
                // these rows exist.
                self.fan_out_landed(*seq, l)?;
            }
            self.db.set_land_cursor(last)?;
            cursor = last;
            if page.len() < LAND_PAGE {
                return Ok(());
            }
        }
    }

    fn fan_out_landed(&self, seq: i64, l: &code_store::Landed) -> Result<()> {
        let hooks = self.hooks_for(&l.repo)?;
        let targets: Vec<&HookRow> = hooks.iter().filter(|h| h.active && seq > h.since_seq && h.events.iter().any(|e| e == "landed")).collect();
        if targets.is_empty() {
            return Ok(());
        }
        let Some(r) = self.db.repo_by_id(&l.repo)? else { return Ok(()) };
        let data = self.landed_json(l);
        for h in targets {
            self.queue_hook_delivery(&r, h, "landed", data.clone())?;
        }
        Ok(())
    }

    fn hooks_for(&self, repo_id: &str) -> Result<Arc<Vec<HookRow>>> {
        if let Some(h) = self.webhooks.cache.lock().get(repo_id) {
            return Ok(h.clone());
        }
        let hooks = Arc::new(self.db.webhooks(repo_id)?);
        self.webhooks.cache.lock().insert(repo_id.to_string(), hooks.clone());
        Ok(hooks)
    }

    fn forget_hooks(&self, repo_id: &str) {
        self.webhooks.cache.lock().remove(repo_id);
        self.webhooks.loaded.store(false, Ordering::Relaxed);
    }

    fn fan_out(&self, repo_id: &str, kind: &str, data: Value) -> Result<()> {
        let hooks = self.hooks_for(repo_id)?;
        let targets: Vec<&HookRow> = hooks.iter().filter(|h| h.active && h.events.iter().any(|e| e == kind)).collect();
        if targets.is_empty() {
            return Ok(());
        }
        let Some(r) = self.db.repo_by_id(repo_id)? else { return Ok(()) };
        for h in targets {
            self.queue_hook_delivery(&r, h, kind, data.clone())?;
        }
        Ok(())
    }

    fn queue_hook_delivery(&self, r: &RepoRow, h: &HookRow, kind: &str, data: Value) -> Result<String> {
        let id = new_id("dl");
        let payload = json!({
            "event": kind,
            "delivery": id,
            "webhook": h.id,
            "repo": { "id": r.id, "full_name": r.full_name(), "default_branch": r.default_branch, "visibility": r.visibility },
            "data": data,
        });
        self.db.insert_delivery(&id, &h.id, kind, &payload.to_string())?;
        self.kick_deliveries();
        Ok(id)
    }

    // ── management ──

    fn hook_of(&self, r: &RepoRow, id: &str) -> Result<HookRow> {
        self.db.webhook(id)?.filter(|h| h.repo_id == r.id).ok_or_else(|| Error::NotFound(format!("webhook {id}")))
    }

    pub fn list_webhooks(&self, r: &RepoRow) -> Result<Vec<Value>> {
        Ok(self.db.webhooks(&r.id)?.iter().map(hook_json).collect())
    }

    /// `POST /repos/{o}/{r}/webhooks`: the webhook, plus its secret when roll generated it.
    pub fn create_webhook(&self, caller: &Caller, r: &RepoRow, input: &Value) -> Result<Value> {
        self.check_write(caller, r)?;
        let url = check_url(input.get("url").and_then(Value::as_str).ok_or_else(|| Error::Invalid("url is required".into()))?)?;
        if self.db.webhooks(&r.id)?.len() >= MAX_HOOKS_PER_REPO {
            return Err(Error::Unprocessable(format!("at most {MAX_HOOKS_PER_REPO} webhooks per repo")));
        }
        let given = secret_of(input.get("secret"))?;
        let t = now();
        let h = HookRow {
            id: new_id("wh"),
            repo_id: r.id.clone(),
            url: url.to_string(),
            events: events_of(input.get("events"))?.unwrap_or_else(|| vec!["landed".into()]),
            secret: given.clone().unwrap_or_else(new_secret),
            active: match input.get("active") {
                None | Some(Value::Null) => true,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err(Error::Invalid("active must be true or false".into())),
            },
            created_by: caller.identity().unwrap_or_default(),
            created_at: t,
            updated_at: t,
            since_seq: self.store.land_log_head()?,
        };
        self.db.save_webhook(&h)?;
        self.webhooks.any.store(true, Ordering::Relaxed);
        self.forget_hooks(&r.id);
        self.db.audit(&r.id, &caller.identity().unwrap_or_default(), "webhook_create", &json!({ "webhook": h.id, "url": h.url }))?;
        self.start_webhooks();
        let ping = self.queue_hook_delivery(r, &h, "ping", json!({ "zen": "keep the code rolling", "webhook": hook_json(&h) }))?;
        let mut v = json!({ "webhook": hook_json(&h), "ping": ping });
        if given.is_none() {
            v["secret"] = json!(h.secret);
        }
        Ok(v)
    }

    pub fn get_webhook(&self, r: &RepoRow, id: &str) -> Result<Value> {
        Ok(hook_json(&self.hook_of(r, id)?))
    }

    pub fn update_webhook(&self, caller: &Caller, r: &RepoRow, id: &str, patch: &Value) -> Result<Value> {
        self.check_write(caller, r)?;
        let mut h = self.hook_of(r, id)?;
        if let Some(u) = patch.get("url").and_then(Value::as_str) {
            h.url = check_url(u)?.to_string();
        }
        if let Some(e) = events_of(patch.get("events"))? {
            h.events = e;
        }
        if let Some(s) = secret_of(patch.get("secret"))? {
            h.secret = s;
        }
        match patch.get("active") {
            None | Some(Value::Null) => {}
            Some(Value::Bool(b)) => h.active = *b,
            Some(_) => return Err(Error::Invalid("active must be true or false".into())),
        }
        h.updated_at = now();
        self.db.save_webhook(&h)?;
        self.forget_hooks(&r.id);
        self.db.audit(&r.id, &caller.identity().unwrap_or_default(), "webhook_update", &json!({ "webhook": h.id }))?;
        Ok(hook_json(&h))
    }

    pub fn delete_webhook(&self, caller: &Caller, r: &RepoRow, id: &str) -> Result<()> {
        self.check_write(caller, r)?;
        let h = self.hook_of(r, id)?;
        self.db.delete_webhook(&h.id)?;
        self.forget_hooks(&r.id);
        self.db.audit(&r.id, &caller.identity().unwrap_or_default(), "webhook_delete", &json!({ "webhook": h.id }))?;
        Ok(())
    }

    pub fn ping_webhook(&self, caller: &Caller, r: &RepoRow, id: &str) -> Result<Value> {
        self.check_write(caller, r)?;
        let h = self.hook_of(r, id)?;
        let d = self.queue_hook_delivery(r, &h, "ping", json!({ "zen": "keep the code rolling", "webhook": hook_json(&h) }))?;
        self.delivery_view(&d)
    }

    pub fn webhook_deliveries(&self, caller: &Caller, r: &RepoRow, id: &str) -> Result<Vec<Value>> {
        self.check_write(caller, r)?;
        let h = self.hook_of(r, id)?;
        Ok(self.db.deliveries(&h.id, 100)?.iter().map(crate::review::apps::delivery_json).collect())
    }

    pub fn redeliver_webhook(&self, caller: &Caller, r: &RepoRow, id: &str, delivery: &str) -> Result<Value> {
        self.check_write(caller, r)?;
        let h = self.hook_of(r, id)?;
        let d = self.db.delivery(delivery)?.filter(|d| d.app_id == h.id).ok_or_else(|| Error::NotFound(format!("delivery {delivery}")))?;
        self.db.redeliver(&d.id)?;
        self.kick_deliveries();
        self.delivery_view(&d.id)
    }

    fn delivery_view(&self, id: &str) -> Result<Value> {
        Ok(crate::review::apps::delivery_json(&self.db.delivery(id)?.ok_or_else(|| Error::NotFound(format!("delivery {id}")))?))
    }

    pub(crate) fn forget_repo_hooks(&self, repo_id: &str) -> Result<()> {
        self.db.delete_repo_hooks(repo_id)?;
        self.forget_hooks(repo_id);
        Ok(())
    }

    // ── commit statuses ──

    fn status_target(&self, r: &RepoRow, rev: &str) -> Result<ChangesetId> {
        self.store.resolve(&r.id, rev.trim())?.ok_or_else(|| Error::NotFound(format!("commit {rev}")))
    }

    /// `POST /repos/{o}/{r}/statuses/{rev}`: record a CI status for a commit and turn it into a
    /// check on the open changes whose head it is.
    pub fn post_status(&self, caller: &Caller, r: &RepoRow, rev: &str, input: &Value) -> Result<Value> {
        self.check_write(caller, r)?;
        let s = |k: &str| input.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
        let state = s("state").filter(|v| STATES.contains(v)).ok_or_else(|| Error::Invalid(format!("state is one of {}", STATES.join(", "))))?;
        let context = s("context").unwrap_or("default");
        if context.len() > 100 {
            return Err(Error::Invalid("context is at most 100 characters".into()));
        }
        let target_url = match s("target_url") {
            Some(u) => {
                let parsed = url::Url::parse(u).map_err(|e| Error::Invalid(format!("target_url: {e}")))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(Error::Invalid("target_url must be http(s)".into()));
                }
                Some(u.to_string())
            }
            None => None,
        };
        let description: Option<String> = s("description").map(|d| d.chars().take(1000).collect());
        let cs = self.status_target(r, rev)?;
        let id = cs_id(&cs);
        let t = now();
        let created_at = self.db.statuses(&r.id, &id)?.into_iter().find(|x| x.context == context).map(|x| x.created_at).unwrap_or(t);
        let status = Status {
            context: context.to_string(),
            state: state.to_string(),
            target_url: target_url.clone(),
            description: description.clone(),
            creator: caller.identity().unwrap_or_default(),
            created_at,
            updated_at: t,
        };
        self.db.upsert_status(&r.id, &id, &status)?;
        let check_status = match state {
            "pending" => "running",
            "success" => "success",
            _ => "failure",
        };
        let summary = match (state, &description) {
            ("error", Some(d)) => Some(format!("error: {d}")),
            ("error", None) => Some("error".to_string()),
            (_, d) => d.clone(),
        };
        let mut changes = Vec::new();
        for cid in self.db.changes_at_head(&r.id, &id)? {
            let c: Change = self.change_or_404(&cid)?;
            if c.state == ChangeState::Landed || c.state == ChangeState::Closed {
                continue;
            }
            let body = json!({ "name": context, "status": check_status, "source": "external", "url": target_url, "summary": summary, "head": id });
            match self.report_check(&c, &body) {
                Ok(_) => changes.push(c.id),
                Err(Error::Conflict(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let git = self.store.git_of(&r.id, &cs)?.map(|(c, _)| c.hex());
        let out = json!({ "changeset": id, "git_commit": git, "status": status, "changes": changes });
        self.emit(&r.id, "check", json!({ "changeset": id, "status": status }));
        // On the default branch, a failure opens a broken-main item and a success closes it.
        if state != "pending"
            && let Some(code) = self.me.upgrade()
            && tokio::runtime::Handle::try_current().is_ok()
        {
            let (r, context, state) = (r.clone(), context.to_string(), state.to_string());
            let output = [description.clone(), target_url.clone()].into_iter().flatten().collect::<Vec<_>>().join("\n");
            let output = (!output.is_empty()).then_some(output);
            let lock = code.main_status_lock(&r.id);
            tokio::spawn(async move {
                let _held = lock.lock().await;
                if let Err(e) = code.main_status(&r, &cs, &context, &state, output).await {
                    tracing::warn!(repo = %r.full_name(), context = %context, error = %e, "a status on the default branch");
                }
            });
        }
        Ok(out)
    }

    /// `GET /repos/{o}/{r}/statuses/{rev}`: the statuses of a commit and their combined state.
    pub fn get_statuses(&self, r: &RepoRow, rev: &str) -> Result<Value> {
        let cs = self.status_target(r, rev)?;
        let id = cs_id(&cs);
        let statuses = self.db.statuses(&r.id, &id)?;
        let state = if statuses.is_empty() {
            "pending"
        } else if statuses.iter().any(|s| s.state == "failure" || s.state == "error") {
            "failure"
        } else if statuses.iter().any(|s| s.state == "pending") {
            "pending"
        } else {
            "success"
        };
        let git = self.store.git_of(&r.id, &cs)?.map(|(c, _)| c.hex());
        Ok(json!({ "changeset": id, "git_commit": git, "state": state, "total_count": statuses.len(), "statuses": statuses }))
    }
}
