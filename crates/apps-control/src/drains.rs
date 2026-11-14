//! Log drains (docs/apps/README.md, "Log drains"; Render's log streams, Fly's and Heroku's
//! drains, Vercel's): a project's services' output (runtime, requests, builds, as each drain
//! chooses, filtered by environment and service), as it's written, POSTed in batches of NDJSON to
//! HTTPS endpoints the project names, with headers of its own (a vendor's API key, sealed). Each node sends what its own instances write (the log tap):
//! nothing crosses nodes. Batches are signed as webhooks are (`X-Roll-Signature-256` over
//! `<timestamp>.<body>`, `X-Roll-Delivery` the batch's id). Best effort, as drains are: a batch
//! is tried three times, then dropped and counted; a drain that can't keep up loses its oldest
//! lines, never a writer's time or the node's memory.

use crate::model::{DRAIN_SOURCES, LogDrain};
use crate::store::{DrainSettings, OpenedDrain};
use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Drains per project.
const MAX_DRAINS: usize = 5;
/// Lines one drain holds while its batch is out; past it, the oldest go.
const BUFFER: usize = 10_000;
/// The most lines a batch carries, and the longest a line waits for its batch.
const BATCH: usize = 500;
const LINGER: Duration = Duration::from_secs(1);
/// How long a node keeps a project's drains (and a service's project) before reading again.
const REFRESH: Duration = Duration::from_secs(30);

impl Apps {
    /// `POST …/projects/{project}/drains {url}`: a drain; its signing secret is in the answer
    /// this once.
    pub async fn add_drain(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["url", "sources", "environments", "services", "previews", "headers"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let settings = self.drain_settings(project, &o, None).await?;
        let url = o.get("url").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("url is required".into()))?.trim().to_string();
        let host = self.check_target("webhook", &url)?;
        {
            let (this, p) = (self.clone(), project.to_string());
            tokio::task::spawn_blocking(move || this.project_name(&p)).await.map_err(|e| Error::Internal(e.to_string()))??;
        }
        let p = project.to_string();
        if self.db(move |s| s.drains(&p)).await?.len() >= MAX_DRAINS {
            return Err(Error::Conflict(format!("a project has at most {MAX_DRAINS} log drains")));
        }
        let secret = format!("whsec_{}", crate::model::random(32));
        let (p, s2) = (project.to_string(), secret.clone());
        let d = self.db(move |s| s.add_drain(&p, &host, &url, &s2, &settings)).await?;
        self.drains_changed();
        let mut v = serde_json::to_value(&d).expect("JSON");
        v["secret"] = json!(secret);
        Ok(v)
    }

    pub async fn drains(self: &Arc<Self>, project: &str) -> Result<Vec<LogDrain>> {
        let p = project.to_string();
        self.db(move |s| s.drains(&p)).await
    }

    /// A drain's sources, filters and headers from a body: what's absent is `current`'s (an
    /// edit), else the defaults (runtime output, every environment but previews, no headers).
    async fn drain_settings(self: &Arc<Self>, project: &str, o: &serde_json::Map<String, Value>, current: Option<&OpenedDrain>) -> Result<DrainSettings> {
        let list = |field: &str, max: usize| -> Result<Option<Vec<String>>> {
            match o.get(field) {
                None => Ok(None),
                Some(Value::Null) => Ok(Some(Vec::new())),
                Some(Value::Array(a)) if a.len() <= max => {
                    let v: Vec<String> = a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    if v.len() != a.len() {
                        return Err(Error::BadRequest(format!("{field} is a list of names")));
                    }
                    Ok(Some(v))
                }
                Some(_) => Err(Error::BadRequest(format!("{field} is a list of at most {max} names"))),
            }
        };
        let sources = match list("sources", 3)? {
            Some(v) if v.is_empty() || v.iter().any(|s| !DRAIN_SOURCES.contains(&s.as_str())) => return Err(Error::BadRequest(format!("sources are some of {}", DRAIN_SOURCES.join(", ")))),
            Some(v) => v,
            None => current.map_or_else(|| vec!["runtime".to_string()], |c| c.drain.sources.clone()),
        };
        let environments = list("environments", 50)?.unwrap_or_else(|| current.map(|c| c.drain.environments.clone()).unwrap_or_default());
        // A service by its name or id, kept by id; only the project's.
        let services = match list("services", 50)? {
            None => current.map(|c| c.drain.services.clone()).unwrap_or_default(),
            Some(wanted) => {
                let pp = project.to_string();
                let all = self.db(move |s| s.services(Some(&pp))).await?;
                wanted.iter().map(|w| all.iter().find(|s| &s.id == w || &s.name == w).map(|s| s.id.clone()).ok_or_else(|| Error::BadRequest(format!("services: the project has no service {w}")))).collect::<Result<_>>()?
            }
        };
        let previews = match o.get("previews") {
            None => current.is_some_and(|c| c.drain.previews),
            Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(Error::BadRequest("previews is true or false".into())),
        };
        // A vendor's key (Datadog's DD-API-KEY, an Authorization): `{name: value}`, sealed, never shown.
        let headers = match o.get("headers") {
            None => current.map(|c| c.headers.clone()).unwrap_or_default(),
            Some(Value::Null) => Vec::new(),
            Some(Value::Object(m)) if m.len() <= 10 => {
                let mut h = Vec::new();
                for (k, v) in m {
                    let v = v.as_str().ok_or_else(|| Error::BadRequest(format!("headers.{k} is text")))?;
                    let name_ok = !k.is_empty() && k.len() <= 64 && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
                    let lower = k.to_ascii_lowercase();
                    if !name_ok || ["content-type", "content-length", "host", "transfer-encoding", "connection"].contains(&lower.as_str()) || lower.starts_with("x-roll-") {
                        return Err(Error::BadRequest(format!("headers: {k:?} isn't a header roll sends for a drain")));
                    }
                    if v.is_empty() || v.len() > 1024 || v.chars().any(|c| c.is_control()) {
                        return Err(Error::BadRequest(format!("headers.{k} is 1–1024 printable characters")));
                    }
                    h.push((k.clone(), v.to_string()));
                }
                h
            }
            Some(_) => return Err(Error::BadRequest("headers is an object of at most 10 names to values".into())),
        };
        Ok(DrainSettings { sources, environments, services, previews, headers })
    }

    /// `PATCH …/drains/{did}`: its sources, filters and headers (given ones replace, the rest
    /// stay; `headers` replaces them all). Its URL and secret stay: a new URL is a new drain.
    pub async fn update_drain(self: &Arc<Self>, project: &str, id: &str, body: Value) -> Result<LogDrain> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["sources", "environments", "services", "previews", "headers"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("{k} can't be changed (a new URL is a new drain)")));
        }
        let p = project.to_string();
        let current = self.db(move |s| s.drain_secrets(&p)).await?.into_iter().find(|d| d.drain.id == id).ok_or_else(|| Error::NotFound(format!("no log drain {id}")))?;
        let settings = self.drain_settings(project, &o, Some(&current)).await?;
        let (p, i, headers) = (project.to_string(), id.to_string(), o.contains_key("headers"));
        let d = self.db(move |s| s.update_drain(&p, &i, &settings, headers)).await?;
        self.drains_changed();
        Ok(d)
    }

    /// A drain made, changed or deleted here takes effect at once on this node (others read
    /// their drains again within REFRESH).
    fn drains_changed(&self) {
        self.drains_gen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// `POST …/drains/{did}/test`: one line sent now, as a batch is (signed, with its headers),
    /// tried once: how it answered.
    pub async fn test_drain(self: &Arc<Self>, project: &str, id: &str) -> Result<Value> {
        let p = project.to_string();
        let d = self.db(move |s| s.drain_secrets(&p)).await?.into_iter().find(|d| d.drain.id == id).ok_or_else(|| Error::NotFound(format!("no log drain {id}")))?;
        let line = json!({ "t": code_sql::now_ms(), "source": "test", "text": "a test line from roll apps" });
        Ok(match self.post_batch(&d, &[line]).await {
            Ok(code) => json!({ "ok": (200..300).contains(&code), "code": code, "error": Value::Null }),
            Err(e) => json!({ "ok": false, "code": Value::Null, "error": e }),
        })
    }

    pub async fn delete_drain(self: &Arc<Self>, project: &str, id: &str) -> Result<()> {
        let (p, i) = (project.to_string(), id.to_string());
        self.db(move |s| s.delete_drain(&p, &i)).await?;
        self.drains_changed();
        Ok(())
    }

    /// Every node: read the log tap, batch each project's lines per drain, and send them.
    pub(crate) async fn drainer(self: Arc<Self>) {
        let mut tap = self.logs.tap(BUFFER);
        // service → (project, name), and project → its drains, each read again after REFRESH.
        let mut services: HashMap<String, (Instant, Option<(String, String)>)> = HashMap::new();
        let mut drains: HashMap<String, (Instant, Vec<OpenedDrain>)> = HashMap::new();
        // A build's deployment → its service and environment; an environment → a preview's.
        let mut builds: HashMap<String, Option<(String, String)>> = HashMap::new();
        let mut previews: HashMap<(String, String), (Instant, bool)> = HashMap::new();
        let mut seen_gen = self.drains_gen.load(std::sync::atomic::Ordering::SeqCst);
        let mut queues: HashMap<String, Queue> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                got = tap.recv() => {
                    let Some((key, line)) = got else { return };
                    // Which of a drain's sources the line is, and whose.
                    let (source, service, env) = if let Some((s, e)) = key.strip_prefix("env:").and_then(|k| k.split_once('/')) {
                        ("runtime", s.to_string(), e.to_string())
                    } else if let Some((s, e)) = key.strip_prefix("http:").and_then(|k| k.split_once('/')) {
                        ("http", s.to_string(), e.to_string())
                    } else if let Some(dep) = key.strip_prefix("build:") {
                        if !builds.contains_key(dep) {
                            if builds.len() > 1000 {
                                builds.clear();
                            }
                            let d = dep.to_string();
                            let found = self.db(move |s| s.deployment(&d)).await.ok().map(|d| (d.service, d.environment));
                            builds.insert(dep.to_string(), found);
                        }
                        let Some((s, e)) = builds[dep].clone() else { continue };
                        ("build", s, e)
                    } else {
                        continue;
                    };
                    let now_gen = self.drains_gen.load(std::sync::atomic::Ordering::SeqCst);
                    if now_gen != seen_gen {
                        drains.clear();
                        seen_gen = now_gen;
                    }
                    let fresh = |at: &Instant| at.elapsed() < REFRESH;
                    if !services.get(&service).is_some_and(|(at, _)| fresh(at)) {
                        let sid = service.clone();
                        let found = self.db(move |s| s.service(&sid)).await.ok().map(|s| (s.project, s.name));
                        services.insert(service.clone(), (Instant::now(), found));
                    }
                    let Some((project, name)) = services[&service].1.clone() else { continue };
                    if !drains.get(&project).is_some_and(|(at, _)| fresh(at)) {
                        let p = project.clone();
                        let list = self.db(move |s| s.drain_secrets(&p)).await.unwrap_or_default();
                        drains.insert(project.clone(), (Instant::now(), list));
                    }
                    let wanted: Vec<&OpenedDrain> = drains[&project].1.iter().filter(|o| {
                        let d = &o.drain;
                        d.sources.iter().any(|s| s == source) && (d.environments.is_empty() || d.environments.contains(&env)) && (d.services.is_empty() || d.services.contains(&service))
                    }).collect();
                    if wanted.is_empty() {
                        continue;
                    }
                    // A preview's output goes only to drains that ask for previews' (read once a while).
                    let k = (service.clone(), env.clone());
                    if !previews.get(&k).is_some_and(|(at, _)| fresh(at)) {
                        let (sid, e) = k.clone();
                        let ephemeral = self.db(move |s| s.environment(&sid, &e)).await.map(|r| r.ephemeral).unwrap_or(false);
                        previews.insert(k.clone(), (Instant::now(), ephemeral));
                    }
                    let preview = previews[&k].1;
                    for o in wanted.into_iter().filter(|o| !preview || o.drain.previews) {
                        let d = &o.drain;
                        let q = queues.entry(d.id.clone()).or_insert_with(Queue::new);
                        if q.lines.is_empty() {
                            q.first = Instant::now();
                        }
                        if q.lines.len() == BUFFER {
                            q.lines.pop_front();
                            q.dropped += 1;
                        }
                        q.lines.push_back(json!({ "t": line.t, "source": source, "service": service, "service_name": name, "environment": env, "instance": line.instance, "deployment": line.deployment, "stream": line.stream, "text": line.text }));
                    }
                }
                _ = tick.tick() => {
                    for (_, list) in drains.values() {
                        for o in list {
                            let d = &o.drain;
                            let Some(q) = queues.get_mut(&d.id) else { continue };
                            if q.out || q.lines.is_empty() || (q.lines.len() < BATCH && q.first.elapsed() < LINGER) {
                                continue;
                            }
                            let n = q.lines.len().min(BATCH);
                            let batch: Vec<Value> = q.lines.drain(..n).collect();
                            q.out = true;
                            let dropped = std::mem::take(&mut q.dropped);
                            if dropped > 0 {
                                tracing::warn!(drain = %d.id, dropped, "apps: a log drain fell behind; its oldest lines were dropped");
                            }
                            let (this, o) = (self.clone(), OpenedDrain { drain: d.clone(), url: o.url.clone(), secret: o.secret.clone(), headers: o.headers.clone() });
                            let done = self.drain_done.clone();
                            tokio::spawn(async move {
                                this.send_batch(&o, &batch).await;
                                done.lock().push(o.drain.id.clone());
                            });
                        }
                    }
                    for id in std::mem::take(&mut *self.drain_done.lock()) {
                        if let Some(q) = queues.get_mut(&id) {
                            q.out = false;
                        }
                    }
                    // Drains deleted (or of projects no longer seen) stop holding lines.
                    queues.retain(|id, _| drains.values().any(|(_, l)| l.iter().any(|o| &o.drain.id == id)));
                }
                _ = self.stop.cancelled() => return,
            }
        }
    }

    /// One batch, tried three times (1 s, then 5 s apart), then dropped.
    async fn send_batch(&self, o: &OpenedDrain, batch: &[Value]) {
        let drain = o.drain.id.as_str();
        for (i, wait) in [0u64, 1, 5].into_iter().enumerate() {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            match self.post_batch(o, batch).await {
                Ok(code) if (200..300).contains(&code) => return,
                Ok(code) if i == 2 => tracing::warn!(drain, status = code, lines = batch.len(), "apps: a log drain refused a batch; it's dropped"),
                Err(e) if i == 2 => tracing::warn!(drain, error = %e, lines = batch.len(), "apps: a log drain didn't answer; the batch is dropped"),
                _ => {}
            }
        }
    }

    /// A batch POSTed once, signed as webhooks are and with the drain's own headers: its status,
    /// or why there was none (never the URL).
    async fn post_batch(&self, o: &OpenedDrain, batch: &[Value]) -> std::result::Result<u16, String> {
        let mut body = Vec::new();
        for l in batch {
            serde_json::to_writer(&mut body, l).expect("JSON");
            body.push(b'\n');
        }
        let ts = (code_sql::now_ms() / 1000).to_string();
        let sig = {
            use hmac::Mac;
            let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(o.secret.as_bytes()).expect("any key");
            mac.update(ts.as_bytes());
            mac.update(b".");
            mac.update(&body);
            mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        let mut req = self
            .notify_client
            .post(&o.url)
            .header("content-type", "application/x-ndjson")
            .header("x-roll-delivery", format!("lgb_{}", crate::model::random(12)))
            .header("x-roll-timestamp", ts)
            .header("x-roll-signature-256", format!("sha256={sig}"));
        for (k, v) in &o.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        match req.body(body).send().await {
            Ok(r) => Ok(r.status().as_u16()),
            Err(e) => Err(e.without_url().to_string()),
        }
    }
}

/// One drain's lines waiting for their batch.
struct Queue {
    lines: VecDeque<Value>,
    /// When the oldest waiting line came (a batch goes after LINGER even if it's short).
    first: Instant,
    /// A batch is on its way: the next waits for it, so batches arrive in order.
    out: bool,
    dropped: u64,
}

impl Queue {
    fn new() -> Queue {
        Queue { lines: VecDeque::new(), first: Instant::now(), out: false, dropped: 0 }
    }
}
