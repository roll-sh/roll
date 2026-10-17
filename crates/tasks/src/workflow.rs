//! Durable workflows over HTTP.
//!
//! An *app* (one deployment of the SDK) serves functions at a URL. Events start runs. A run is
//! executed by calling the app repeatedly; each call gets the memoised results of the steps so
//! far, re-runs the function, and reports what it reached next:
//!
//! ```text
//! request:  { function_id, run_id, attempt, event, events, steps: { <id>: {data} | {error} } }
//! response: { status: "done", output }
//!         | { status: "error", error, retriable }
//!         | { status: "steps", steps: [op…] }
//! op:       { op: "run", id, name?, data } | { op: "run", id, error, retriable? }
//!         | { op: "sleep", id, until | duration_ms }
//!         | { op: "wait_for_event", id, event, match?, if?, timeout_ms }
//!         | { op: "invoke", id, function, data, timeout_ms? }
//!         | { op: "send_event", id, events: [...] }
//! ```
//!
//! Each call is a message in the function's queue, so retries, leases, concurrency, throttling
//! and failover come from the delivery engine. One call per run is queued or in flight at a time.

use crate::dispatch::{Done, json_body};
use crate::engine::{KIND_APP, NewMessage, QueueUpdate, Result, TaskError, Tasks, invalid};
use crate::expr::{self, key_string};
use crate::ids::new_id;
use crate::model::*;
use crate::store::{self, Expect, RunQuery};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::warn;

pub const EVENT_FINISHED: &str = "roll/function.finished";
pub const EVENT_FAILED: &str = "roll/function.failed";
pub const EVENT_CANCELED: &str = "roll/function.canceled";
pub const EVENT_INVOKED: &str = "roll/function.invoked";
const DAY: i64 = 86_400_000;

#[derive(Default)]
pub struct Registry {
    pub apps: BTreeMap<String, App>,
    functions: HashMap<String, (String, FunctionDef)>,
    triggers: Vec<(String, String, Option<String>)>,
}

impl Registry {
    pub fn build(apps: Vec<App>) -> Self {
        let mut r = Registry::default();
        for app in apps {
            for f in &app.functions {
                r.functions.insert(f.id.clone(), (app.id.clone(), f.clone()));
                for t in &f.triggers {
                    if let Some(ev) = &t.event {
                        r.triggers.push((ev.clone(), f.id.clone(), t.if_.clone()));
                    }
                }
            }
            r.apps.insert(app.id.clone(), app);
        }
        r
    }

    pub fn function(&self, id: &str) -> Option<(&App, &FunctionDef)> {
        let (app, f) = self.functions.get(id)?;
        Some((self.apps.get(app)?, f))
    }

    pub fn functions(&self) -> impl Iterator<Item = (&App, &FunctionDef)> {
        self.apps.values().flat_map(|a| a.functions.iter().map(move |f| (a, f)))
    }

    /// Functions an event starts: exact names, or `prefix/*` patterns.
    fn matching(&self, name: &str) -> Vec<(String, Option<String>)> {
        self.triggers
            .iter()
            .filter(|(pat, _, _)| pat == name || pat.strip_suffix('*').is_some_and(|p| name.starts_with(p)) || pat == "*")
            .map(|(_, f, cond)| (f.clone(), cond.clone()))
            .collect()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EventIn {
    pub name: String,
    #[serde(default)]
    pub data: Value,
    /// Deduplication id: a repeat within 24h is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<Ms>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SyncRequest {
    pub url: String,
    #[serde(default)]
    pub id: Option<String>,
    /// Without functions, they're fetched from `GET url`.
    #[serde(default)]
    pub functions: Option<Vec<FunctionDef>>,
    #[serde(default)]
    pub sdk: Option<String>,
}

#[derive(Deserialize)]
struct Introspection {
    #[serde(alias = "app_id")]
    id: String,
    functions: Vec<FunctionDef>,
    #[serde(default)]
    sdk: Option<String>,
}

fn fn_queue(id: &str) -> String {
    format!("fn:{id}")
}

fn eval_key(src: &str, event: &Value) -> Result<String> {
    let v = expr::eval_str(src, &json!({ "event": event })).map_err(|e| TaskError::Invalid(format!("key {src:?}: {e}")))?;
    Ok(key_string(&v))
}

fn validate_function(f: &FunctionDef) -> Result<()> {
    if f.id.is_empty() || f.id.len() > 128 || !f.id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return invalid(format!("function ids use letters, digits, - _ . (max 128): {:?}", f.id));
    }
    let check = |src: &Option<String>, what: &str| -> Result<()> {
        if let Some(s) = src {
            expr::parse(s).map_err(|e| TaskError::Invalid(format!("{}: {what} {s:?}: {e}", f.id)))?;
        }
        Ok(())
    };
    if let Some(b) = &f.backoff {
        b.validate().map_err(|e| TaskError::Invalid(format!("{}: {e}", f.id)))?;
    }
    for t in &f.triggers {
        match (&t.event, &t.cron) {
            (Some(_), None) => check(&t.if_, "trigger if")?,
            (None, Some(c)) => {
                crate::schedule::parse(c, t.tz.as_deref()).map_err(|e| TaskError::Invalid(format!("{}: {e}", f.id)))?;
            }
            _ => return invalid(format!("{}: each trigger needs an event or a cron", f.id)),
        }
    }
    for c in &f.concurrency {
        if c.limit == 0 {
            return invalid(format!("{}: concurrency limit must be positive", f.id));
        }
        check(&c.key, "concurrency key")?;
    }
    if f.concurrency.iter().filter(|c| c.key.is_none()).count() > 1 {
        return invalid(format!("{}: at most one concurrency limit without a key", f.id));
    }
    for r in [&f.rate_limit, &f.throttle].into_iter().flatten() {
        if r.limit == 0 || r.period_ms <= 0 {
            return invalid(format!("{}: limits need a positive limit and period", f.id));
        }
        check(&r.key, "key")?;
    }
    if let Some(d) = &f.debounce {
        if d.period_ms <= 0 {
            return invalid(format!("{}: debounce period must be positive", f.id));
        }
        check(&d.key, "debounce key")?;
    }
    if let Some(b) = &f.batch {
        if b.max_size == 0 || b.max_size > 1000 || b.timeout_ms <= 0 {
            return invalid(format!("{}: batch needs max_size 1–1000 and a positive timeout", f.id));
        }
        check(&b.key, "batch key")?;
    }
    check(&f.idempotency, "idempotency key")?;
    check(&f.priority, "priority")?;
    if let Some(s) = &f.singleton {
        check(&s.key, "singleton key")?;
    }
    for c in &f.cancel_on {
        check(&c.if_, "cancel_on if")?;
        if let Some(m) = &c.match_ {
            expr::parse_match_path(m).map_err(|e| TaskError::Invalid(format!("{}: cancel_on match: {e}", f.id)))?;
        }
    }
    Ok(())
}

impl Tasks {
    // --- apps and functions ---

    pub async fn apps(&self) -> Vec<App> {
        self.cache.read().unwrap().registry.apps.values().cloned().collect()
    }

    pub async fn sync_app(&self, req: SyncRequest) -> Result<App> {
        crate::engine::check_url(&req.url, self.config.block_private_urls)?;
        let (id, functions, sdk) = match req.functions {
            Some(f) => (req.id.ok_or_else(|| TaskError::Invalid("give the app id".into()))?, f, req.sdk),
            None => {
                let body = b"";
                let mut r = self.http.get(&req.url).timeout(std::time::Duration::from_secs(15)).header("roll-sync", "1");
                for (k, v) in self.sign_headers(&new_id("sync"), body) {
                    r = r.header(k, v);
                }
                let resp = r.send().await.map_err(|e| TaskError::Invalid(format!("couldn't reach {}: {e}", req.url)))?;
                if !resp.status().is_success() {
                    return invalid(format!("{} answered {}", req.url, resp.status()));
                }
                let intro: Introspection =
                    resp.json().await.map_err(|e| TaskError::Invalid(format!("{} didn't describe its functions: {e}", req.url)))?;
                (req.id.unwrap_or(intro.id), intro.functions, intro.sdk.or(req.sdk))
            }
        };
        if id.is_empty() || id.len() > 128 || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            return invalid(format!("app ids use letters, digits, - _ . : {id:?}"));
        }
        let mut seen = std::collections::HashSet::new();
        for f in &functions {
            validate_function(f)?;
            if !seen.insert(&f.id) {
                return invalid(format!("function {} is defined twice", f.id));
            }
        }
        let others: Vec<App> = store::list_docs(&*self.store, KIND_APP).await?;
        for other in others.iter().filter(|a| a.id != id) {
            if let Some(f) = other.functions.iter().find(|f| seen.contains(&f.id)) {
                return Err(TaskError::Conflict(format!("function {} belongs to app {}", f.id, other.id)));
            }
        }
        let now = now_ms();
        let previous = others.into_iter().find(|a| a.id == id);
        let app = App {
            id: id.clone(),
            url: req.url,
            functions,
            sdk,
            synced_at: now,
            created_at: previous.as_ref().map(|a| a.created_at).unwrap_or(now),
        };
        store::put_doc(&*self.store, KIND_APP, &id, &app, Expect::Any).await?;
        for f in &app.functions {
            self.apply_function_queue(f).await?;
        }
        self.refresh().await?;
        // Cron triggers become schedules; drop the ones that went away.
        let mut wanted = std::collections::HashSet::new();
        for f in &app.functions {
            for (i, t) in f.triggers.iter().enumerate() {
                if let Some(cron) = &t.cron {
                    let sid = format!("fn-{}-{i}", f.id);
                    self.upsert_schedule(&sid, cron, t.tz.clone(), ScheduleTarget::Function { function_id: f.id.clone() }).await?;
                    if f.paused {
                        self.pause_schedule(&sid, true).await?;
                    }
                    wanted.insert(sid);
                }
            }
        }
        if let Some(prev) = previous {
            for f in prev.functions {
                for i in 0..f.triggers.len() {
                    let sid = format!("fn-{}-{i}", f.id);
                    if !wanted.contains(&sid) {
                        let _ = self.delete_schedule(&sid).await;
                    }
                }
            }
        }
        Ok(app)
    }

    async fn apply_function_queue(&self, f: &FunctionDef) -> Result<()> {
        let parallelism = f.concurrency.iter().find(|c| c.key.is_none()).map(|c| c.limit).unwrap_or(0);
        let rate = f.throttle.as_ref().filter(|t| t.key.is_none()).map(|t| crate::engine::RateSpec(Rate { limit: t.limit, period_ms: t.period_ms }));
        self.put_queue(
            &fn_queue(&f.id),
            QueueUpdate {
                parallelism: Some(parallelism),
                rate: Some(rate),
                paused: Some(f.paused),
                retries: Some(Some(f.retries.unwrap_or(3).min(self.config.max_retries))),
                internal: true,
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    pub async fn delete_app(&self, id: &str) -> Result<()> {
        let Some(app) = store::get_doc::<App>(&*self.store, KIND_APP, id).await? else {
            return Err(TaskError::NotFound(format!("app {id}")));
        };
        for f in &app.value.functions {
            for i in 0..f.triggers.len() {
                let _ = self.delete_schedule(&format!("fn-{}-{i}", f.id)).await;
            }
            // Runs in progress are canceled with their queue.
            let runs = self.store.run_list(&RunQuery { function_id: Some(f.id.clone()), limit: 1000, ..Default::default() }).await?;
            for r in runs.items.into_iter().filter(|r| !r.status.is_final()) {
                let _ = self.cancel_run(&r.id, "app removed").await;
            }
            let _ = self.delete_queue(&fn_queue(&f.id), true).await;
        }
        self.store.doc_delete(KIND_APP, id).await?;
        self.refresh().await?;
        Ok(())
    }

    pub async fn set_function_paused(&self, function_id: &str, paused: bool) -> Result<FunctionDef> {
        let app_id = self
            .cache
            .read()
            .unwrap()
            .registry
            .function(function_id)
            .map(|(a, _)| a.id.clone())
            .ok_or_else(|| TaskError::NotFound(format!("function {function_id}")))?;
        for _ in 0..10 {
            let v = store::get_doc::<App>(&*self.store, KIND_APP, &app_id).await?.ok_or_else(|| TaskError::NotFound(format!("app {app_id}")))?;
            let mut app = v.value;
            let Some(f) = app.functions.iter_mut().find(|f| f.id == function_id) else {
                return Err(TaskError::NotFound(format!("function {function_id}")));
            };
            f.paused = paused;
            let f = f.clone();
            if store::put_doc(&*self.store, KIND_APP, &app_id, &app, Expect::Version(v.version)).await? {
                self.apply_function_queue(&f).await?;
                for (i, t) in f.triggers.iter().enumerate() {
                    if t.cron.is_some() {
                        let _ = self.pause_schedule(&format!("fn-{}-{i}", f.id), paused).await;
                    }
                }
                self.refresh().await?;
                return Ok(f);
            }
        }
        Err(TaskError::Conflict(format!("function {function_id} is changing; try again")))
    }

    fn function_def(&self, id: &str) -> Option<(App, FunctionDef)> {
        self.cache.read().unwrap().registry.function(id).map(|(a, f)| (a.clone(), f.clone()))
    }

    // --- events ---

    pub async fn send_events(&self, events: Vec<EventIn>) -> Result<Vec<String>> {
        if events.len() > 1000 {
            return invalid("at most 1000 events per call");
        }
        let mut ids = Vec::with_capacity(events.len());
        for e in events {
            ids.push(self.send_event(e).await?);
        }
        Ok(ids)
    }

    async fn store_event(&self, e: EventIn) -> Result<(Event, bool)> {
        if e.name.is_empty() || e.name.len() > 256 {
            return invalid("event names are 1–256 characters");
        }
        let size = serde_json::to_vec(&e.data).map(|b| b.len()).unwrap_or(0);
        if size > self.config.max_body_bytes {
            return Err(TaskError::TooLarge(format!("event data is {size} bytes; the limit is {}", self.config.max_body_bytes)));
        }
        let now = now_ms();
        let id = new_id("evt");
        if let Some(d) = &e.id {
            let key = format!("evdedup:{d}");
            if !self.store.kv_put(&key, &json!(id), Expect::Absent, Some(now + DAY)).await? {
                let existing = self.store.kv_get(&key).await?.and_then(|v| v.value.as_str().map(String::from)).unwrap_or_default();
                if let Some(ev) = self.store.event_get(&existing).await? {
                    return Ok((ev, false));
                }
                return Ok((Event { id: existing, name: e.name, dedup_id: e.id, data: e.data, user: e.user, ts: now, runs: vec![], expires_at: None }, false));
            }
        }
        let ev = Event {
            id,
            name: e.name,
            dedup_id: e.id,
            data: e.data,
            user: e.user,
            ts: e.ts.unwrap_or(now),
            runs: vec![],
            expires_at: Some(now + self.config.retention_ms),
        };
        self.store.event_put(&ev).await?;
        self.count(|u| {
            u.events += 1;
            u.bytes_in += size as u64;
        });
        Ok((ev, true))
    }

    async fn send_event(&self, e: EventIn) -> Result<String> {
        let (mut ev, fresh) = self.store_event(e).await?;
        if !fresh {
            return Ok(ev.id);
        }
        let json = ev.json();
        self.resolve_waits(&ev, &json).await?;
        let matching = self.cache.read().unwrap().registry.matching(&ev.name);
        let mut runs = Vec::new();
        for (fid, cond) in matching {
            if let Some(cond) = cond {
                match expr::parse(&cond) {
                    Ok(c) if c.test(&json!({ "event": json })) => {}
                    Ok(_) => continue,
                    Err(e) => {
                        warn!(function = fid, error = e, "bad trigger condition");
                        continue;
                    }
                }
            }
            // Functions that handle failures don't trigger on their own failures.
            if ev.name == EVENT_FAILED && json["data"]["function_id"] == json!(fid) {
                continue;
            }
            match self.trigger(&fid, &ev, &json).await {
                Ok(Some(run)) => runs.push(run),
                Ok(None) => {}
                Err(e) => warn!(function = fid, error = %e, "trigger failed"),
            }
        }
        if !runs.is_empty() {
            ev.runs = runs;
            self.store.event_put(&ev).await?;
        }
        Ok(ev.id)
    }

    pub async fn event(&self, id: &str) -> Result<Event> {
        self.store.event_get(id).await?.ok_or_else(|| TaskError::NotFound(format!("event {id}")))
    }

    /// Run a function directly (API, schedules, `step.invoke` from outside).
    pub async fn invoke_function(&self, function_id: &str, event: EventIn) -> Result<String> {
        let (app, def) = self.function_def(function_id).ok_or_else(|| TaskError::NotFound(format!("function {function_id}")))?;
        let (mut ev, fresh) = self.store_event(event).await?;
        if !fresh && let Some(run) = ev.runs.first() {
            return Ok(run.clone());
        }
        let run = self.start_run(&app, &def, vec![ev.json()], StartOpts::default()).await?;
        ev.runs = vec![run.id.clone()];
        self.store.event_put(&ev).await?;
        Ok(run.id)
    }

    // --- triggering ---

    async fn trigger(&self, fid: &str, ev: &Event, json: &Value) -> Result<Option<String>> {
        let Some((app, def)) = self.function_def(fid) else { return Ok(None) };
        if def.paused {
            return Ok(None);
        }
        if let Some(rl) = &def.rate_limit {
            let key = match &rl.key {
                Some(k) => eval_key(k, json)?,
                None => String::new(),
            };
            let wait = crate::flow::take(self, &format!("rl:{fid}:{key}"), Rate { limit: rl.limit, period_ms: rl.period_ms }).await?;
            if wait > 0 {
                return Ok(None);
            }
        }
        if let Some(src) = &def.idempotency {
            let key = format!("idem:{fid}:{}", eval_key(src, json)?);
            if !self.store.kv_put(&key, &json!(ev.id), Expect::Absent, Some(now_ms() + DAY)).await? {
                return Ok(None);
            }
        }
        if let Some(d) = &def.debounce {
            return self.debounce(&app, &def, d, json).await.map(Some);
        }
        if let Some(b) = &def.batch {
            return self.batch(&app, &def, b, ev, json).await;
        }
        let run = self.start_run(&app, &def, vec![json.clone()], StartOpts::default()).await?;
        Ok(Some(run.id))
    }

    async fn debounce(&self, app: &App, def: &FunctionDef, d: &Debounce, json: &Value) -> Result<String> {
        let key = format!(
            "debounce:{}:{}",
            def.id,
            match &d.key {
                Some(k) => eval_key(k, json)?,
                None => String::new(),
            }
        );
        let ttl = d.period_ms + d.timeout_ms.unwrap_or(d.period_ms * 10) + 60_000;
        for _ in 0..50 {
            let now = now_ms();
            match self.store.kv_get(&key).await? {
                Some(v) if v.value.get("pending").is_some() => {
                    if v.value["pending"].as_i64().unwrap_or(0) < now - 10_000 {
                        self.store.kv_delete(&key, Expect::Version(v.version)).await?;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Some(v) => {
                    let run_id = v.value["run_id"].as_str().unwrap_or_default().to_string();
                    let msg_id = v.value["msg_id"].as_str().unwrap_or_default().to_string();
                    let first_at = v.value["first_at"].as_i64().unwrap_or(now);
                    if self.redebounce(&run_id, &msg_id, json, now + d.period_ms, first_at + d.timeout_ms.unwrap_or(i64::MAX / 4)).await? {
                        return Ok(run_id);
                    }
                    self.store.kv_delete(&key, Expect::Version(v.version)).await?;
                }
                None => {
                    if !self.store.kv_put(&key, &json!({ "pending": now }), Expect::Absent, Some(now + ttl)).await? {
                        continue;
                    }
                    let v = self.store.kv_get(&key).await?.map(|v| v.version).unwrap_or(0);
                    let run = self.start_run(app, def, vec![json.clone()], StartOpts { delay_ms: d.period_ms, ..Default::default() }).await?;
                    self.store
                        .kv_put(&key, &json!({ "run_id": run.id, "msg_id": run.invocation, "first_at": now }), Expect::Version(v), Some(now + ttl))
                        .await?;
                    return Ok(run.id);
                }
            }
        }
        Err(TaskError::Conflict("debounce is contended; try again".into()))
    }

    /// Replace a debounced run's event and push its start out. False if it already started.
    async fn redebounce(&self, run_id: &str, msg_id: &str, json: &Value, due: Ms, latest: Ms) -> Result<bool> {
        for _ in 0..10 {
            let Some(mut m) = self.store.msg_get(msg_id).await? else { return Ok(false) };
            if m.state != MsgState::Pending || m.attempts > 0 {
                return Ok(false);
            }
            let Some(mut run) = self.store.run_get(run_id).await? else { return Ok(false) };
            if run.status != RunStatus::Queued {
                return Ok(false);
            }
            let rv = run.version;
            run.events = vec![json.clone()];
            if !self.store.run_put(&run, Expect::Version(rv)).await? {
                continue;
            }
            let mv = m.version;
            m.due_at = due.min(latest);
            if self.store.msg_update(&m, mv).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn batch(&self, app: &App, def: &FunctionDef, b: &Batch, ev: &Event, json: &Value) -> Result<Option<String>> {
        let key = format!(
            "batch:{}:{}",
            def.id,
            match &b.key {
                Some(k) => eval_key(k, json)?,
                None => String::new(),
            }
        );
        if b.max_size == 1 {
            return Ok(Some(self.start_run(app, def, vec![json.clone()], StartOpts::default()).await?.id));
        }
        for _ in 0..50 {
            let now = now_ms();
            match self.store.kv_get(&key).await? {
                None => {
                    let batch_id = crate::ids::ulid();
                    let doc = json!({ "id": batch_id, "events": [ev.id], "first_at": now });
                    if self.store.kv_put(&key, &doc, Expect::Absent, Some(now + b.timeout_ms + DAY)).await? {
                        self.enqueue(
                            NewMessage::internal(Target::Flush { function_id: def.id.clone(), key: format!("{key}#{batch_id}") }, now + b.timeout_ms),
                            b"",
                        )
                        .await?;
                        return Ok(None);
                    }
                }
                Some(v) => {
                    let mut doc = v.value.clone();
                    let mut ids: Vec<String> = serde_json::from_value(doc["events"].clone()).unwrap_or_default();
                    ids.push(ev.id.clone());
                    if ids.len() >= b.max_size as usize {
                        if self.store.kv_delete(&key, Expect::Version(v.version)).await? {
                            let events = self.load_events(&ids).await?;
                            return Ok(Some(self.start_run(app, def, events, StartOpts::default()).await?.id));
                        }
                    } else {
                        doc["events"] = json!(ids);
                        if self.store.kv_put(&key, &doc, Expect::Version(v.version), Some(now + b.timeout_ms + DAY)).await? {
                            return Ok(None);
                        }
                    }
                }
            }
        }
        Err(TaskError::Conflict("batch is contended; try again".into()))
    }

    async fn load_events(&self, ids: &[String]) -> Result<Vec<Value>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(e) = self.store.event_get(id).await? {
                out.push(e.json());
            }
        }
        Ok(out)
    }

    // --- runs ---

    async fn start_run(&self, app: &App, def: &FunctionDef, events: Vec<Value>, opts: StartOpts) -> Result<Run> {
        let now = now_ms();
        let first = events.first().cloned().unwrap_or(Value::Null);
        let mut run = Run {
            id: opts.run_id.unwrap_or_else(|| new_id("run")),
            function_id: def.id.clone(),
            app_id: app.id.clone(),
            status: RunStatus::Queued,
            events,
            output: None,
            error: None,
            created_at: now,
            started_at: None,
            ended_at: None,
            invocation: None,
            dirty: false,
            pending: vec![],
            step_attempts: BTreeMap::new(),
            parent: opts.parent,
            keys: BTreeMap::new(),
            invocations: 0,
            version: 0,
            expires_at: None,
        };

        if let Some(s) = &def.singleton {
            let key = format!(
                "singleton:{}:{}",
                def.id,
                match &s.key {
                    Some(k) => eval_key(k, &first)?,
                    None => String::new(),
                }
            );
            for _ in 0..20 {
                match self.store.kv_get(&key).await? {
                    Some(v) => {
                        let holder = v.value.as_str().unwrap_or_default().to_string();
                        let alive = self.store.run_get(&holder).await?.is_some_and(|r| !r.status.is_final());
                        if alive && s.mode == SingletonMode::Skip {
                            run.status = RunStatus::Canceled;
                            run.error = Some(json!({ "name": "Skipped", "message": "a run is already in progress" }));
                            run.ended_at = Some(now);
                            run.expires_at = Some(now + self.config.retention_ms);
                            self.store.run_put(&run, Expect::Absent).await?;
                            return Ok(run);
                        }
                        if alive {
                            let _ = self.cancel_run(&holder, "replaced by a newer run").await;
                        }
                        if self.store.kv_put(&key, &json!(run.id), Expect::Version(v.version), None).await? {
                            break;
                        }
                    }
                    None => {
                        if self.store.kv_put(&key, &json!(run.id), Expect::Absent, None).await? {
                            break;
                        }
                    }
                }
            }
            run.keys.insert("singleton".into(), key);
        }

        let mut flows = Vec::new();
        for c in def.concurrency.iter().filter(|c| c.key.is_some()) {
            let k = eval_key(c.key.as_deref().unwrap(), &first)?;
            flows.push(Flow { key: format!("fn:{}:c:{}:{k}", def.id, c.key.as_deref().unwrap()), parallelism: Some(c.limit), rate: None });
        }
        if let Some(t) = def.throttle.as_ref().filter(|t| t.key.is_some()) {
            let k = eval_key(t.key.as_deref().unwrap(), &first)?;
            flows.push(Flow { key: format!("fn:{}:t:{k}", def.id), parallelism: None, rate: Some(Rate { limit: t.limit, period_ms: t.period_ms }) });
        }
        let mut due = now + opts.delay_ms;
        if let Some(p) = &def.priority {
            let v = expr::eval_str(p, &json!({ "event": first })).unwrap_or(Value::Null);
            due -= (v.as_f64().unwrap_or(0.0) as i64).clamp(-600, 600) * 1000;
        }

        // The invocation exists before the run: a crash in between leaves an orphan message
        // (dropped when it finds no run), never a run nobody will call.
        let inv = self.new_invocation(&run, def, due, flows.clone()).await?;
        run.invocation = Some(inv.id.clone());
        run.keys.insert("flows".into(), serde_json::to_string(&flows).unwrap_or_default());
        if !self.store.run_put(&run, Expect::Absent).await? {
            let _ = self.cancel(&inv.id, "duplicate run").await;
            return self.store.run_get(&run.id).await?.ok_or_else(|| TaskError::Conflict("run vanished".into()));
        }
        run.version = 1;
        self.count(|u| u.runs += 1);

        for (i, c) in def.cancel_on.iter().enumerate() {
            let key = c.match_.as_ref().and_then(|m| match_key(m, &first));
            let w = Wait {
                id: format!("{}:cancel:{i}", run.id),
                event: c.event.clone(),
                key: key.clone(),
                kind: WaitKind::Cancel,
                run_id: run.id.clone(),
                step_id: None,
                if_: c.if_.clone(),
                origin: first.clone(),
                expires_at: now + c.timeout_ms.unwrap_or(366 * DAY),
            };
            if let Some(m) = &c.match_ {
                self.add_wait_path(&c.event, m).await?;
            }
            self.store.wait_put(&w).await?;
        }
        if let Some(t) = &def.timeouts {
            if let Some(ms) = t.start_ms {
                self.timer(&run.id, "", TimerKind::StartTimeout, now + ms).await?;
            }
            if let Some(ms) = t.finish_ms {
                self.timer(&run.id, "", TimerKind::FinishTimeout, now + ms).await?;
            }
        }
        Ok(run)
    }

    async fn new_invocation(&self, run: &Run, def: &FunctionDef, due: Ms, flows: Vec<Flow>) -> Result<Message> {
        self.enqueue(
            NewMessage {
                queue: Some(fn_queue(&def.id)),
                retries: def.retries.unwrap_or(3).min(self.config.max_retries),
                backoff: def.backoff.clone().unwrap_or_else(|| self.config.backoff.clone()),
                timeout_ms: self.config.run_timeout_ms,
                flows,
                run_id: Some(run.id.clone()),
                ..NewMessage::internal(Target::Run { run_id: run.id.clone() }, due)
            },
            b"",
        )
        .await
    }

    async fn timer(&self, run_id: &str, step_id: &str, kind: TimerKind, at: Ms) -> Result<()> {
        let key = format!("timer:{run_id}:{step_id}:{kind:?}");
        if !self.store.kv_put(&key, &json!(at), Expect::Absent, Some(at + DAY)).await? {
            return Ok(());
        }
        self.enqueue(
            NewMessage {
                run_id: Some(run_id.into()),
                ..NewMessage::internal(Target::Timer { run_id: run_id.into(), step_id: step_id.into(), kind }, at)
            },
            b"",
        )
        .await?;
        Ok(())
    }

    async fn add_wait_path(&self, event: &str, path: &str) -> Result<()> {
        let key = format!("waitpaths:{event}");
        let path = path.trim().trim_start_matches("event.").to_string();
        for _ in 0..20 {
            let (mut paths, expect) = match self.store.kv_get(&key).await? {
                Some(v) => (serde_json::from_value::<Vec<String>>(v.value).unwrap_or_default(), Expect::Version(v.version)),
                None => (vec![], Expect::Absent),
            };
            if paths.contains(&path) {
                return Ok(());
            }
            paths.push(path.clone());
            if self.store.kv_put(&key, &json!(paths), expect, None).await? {
                return Ok(());
            }
        }
        Err(TaskError::Conflict("wait paths are contended".into()))
    }

    async fn resolve_waits(&self, ev: &Event, json: &Value) -> Result<()> {
        let mut candidates = self.store.waits(&ev.name, None).await?;
        if let Some(v) = self.store.kv_get(&format!("waitpaths:{}", ev.name)).await? {
            for path in serde_json::from_value::<Vec<String>>(v.value).unwrap_or_default() {
                if let Some(key) = match_key(&path, json) {
                    candidates.extend(self.store.waits(&ev.name, Some(&key)).await?);
                }
            }
        }
        for w in candidates {
            if let Some(cond) = &w.if_ {
                let ctx = json!({ "event": w.origin, "async": json });
                match expr::parse(cond) {
                    Ok(e) if e.test(&ctx) => {}
                    _ => continue,
                }
            }
            match w.kind {
                WaitKind::Step => {
                    if let Some(step) = &w.step_id {
                        self.resume(&w.run_id, step, Some(json.clone()), None).await?;
                    }
                }
                WaitKind::Cancel => {
                    let _ = self.cancel_run(&w.run_id, &format!("canceled by event {}", ev.name)).await;
                }
            }
            self.store.wait_delete(&w).await?;
        }
        Ok(())
    }

    /// Resolve a step the run is blocked on and invoke the run again.
    pub(crate) async fn resume(&self, run_id: &str, step_id: &str, data: Option<Value>, error: Option<Value>) -> Result<bool> {
        let Some(run) = self.store.run_get(run_id).await? else { return Ok(false) };
        let Some(op) = run.pending.iter().find(|p| p.step_id == step_id).cloned() else { return Ok(false) };
        // First resolver wins (event vs timeout).
        let claim = format!("resolved:{run_id}:{step_id}");
        if !self.store.kv_put(&claim, &json!(true), Expect::Absent, Some(now_ms() + self.config.retention_ms)).await? {
            let memo = self.store.steps(run_id).await?;
            if !memo.iter().any(|s| s.id == step_id) {
                return Ok(false);
            }
        } else {
            let data = if error.is_some() { None } else { Some(data.unwrap_or(Value::Null)) };
            self.store
                .step_put(&Step { run_id: run_id.into(), id: step_id.into(), name: None, op: op.op.clone(), data, error, at: now_ms(), attempts: 1 })
                .await?;
            self.count(|u| u.steps += 1);
        }
        if op.op == "wait_for_event"
            && let Some(event) = &op.target
        {
            let _ = self
                .store
                .wait_delete(&Wait {
                    id: format!("{run_id}:{step_id}"),
                    event: event.clone(),
                    key: op.key.clone(),
                    kind: WaitKind::Step,
                    run_id: run_id.into(),
                    step_id: None,
                    if_: None,
                    origin: Value::Null,
                    expires_at: 0,
                })
                .await;
        }
        self.mutate_run(run_id, |run| run.pending.retain(|p| p.step_id != step_id)).await?;
        Ok(true)
    }

    /// Apply a change to a run and make sure it's invoked if needed.
    async fn mutate_run(&self, run_id: &str, f: impl Fn(&mut Run)) -> Result<Option<Run>> {
        let mut pre: Option<Message> = None;
        for _ in 0..20 {
            let Some(mut run) = self.store.run_get(run_id).await? else { return Ok(None) };
            if run.status.is_final() {
                break;
            }
            let v = run.version;
            f(&mut run);
            if run.invocation.is_some() {
                run.dirty = true;
            } else {
                if pre.is_none() {
                    let def = self.function_def(&run.function_id).map(|(_, d)| d).unwrap_or_default();
                    let flows: Vec<Flow> = run.keys.get("flows").and_then(|f| serde_json::from_str(f).ok()).unwrap_or_default();
                    pre = Some(self.new_invocation(&run, &FunctionDef { id: run.function_id.clone(), ..def }, now_ms(), flows).await?);
                }
                run.invocation = pre.as_ref().map(|m| m.id.clone());
                run.dirty = false;
                run.status = RunStatus::Running;
            }
            if self.store.run_put(&run, Expect::Version(v)).await? {
                run.version = v + 1;
                if let Some(m) = pre
                    && run.invocation.as_deref() != Some(m.id.as_str())
                {
                    let _ = self.cancel(&m.id, "not needed").await;
                }
                return Ok(Some(run));
            }
        }
        if let Some(m) = pre {
            let _ = self.cancel(&m.id, "not needed").await;
        }
        Ok(None)
    }

    pub async fn run(&self, id: &str) -> Result<Run> {
        self.store.run_get(id).await?.ok_or_else(|| TaskError::NotFound(format!("run {id}")))
    }

    pub async fn runs(&self, q: &RunQuery) -> Result<store::Page<Run>> {
        Ok(self.store.run_list(q).await?)
    }

    pub async fn run_steps(&self, id: &str) -> Result<Vec<Step>> {
        Ok(self.store.steps(id).await?)
    }

    /// Wait for a run to finish (up to `timeout_ms`).
    pub async fn wait_run(&self, id: &str, timeout_ms: i64) -> Result<Run> {
        let deadline = now_ms() + timeout_ms.clamp(0, 300_000);
        let mut delay = 10;
        loop {
            let run = self.run(id).await?;
            if run.status.is_final() || now_ms() >= deadline {
                return Ok(run);
            }
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            delay = (delay * 2).min(250);
        }
    }

    pub async fn cancel_run(&self, id: &str, reason: &str) -> Result<Run> {
        let run = self.run(id).await?;
        if run.status.is_final() {
            return Err(TaskError::Conflict(format!("run {id} is already {}", run.status.as_str())));
        }
        let error = json!({ "name": "RunCanceled", "message": reason });
        let run = self.end_run(id, RunStatus::Canceled, None, Some(error)).await?;
        Ok(run)
    }

    /// Cancel unfinished runs of a function, optionally only those whose event matches `if`.
    pub async fn cancel_runs(&self, function_id: &str, cond: Option<&str>, reason: &str) -> Result<usize> {
        let cond = cond.map(expr::parse).transpose().map_err(TaskError::Invalid)?;
        let mut q = RunQuery { function_id: Some(function_id.into()), limit: 500, ..Default::default() };
        let mut n = 0;
        loop {
            let page = self.store.run_list(&q).await?;
            for r in page.items.iter().filter(|r| !r.status.is_final()) {
                if let Some(c) = &cond
                    && !c.test(&json!({ "event": r.events.first() }))
                {
                    continue;
                }
                if self.cancel_run(&r.id, reason).await.is_ok() {
                    n += 1;
                }
            }
            match page.cursor {
                Some(c) => q.cursor = Some(c),
                None => return Ok(n),
            }
        }
    }

    /// Start a new run with the same events.
    pub async fn rerun(&self, id: &str) -> Result<Run> {
        let old = self.run(id).await?;
        let (app, def) = self.function_def(&old.function_id).ok_or_else(|| TaskError::NotFound(format!("function {}", old.function_id)))?;
        self.start_run(&app, &def, old.events, StartOpts::default()).await
    }

    /// Put a run in a final state, clean up after it, and tell whoever is waiting.
    async fn end_run(&self, id: &str, status: RunStatus, output: Option<Value>, error: Option<Value>) -> Result<Run> {
        let mut ended: Option<Run> = None;
        for _ in 0..20 {
            let mut run = self.run(id).await?;
            if run.status.is_final() {
                return Ok(run);
            }
            let v = run.version;
            let now = now_ms();
            run.status = status;
            run.output = output.clone();
            run.error = error.clone();
            run.ended_at = Some(now);
            run.expires_at = Some(now + self.config.retention_ms);
            let pending = std::mem::take(&mut run.pending);
            let invocation = run.invocation.take();
            run.dirty = false;
            if self.store.run_put(&run, Expect::Version(v)).await? {
                run.version = v + 1;
                if status == RunStatus::Canceled {
                    if let Some(inv) = &invocation
                        && let Ok(Some(m)) = self.store.msg_get(inv).await
                        && m.state == MsgState::Pending
                    {
                        let _ = self.cancel(inv, "run canceled").await;
                    }
                    for p in &pending {
                        if let Some(child) = &p.child_run {
                            let _ = Box::pin(self.cancel_run(child, "parent canceled")).await;
                        }
                    }
                }
                for p in pending.iter().filter(|p| p.op == "wait_for_event") {
                    if let Some(event) = &p.target {
                        let _ = self
                            .store
                            .wait_delete(&wait_ref(&format!("{id}:{}", p.step_id), event, p.key.clone(), &run.id))
                            .await;
                    }
                }
                ended = Some(run);
                break;
            }
        }
        let run = ended.ok_or_else(|| TaskError::Conflict(format!("run {id} is changing")))?;
        let def = self.function_def(&run.function_id).map(|(_, d)| d);
        if let Some(def) = &def {
            for (i, c) in def.cancel_on.iter().enumerate() {
                let key = c.match_.as_ref().and_then(|m| match_key(m, run.events.first().unwrap_or(&Value::Null)));
                let _ = self.store.wait_delete(&wait_ref(&format!("{id}:cancel:{i}"), &c.event, key, id)).await;
            }
        }
        if let Some(key) = run.keys.get("singleton")
            && let Some(v) = self.store.kv_get(key).await?
            && v.value.as_str() == Some(id)
        {
            let _ = self.store.kv_delete(key, Expect::Version(v.version)).await;
        }
        if let Some((parent, step)) = &run.parent {
            match status {
                RunStatus::Completed => Box::pin(self.resume(parent, step, Some(run.output.clone().unwrap_or(Value::Null)), None)).await?,
                _ => Box::pin(self.resume(parent, step, None, Some(run.error.clone().unwrap_or(json!({ "name": "InvokeFailed" }))))).await?,
            };
        }
        let name = match status {
            RunStatus::Completed => EVENT_FINISHED,
            RunStatus::Failed => EVENT_FAILED,
            _ => EVENT_CANCELED,
        };
        let triggered_by_failure = run.events.first().and_then(|e| e["name"].as_str()) == Some(EVENT_FAILED);
        if !(status == RunStatus::Failed && triggered_by_failure) {
            let data = json!({
                "function_id": run.function_id,
                "run_id": run.id,
                "output": run.output,
                "error": run.error,
                "event": run.events.first(),
            });
            if let Err(e) = Box::pin(self.send_events(vec![EventIn { name: name.into(), data, ..Default::default() }])).await {
                warn!(error = %e, "lifecycle event failed");
            }
        }
        Ok(run)
    }
}

#[derive(Default)]
struct StartOpts {
    run_id: Option<String>,
    parent: Option<(String, String)>,
    delay_ms: i64,
}

fn wait_ref(id: &str, event: &str, key: Option<String>, run_id: &str) -> Wait {
    Wait {
        id: id.into(),
        event: event.into(),
        key,
        kind: WaitKind::Step,
        run_id: run_id.into(),
        step_id: None,
        if_: None,
        origin: Value::Null,
        expires_at: 0,
    }
}

/// Index key for a `match` path, from an event.
fn match_key(path: &str, event: &Value) -> Option<String> {
    let segs = expr::parse_match_path(path).ok()?;
    let v = expr::get_path(event, &segs).cloned().unwrap_or(Value::Null);
    let path = path.trim().trim_start_matches("event.");
    Some(format!("{path}={v}"))
}

// --- handlers called by the worker ---

#[derive(Deserialize)]
struct Reply {
    status: String,
    #[serde(default)]
    output: Value,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default)]
    retriable: Option<bool>,
    #[serde(default)]
    steps: Vec<Op>,
}

#[derive(Clone, Deserialize)]
struct Op {
    op: String,
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
    #[serde(default)]
    retriable: Option<bool>,
    #[serde(default)]
    until: Option<Ms>,
    #[serde(default)]
    duration_ms: Option<i64>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default, rename = "match")]
    match_: Option<String>,
    #[serde(default, rename = "if")]
    if_: Option<String>,
    #[serde(default)]
    timeout_ms: Option<i64>,
    #[serde(default)]
    function: Option<String>,
    #[serde(default)]
    events: Option<Vec<EventIn>>,
}

fn fail(error: impl Into<String>, retryable: bool) -> Done {
    Done::Failed { outcome: Outcome { at: now_ms(), error: Some(error.into()), ..Default::default() }, after_ms: None, retryable }
}

/// Call the function for a run.
pub(crate) async fn invoke(t: &Arc<Tasks>, cell: &Arc<Mutex<Message>>) -> anyhow::Result<Done> {
    let c = cell.lock().await.clone();
    let Target::Run { run_id } = &c.target else { unreachable!() };

    // Take ownership of the run's single invocation slot.
    let mut run = loop {
        let Some(mut run) = t.store.run_get(run_id).await? else { return Ok(Done::Skip("run not found".into())) };
        if run.status.is_final() {
            return Ok(Done::Skip(format!("run {}", run.status.as_str())));
        }
        if let Some(other) = &run.invocation
            && other != &c.id
            && let Some(m) = t.store.msg_get(other).await?
            && !m.state.is_final()
        {
            return Ok(Done::Skip("superseded".into()));
        }
        let v = run.version;
        run.invocation = Some(c.id.clone());
        run.status = RunStatus::Running;
        run.started_at.get_or_insert(now_ms());
        run.invocations += 1;
        if t.store.run_put(&run, Expect::Version(v)).await? {
            run.version = v + 1;
            break run;
        }
    };

    let Some((app, def)) = t.function_def(&run.function_id) else {
        t.end_run(&run.id, RunStatus::Failed, None, Some(json!({ "name": "FunctionNotFound", "message": "the function is no longer registered" })))
            .await?;
        return Ok(Done::Skip("function not registered".into()));
    };
    let steps = t.store.steps(&run.id).await?;
    let memo: serde_json::Map<String, Value> = steps
        .iter()
        .map(|s| (s.id.clone(), match &s.error {
            Some(e) => json!({ "error": e }),
            None => json!({ "data": s.data }),
        }))
        .collect();
    let payload = json!({
        "function_id": def.id,
        "run_id": run.id,
        "attempt": c.attempts.saturating_sub(1),
        "event": run.events.first(),
        "events": run.events,
        "steps": memo,
        "ctx": { "run_id": run.id, "app_id": app.id, "invocation": run.invocations },
    });
    let body = json_body(&payload);
    let mut url = reqwest::Url::parse(&app.url)?;
    url.query_pairs_mut().append_pair("fnId", &def.id);
    let mut req = t
        .http
        .post(url)
        .timeout(std::time::Duration::from_millis(c.timeout_ms.max(1000) as u64))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header("roll-run-id", &run.id)
        .header("roll-function-id", &def.id)
        .header("roll-message-id", &c.id)
        .header("roll-retried", c.attempts.saturating_sub(1).to_string());
    for (k, v) in t.sign_headers(&c.id, &body) {
        req = req.header(k, v);
    }
    t.count(|u| {
        u.deliveries += 1;
        u.bytes_out += body.len() as u64;
    });
    let started = std::time::Instant::now();
    let resp = match req.body(body).send().await {
        Ok(r) => r,
        Err(e) => return Ok(fail(format!("calling {}: {e}", app.url), true)),
    };
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    let duration_ms = started.elapsed().as_millis() as i64;
    if !status.is_success() {
        let outcome = Outcome {
            at: now_ms(),
            status: Some(status.as_u16()),
            body: Some(crate::dispatch::text_or_base64(&bytes[..bytes.len().min(t.config.response_max_bytes)])),
            duration_ms,
            error: None,
        };
        return Ok(Done::Failed { outcome, after_ms: None, retryable: status.as_u16() != 489 });
    }
    let reply: Reply = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => return Ok(fail(format!("the function's reply isn't valid: {e}"), true)),
    };
    let summary = Outcome {
        at: now_ms(),
        status: Some(status.as_u16()),
        body: Some(format!("{} ({} steps)", reply.status, reply.steps.len())),
        duration_ms,
        error: None,
    };
    match reply.status.as_str() {
        "done" => {
            t.end_run(&run.id, RunStatus::Completed, Some(reply.output), None).await?;
            Ok(Done::Ok(summary))
        }
        "error" => {
            let error = reply.error.unwrap_or(json!({ "name": "Error", "message": "the function failed" }));
            if reply.retriable.unwrap_or(true) && c.attempts <= c.retries {
                let msg = error["message"].as_str().unwrap_or("the function failed").to_string();
                Ok(Done::Failed { outcome: Outcome { error: Some(msg), ..summary }, after_ms: None, retryable: true })
            } else {
                t.end_run(&run.id, RunStatus::Failed, None, Some(error)).await?;
                Ok(Done::Ok(summary))
            }
        }
        "steps" => {
            if let Err(e) = apply_ops(t, &mut run, &def, &c, reply.steps).await {
                t.end_run(&run.id, RunStatus::Failed, None, Some(json!({ "name": "ProtocolError", "message": e.to_string() }))).await?;
            }
            Ok(Done::Ok(summary))
        }
        other => {
            t.end_run(&run.id, RunStatus::Failed, None, Some(json!({ "name": "ProtocolError", "message": format!("unknown status {other:?}") })))
                .await?;
            Ok(Done::Ok(summary))
        }
    }
}

async fn apply_ops(t: &Arc<Tasks>, run: &mut Run, def: &FunctionDef, c: &Message, ops: Vec<Op>) -> Result<()> {
    let now = now_ms();
    let memo: std::collections::HashSet<String> = t.store.steps(&run.id).await?.into_iter().map(|s| s.id).collect();
    let retries = def.retries.unwrap_or(3);
    let mut progressed = false;
    let mut retry_at: Option<Ms> = None;
    let mut new_pending: Vec<PendingOp> = Vec::new();
    let mut attempts: BTreeMap<String, u32> = BTreeMap::new();
    let first = run.events.first().cloned().unwrap_or(Value::Null);

    for op in ops {
        if op.id.is_empty() {
            return invalid("step without an id");
        }
        let pending = run.pending.iter().any(|p| p.step_id == op.id) || new_pending.iter().any(|p| p.step_id == op.id);
        if memo.contains(&op.id) && op.op != "run" {
            continue;
        }
        let step = |data: Option<Value>, error: Option<Value>, attempts: u32| Step {
            run_id: run.id.clone(),
            id: op.id.clone(),
            name: op.name.clone(),
            op: op.op.clone(),
            data,
            error,
            at: now_ms(),
            attempts,
        };
        match op.op.as_str() {
            "run" | "step" => {
                if memo.contains(&op.id) {
                    continue;
                }
                let n = run.step_attempts.get(&op.id).copied().unwrap_or(0) + 1;
                match &op.error {
                    Some(err) if op.retriable.unwrap_or(true) && n <= retries => {
                        attempts.insert(op.id.clone(), n);
                        let at = now + c.backoff.delay_ms(n, crate::ids::random_f64());
                        retry_at = Some(retry_at.map_or(at, |r: Ms| r.min(at)));
                        let _ = err;
                    }
                    Some(err) => {
                        t.store.step_put(&step(None, Some(err.clone()), n)).await?;
                        progressed = true;
                    }
                    None => {
                        t.store.step_put(&step(Some(op.data.clone().unwrap_or(Value::Null)), None, n)).await?;
                        progressed = true;
                    }
                }
                t.count(|u| u.steps += 1);
            }
            "sleep" => {
                if pending {
                    continue;
                }
                let until = op.until.or(op.duration_ms.map(|d| now + d)).ok_or_else(|| TaskError::Invalid("sleep needs until or duration_ms".into()))?;
                if until <= now {
                    t.store.step_put(&step(Some(Value::Null), None, 1)).await?;
                    progressed = true;
                } else {
                    t.timer(&run.id, &op.id, TimerKind::Sleep, until).await?;
                    new_pending.push(PendingOp { step_id: op.id.clone(), op: op.op.clone(), until: Some(until), target: None, key: None, child_run: None });
                }
            }
            "wait_for_event" => {
                if pending {
                    continue;
                }
                let event = op.event.clone().ok_or_else(|| TaskError::Invalid("wait_for_event needs an event".into()))?;
                let timeout = op.timeout_ms.unwrap_or(7 * DAY).clamp(1, 366 * DAY);
                if let Some(cond) = &op.if_ {
                    expr::parse(cond).map_err(|e| TaskError::Invalid(format!("wait_for_event if: {e}")))?;
                }
                let key = match &op.match_ {
                    Some(m) => {
                        expr::parse_match_path(m).map_err(|e| TaskError::Invalid(format!("wait_for_event match: {e}")))?;
                        t.add_wait_path(&event, m).await?;
                        match_key(m, &first)
                    }
                    None => None,
                };
                let w = Wait {
                    id: format!("{}:{}", run.id, op.id),
                    event: event.clone(),
                    key: key.clone(),
                    kind: WaitKind::Step,
                    run_id: run.id.clone(),
                    step_id: Some(op.id.clone()),
                    if_: op.if_.clone(),
                    origin: first.clone(),
                    expires_at: now + timeout + 60_000,
                };
                t.store.wait_put(&w).await?;
                t.timer(&run.id, &op.id, TimerKind::WaitTimeout, now + timeout).await?;
                new_pending.push(PendingOp { step_id: op.id.clone(), op: op.op.clone(), until: Some(now + timeout), target: Some(event), key, child_run: None });
            }
            "invoke" => {
                if pending {
                    continue;
                }
                let fid = op.function.clone().ok_or_else(|| TaskError::Invalid("invoke needs a function".into()))?;
                let (app, child_def) = match t.function_def(&fid) {
                    Some(f) => f,
                    None => {
                        t.store.step_put(&step(None, Some(json!({ "name": "FunctionNotFound", "message": format!("no function {fid}") })), 1)).await?;
                        progressed = true;
                        continue;
                    }
                };
                // Deterministic child id: re-reported invokes don't start twice.
                let child_id = format!("run_{}", &crate::sign::sha256_hex(format!("{}:{}", run.id, op.id).as_bytes())[..26]);
                let event = json!({
                    "id": new_id("evt"),
                    "name": EVENT_INVOKED,
                    "data": op.data.clone().unwrap_or(Value::Null),
                    "ts": now,
                    "user": Value::Null,
                });
                if t.store.run_get(&child_id).await?.is_none() {
                    t.start_run(
                        &app,
                        &child_def,
                        vec![event],
                        StartOpts { run_id: Some(child_id.clone()), parent: Some((run.id.clone(), op.id.clone())), delay_ms: 0 },
                    )
                    .await?;
                }
                if let Some(ms) = op.timeout_ms {
                    t.timer(&run.id, &op.id, TimerKind::InvokeTimeout, now + ms).await?;
                }
                new_pending.push(PendingOp {
                    step_id: op.id.clone(),
                    op: op.op.clone(),
                    until: op.timeout_ms.map(|ms| now + ms),
                    target: Some(fid),
                    key: None,
                    child_run: Some(child_id),
                });
            }
            "send_event" => {
                let mut events = op.events.clone().unwrap_or_default();
                for (i, e) in events.iter_mut().enumerate() {
                    e.id.get_or_insert_with(|| format!("{}:{}:{i}", run.id, op.id));
                }
                let ids = Box::pin(t.send_events(events)).await?;
                t.store.step_put(&step(Some(json!({ "ids": ids })), None, 1)).await?;
                progressed = true;
            }
            other => return invalid(format!("unknown step op {other:?}")),
        }
    }

    // Record what the run is waiting on and line up the next call.
    let my_id = c.id.clone();
    let mut pre: Option<Message> = None;
    for _ in 0..20 {
        let Some(mut fresh) = t.store.run_get(&run.id).await? else { break };
        if fresh.status.is_final() {
            break;
        }
        let v = fresh.version;
        for p in &new_pending {
            if !fresh.pending.iter().any(|x| x.step_id == p.step_id) {
                fresh.pending.push(p.clone());
            }
        }
        for (k, n) in &attempts {
            fresh.step_attempts.insert(k.clone(), *n);
        }
        if fresh.invocation.as_deref() == Some(my_id.as_str()) {
            // A step due for a retry sets the pace, even if siblings progressed: calling sooner
            // would retry it without its backoff.
            let next_at = retry_at.or_else(|| (progressed || fresh.dirty).then(now_ms));
            match next_at {
                Some(at) => {
                    if pre.is_none() {
                        let flows: Vec<Flow> = fresh.keys.get("flows").and_then(|f| serde_json::from_str(f).ok()).unwrap_or_default();
                        pre = Some(t.new_invocation(&fresh, def, at, flows).await?);
                    }
                    fresh.invocation = pre.as_ref().map(|m| m.id.clone());
                    fresh.status = RunStatus::Running;
                }
                None if fresh.pending.is_empty() => {
                    if let Some(m) = pre.take() {
                        let _ = t.cancel(&m.id, "not needed").await;
                    }
                    return invalid("the function reported no progress and nothing to wait for");
                }
                None => {
                    fresh.invocation = None;
                    fresh.status = RunStatus::Waiting;
                }
            }
            fresh.dirty = false;
        }
        if t.store.run_put(&fresh, Expect::Version(v)).await? {
            fresh.version = v + 1;
            if let Some(m) = &pre
                && fresh.invocation.as_deref() != Some(m.id.as_str())
            {
                let _ = t.cancel(&m.id, "not needed").await;
            }
            *run = fresh;
            return Ok(());
        }
    }
    if let Some(m) = pre {
        let _ = t.cancel(&m.id, "not needed").await;
    }
    Ok(())
}

/// A run timer came due.
pub(crate) async fn timer(t: &Arc<Tasks>, m: &Message) -> anyhow::Result<Done> {
    let Target::Timer { run_id, step_id, kind } = &m.target else { unreachable!() };
    let ok = Done::Ok(Outcome { at: now_ms(), ..Default::default() });
    let Some(run) = t.store.run_get(run_id).await? else { return Ok(Done::Skip("run not found".into())) };
    if run.status.is_final() {
        return Ok(Done::Skip("run finished".into()));
    }
    match kind {
        TimerKind::Sleep | TimerKind::WaitTimeout => {
            t.resume(run_id, step_id, Some(Value::Null), None).await?;
        }
        TimerKind::InvokeTimeout => {
            let child = run.pending.iter().find(|p| &p.step_id == step_id).and_then(|p| p.child_run.clone());
            let resolved = t
                .resume(run_id, step_id, None, Some(json!({ "name": "InvokeTimeout", "message": "the invoked function didn't finish in time" })))
                .await?;
            if resolved && let Some(child) = child {
                let _ = t.cancel_run(&child, "invoke timed out").await;
            }
        }
        TimerKind::StartTimeout => {
            if run.status == RunStatus::Queued && run.started_at.is_none() {
                t.end_run(run_id, RunStatus::Canceled, None, Some(json!({ "name": "StartTimeout", "message": "the run didn't start in time" }))).await?;
            }
        }
        TimerKind::FinishTimeout => {
            t.end_run(run_id, RunStatus::Canceled, None, Some(json!({ "name": "FinishTimeout", "message": "the run didn't finish in time" }))).await?;
        }
    }
    Ok(ok)
}

/// A batch window closed.
pub(crate) async fn flush(t: &Arc<Tasks>, m: &Message) -> anyhow::Result<Done> {
    let Target::Flush { function_id, key } = &m.target else { unreachable!() };
    let (kv_key, batch_id) = key.rsplit_once('#').unwrap_or((key.as_str(), ""));
    let Some(v) = t.store.kv_get(kv_key).await? else { return Ok(Done::Skip("batch already ran".into())) };
    if v.value["id"].as_str() != Some(batch_id) {
        return Ok(Done::Skip("batch already ran".into()));
    }
    if !t.store.kv_delete(kv_key, Expect::Version(v.version)).await? {
        return Ok(Done::Skip("batch already ran".into()));
    }
    let ids: Vec<String> = serde_json::from_value(v.value["events"].clone()).unwrap_or_default();
    let Some((app, def)) = t.function_def(function_id) else { return Ok(Done::Skip("function not registered".into())) };
    let events = t.load_events(&ids).await?;
    let run = t.start_run(&app, &def, events, StartOpts::default()).await?;
    Ok(Done::Ok(Outcome { at: now_ms(), body: Some(run.id), ..Default::default() }))
}

/// A message ran out of retries.
pub(crate) async fn on_dead(t: &Arc<Tasks>, m: &Message) -> anyhow::Result<()> {
    if let Target::Run { run_id } = &m.target {
        let message = m
            .last
            .as_ref()
            .map(|o| match (&o.status, &o.error) {
                (_, Some(e)) => e.clone(),
                (Some(s), _) => format!("HTTP {s}"),
                _ => "failed".into(),
            })
            .unwrap_or_else(|| "failed".into());
        if let Some(run) = t.store.run_get(run_id).await?
            && !run.status.is_final()
            && run.invocation.as_deref() == Some(m.id.as_str())
        {
            t.end_run(run_id, RunStatus::Failed, None, Some(json!({ "name": "Error", "message": message, "attempts": m.attempts })))
                .await?;
        }
    }
    Ok(())
}
