//! Log drains (docs/apps/README.md, "Log drains"; Render's log streams, Fly's and Heroku's
//! drains, Vercel's): a project's services' output (runtime, requests, builds, as each drain
//! chooses, filtered by environment and service), as it's written, POSTed in batches of NDJSON to
//! HTTPS endpoints the project names, with headers of its own (a vendor's API key, sealed). Each node sends what its own instances write (the log tap):
//! nothing crosses nodes. Batches are signed as webhooks are (`X-Roll-Signature-256` over
//! `<timestamp>.<body>`, `X-Roll-Delivery` the batch's id). Best effort, as drains are: a batch
//! is tried three times, then dropped and counted; a drain that can't keep up loses its oldest
//! lines, never a writer's time or the node's memory.
//!
//! A drain's `format` picks the wire: `ndjson` (the above), `otlp` (OpenTelemetry logs as
//! OTLP/HTTP JSON to a collector's or a vendor's `/v1/logs`, a resource per service and
//! environment) or `syslog` (RFC 5424 messages over TLS with RFC 5425's octet counting, to
//! `syslog+tls://host:port`, as Heroku's and Papertrail's drains take them; a connection a batch,
//! public addresses only, checked as the HTTP client's resolver checks them).
//!
//! A preview's services on roll's own runtime (rolld's previews, `roll.toml`'s `[preview]`)
//! aren't roll apps instances, so their output comes in through [`Apps::drain_preview_lines`],
//! masked by previews, and goes to the drains that take previews' runtime output, in the
//! preview's `pv-` environment as its roll apps services' would; its jobs' output (`release`,
//! `seed`, `teardown`) is the `build` source, the job's name its `service_name`. Such a line
//! has no service id, instance or deployment, so a drain narrowed to services never gets one.

use crate::model::{DRAIN_FORMATS, DRAIN_SOURCES, LogDrain};
use crate::store::{DrainSettings, OpenedDrain};
use crate::traces::Signal;
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
/// An `s3` drain's: an object a batch, so fewer and bigger.
const S3_BATCH: usize = 5_000;
const S3_LINGER: Duration = Duration::from_secs(60);
/// An `s3` drain's credentials, in its sealed headers (never sent as headers).
const S3_KEYS: [&str; 3] = ["aws-access-key-id", "aws-secret-access-key", "aws-region"];
/// How long a node keeps a project's drains (and a service's project) before reading again.
const REFRESH: Duration = Duration::from_secs(30);
/// Previews' batches waiting for the drainer; past it, a batch is dropped (counted).
pub(crate) const PREVIEW_BATCHES: usize = 256;

/// A line a preview's service on roll's runtime wrote: when (ms), on stderr or not, what (masked).
#[derive(Clone, Debug)]
pub struct PreviewLine {
    pub t: i64,
    pub stderr: bool,
    pub text: String,
}

/// A preview service's (or job's) lines, with whose they are and which source.
pub(crate) struct PreviewBatch {
    project: String,
    environment: String,
    service: String,
    source: &'static str,
    lines: Vec<PreviewLine>,
}

/// Whether `d` takes a preview's `source` lines in `env` from roll's runtime (no service id).
fn takes_runtime_preview(d: &LogDrain, env: &str, source: &str) -> bool {
    d.previews && d.sources.iter().any(|s| s == source) && d.services.is_empty() && (d.environments.is_empty() || d.environments.iter().any(|e| e == env))
}

impl Apps {
    /// `POST …/projects/{project}/drains {url}`: a drain; its signing secret is in the answer
    /// this once.
    pub async fn add_drain(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["url", "sources", "environments", "services", "previews", "headers", "format"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let format = match o.get("format") {
            None | Some(Value::Null) => "ndjson",
            Some(Value::String(f)) if DRAIN_FORMATS.contains(&f.as_str()) => f.as_str(),
            Some(_) => return Err(Error::BadRequest(format!("format is one of {}", DRAIN_FORMATS.join(", ")))),
        }
        .to_string();
        let settings = self.drain_settings(project, &o, None).await?;
        let url = o.get("url").and_then(Value::as_str).ok_or_else(|| Error::BadRequest("url is required".into()))?.trim().to_string();
        let host = if format == "syslog" {
            // A syslog server takes no headers: a vendor's token goes in its host or port.
            if !settings.headers.is_empty() {
                return Err(Error::BadRequest("headers: a syslog drain is sent none".into()));
            }
            self.syslog_target(&url)?.0
        } else if format == "s3" {
            check_s3(&url, &settings.headers)?;
            self.check_target("webhook", &url)?
        } else {
            self.check_target("webhook", &url)?
        };
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
        let d = self.db(move |s| s.add_drain(&p, &host, &url, &s2, &format, &settings)).await?;
        self.drains_changed(project).await;
        let mut v = serde_json::to_value(&d).expect("JSON");
        v["secret"] = json!(secret);
        Ok(v)
    }

    pub async fn drains(self: &Arc<Self>, project: &str) -> Result<Vec<LogDrain>> {
        let p = project.to_string();
        self.db(move |s| s.drains(&p)).await
    }

    /// Whether the project has a drain that takes a preview's `source` lines (`runtime`, or
    /// `build` for its jobs) from roll's runtime (in `environment`, or in any `pv-` one when
    /// `None`).
    pub async fn drains_take_previews(self: &Arc<Self>, project: &str, environment: Option<&str>, source: &str) -> Result<bool> {
        let list = self.drains(project).await?;
        Ok(list.iter().any(|d| match environment {
            Some(e) => takes_runtime_preview(d, e, source),
            None => d.previews && d.sources.iter().any(|s| s == source) && d.services.is_empty() && (d.environments.is_empty() || d.environments.iter().any(|e| e.starts_with("pv-"))),
        }))
    }

    /// A preview service's lines from roll's runtime (`build`: a job's output, `service` the job's
    /// name), already masked, to the project's drains that take previews' lines of that source
    /// (each drain's queue, batches, signature and format, as a roll apps instance's lines). Never
    /// waits: past [`PREVIEW_BATCHES`] waiting, the batch is dropped, as a drain that can't keep
    /// up loses its oldest lines. Any source but `build` is `runtime`.
    pub fn drain_preview_lines(&self, project: &str, environment: &str, service: &str, source: &str, lines: Vec<PreviewLine>) {
        if lines.is_empty() {
            return;
        }
        let source = if source == "build" { "build" } else { "runtime" };
        let b = PreviewBatch { project: project.to_string(), environment: environment.to_string(), service: service.to_string(), source, lines };
        if let Err(tokio::sync::mpsc::error::TrySendError::Full(b)) = self.preview_lines.try_send(b) {
            tracing::warn!(project = %b.project, environment = %b.environment, lines = b.lines.len(), "apps: log drains fell behind on previews' lines; a batch was dropped");
        }
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
            return Err(Error::BadRequest(format!("{k} can't be changed (a new URL or format is a new drain)")));
        }
        let p = project.to_string();
        let current = self.db(move |s| s.drain_secrets(&p)).await?.into_iter().find(|d| d.drain.id == id).ok_or_else(|| Error::NotFound(format!("no log drain {id}")))?;
        let settings = self.drain_settings(project, &o, Some(&current)).await?;
        if current.drain.format == "s3" {
            check_s3(&current.url, &settings.headers)?;
        }
        if current.drain.format == "syslog" && !settings.headers.is_empty() {
            return Err(Error::BadRequest("headers: a syslog drain is sent none".into()));
        }
        let (p, i, headers) = (project.to_string(), id.to_string(), o.contains_key("headers"));
        let d = self.db(move |s| s.update_drain(&p, &i, &settings, headers)).await?;
        self.drains_changed(project).await;
        Ok(d)
    }

    /// A drain made, changed or deleted here takes effect at once on this node for its project
    /// (others read their drains again within REFRESH); other projects' lines are untouched.
    async fn drains_changed(self: &Arc<Self>, project: &str) {
        self.drains_dirty.lock().insert(project.to_string());
        // The tap's gate before the answer: a new drain of requests gets the next one.
        if let Ok((http, build, traces, metrics)) = self.db(|s| s.drain_sources_wanted()).await {
            self.logs.set_tapped(http, build, traces, metrics);
        }
    }

    /// `POST …/drains/{did}/test`: one line sent now, as a batch is (signed, with its headers),
    /// tried once: how it answered.
    pub async fn test_drain(self: &Arc<Self>, project: &str, id: &str) -> Result<Value> {
        let p = project.to_string();
        let d = self.db(move |s| s.drain_secrets(&p)).await?.into_iter().find(|d| d.drain.id == id).ok_or_else(|| Error::NotFound(format!("no log drain {id}")))?;
        let line = json!({ "t": code_sql::now_ms(), "source": "test", "text": "a test line from roll apps" });
        Ok(match self.post_batch(&d, &[line], &format!("lgb_{}", crate::model::random(12)), Signal::Logs).await {
            Ok(code) => json!({ "ok": code.is_none_or(|c| (200..300).contains(&c)), "code": code, "error": Value::Null }),
            Err(e) => json!({ "ok": false, "code": Value::Null, "error": e }),
        })
    }

    pub async fn delete_drain(self: &Arc<Self>, project: &str, id: &str) -> Result<()> {
        let (p, i) = (project.to_string(), id.to_string());
        self.db(move |s| s.delete_drain(&p, &i)).await?;
        self.drains_changed(project).await;
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
        // Request and build lines reach the tap only while some drain asks for them (the proxy's
        // hot path pays nothing otherwise): read from the drains' sources now and every REFRESH.
        let mut wanted_at = Instant::now() - REFRESH;
        let mut queues: HashMap<String, Queue> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut preview_rx = self.preview_lines_rx.lock().take();
        loop {
            tokio::select! {
                got = async { match preview_rx.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {
                    let Some(b) = got else { preview_rx = None; continue };
                    if self.drains_fresh(&mut drains, &b.project).await {
                        wanted_at = Instant::now() - REFRESH;
                    }
                    let wanted: Vec<&OpenedDrain> = drains[&b.project].1.iter().filter(|o| takes_runtime_preview(&o.drain, &b.environment, b.source)).collect();
                    for o in wanted {
                        for l in &b.lines {
                            let stream = if l.stderr { "stderr" } else { "stdout" };
                            enqueue(&mut queues, &o.drain.id, &b.project, json!({ "t": l.t, "source": b.source, "service": null, "service_name": b.service, "environment": b.environment, "instance": null, "deployment": null, "stream": stream, "text": l.text }));
                        }
                    }
                }
                got = tap.recv() => {
                    let Some((key, line)) = got else { return };
                    // Which of a drain's sources the line is, and whose.
                    let (source, service, env) = if let Some((s, e)) = key.strip_prefix("env:").and_then(|k| k.split_once('/')) {
                        ("runtime", s.to_string(), e.to_string())
                    } else if let Some((s, e)) = key.strip_prefix("http:").and_then(|k| k.split_once('/')) {
                        ("http", s.to_string(), e.to_string())
                    } else if let Some((s, e)) = key.strip_prefix("trace:").and_then(|k| k.split_once('/')) {
                        ("traces", s.to_string(), e.to_string())
                    } else if let Some((s, e)) = key.strip_prefix("metric:").and_then(|k| k.split_once('/')) {
                        ("metrics", s.to_string(), e.to_string())
                    } else if let Some((s, e)) = key.strip_prefix("audit:").and_then(|k| k.split_once('/')) {
                        // A deleted service's own entry (`audit.deleted`) is read deleted.
                        let sid = s.to_string();
                        let found = self.db(move |st| st.service_any(&sid)).await.ok().map(|(s, _)| (s.project, s.name));
                        services.insert(s.to_string(), (Instant::now(), found));
                        ("audit", s.to_string(), e.to_string())
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
                    let fresh = |at: &Instant| at.elapsed() < REFRESH;
                    // Bounded, as `builds` is: read again rather than kept forever.
                    if services.len() > 5000 {
                        services.clear();
                    }
                    if previews.len() > 5000 {
                        previews.clear();
                    }
                    if !services.get(&service).is_some_and(|(at, _)| fresh(at)) {
                        let sid = service.clone();
                        let found = self.db(move |s| s.service(&sid)).await.ok().map(|s| (s.project, s.name));
                        services.insert(service.clone(), (Instant::now(), found));
                    }
                    let Some((project, name)) = services[&service].1.clone() else { continue };
                    if self.drains_fresh(&mut drains, &project).await {
                        wanted_at = Instant::now() - REFRESH;
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
                        enqueue(&mut queues, &o.drain.id, &project, json!({ "t": line.t, "source": source, "service": service, "service_name": name, "environment": env, "instance": line.instance, "deployment": line.deployment, "stream": line.stream, "text": line.text }));
                    }
                }
                _ = tick.tick() => {
                    if wanted_at.elapsed() >= REFRESH {
                        wanted_at = Instant::now();
                        if let Ok((http, build, traces, metrics)) = self.db(|s| s.drain_sources_wanted()).await {
                            self.logs.set_tapped(http, build, traces, metrics);
                        }
                    }
                    for (_, list) in drains.values() {
                        for o in list {
                            let d = &o.drain;
                            let Some(q) = queues.get_mut(&d.id) else { continue };
                            let (most, linger) = if d.format == "s3" { (S3_BATCH, S3_LINGER) } else { (BATCH, LINGER) };
                            if q.out || q.lines.is_empty() || (q.lines.len() < most && q.first.elapsed() < linger) {
                                continue;
                            }
                            let n = q.lines.len().min(most);
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
                    // A deleted drain's queue goes once its project's drains are read again; a project not
                    // read yet (its drains changed a moment ago) keeps its queues, and its order.
                    queues.retain(|id, q| !drains.contains_key(&q.project) || drains.values().any(|(_, l)| l.iter().any(|o| &o.drain.id == id)));
                }
                _ = self.stop.cancelled() => return,
            }
        }
    }

    /// `project`'s drains in `drains`, read again when older than [`REFRESH`] or changed on this
    /// node. True when some project's drains had changed (what the tap is asked for is read again).
    async fn drains_fresh(self: &Arc<Self>, drains: &mut HashMap<String, (Instant, Vec<OpenedDrain>)>, project: &str) -> bool {
        let dirty = std::mem::take(&mut *self.drains_dirty.lock());
        for p in &dirty {
            drains.remove(p);
        }
        if !drains.get(project).is_some_and(|(at, _)| at.elapsed() < REFRESH) {
            let p = project.to_string();
            let list = self.db(move |s| s.drain_secrets(&p)).await.unwrap_or_default();
            drains.insert(project.to_string(), (Instant::now(), list));
        }
        !dirty.is_empty()
    }

    /// One batch, tried three times (1 s, then 5 s apart), then dropped.
    async fn send_batch(&self, o: &OpenedDrain, batch: &[Value]) {
        // OTLP's signals go apart: logs to the drain's URL, spans and points to its `/v1/traces`
        // and `/v1/metrics`, each in its own shape.
        if o.drain.format == "otlp" {
            let of = |src: &str| batch.iter().filter(|l| l["source"] == src).cloned().collect::<Vec<_>>();
            let logs: Vec<Value> = batch.iter().filter(|l| !matches!(l["source"].as_str(), Some("traces" | "metrics"))).cloned().collect();
            for (part, signal) in [(logs, Signal::Logs), (of("traces"), Signal::Traces), (of("metrics"), Signal::Metrics)] {
                if !part.is_empty() {
                    self.send_part(o, &part, signal).await;
                }
            }
            return;
        }
        self.send_part(o, batch, Signal::Logs).await;
    }

    async fn send_part(&self, o: &OpenedDrain, batch: &[Value], signal: Signal) {
        let drain = o.drain.id.as_str();
        // One id for the batch, its retries too: a receiver dedupes by it.
        let delivery = format!("lgb_{}", crate::model::random(12));
        for (i, wait) in [0u64, 1, 5].into_iter().enumerate() {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            match self.post_batch(o, batch, &delivery, signal).await {
                Ok(None) => return,
                Ok(Some(code)) if (200..300).contains(&code) => return,
                Ok(Some(code)) if i == 2 => tracing::warn!(drain, status = code, lines = batch.len(), "apps: a log drain refused a batch; it's dropped"),
                Err(e) if i == 2 => tracing::warn!(drain, error = %e, lines = batch.len(), "apps: a log drain didn't answer; the batch is dropped"),
                _ => {}
            }
        }
    }

    /// A batch sent once in its drain's format: an HTTP drain's status (signed as webhooks are,
    /// with the drain's own headers), `None` for a syslog server that took it, or why it didn't go
    /// (never the URL).
    async fn post_batch(&self, o: &OpenedDrain, batch: &[Value], delivery: &str, signal: Signal) -> std::result::Result<Option<u16>, String> {
        let mut url = o.url.clone();
        let (body, content_type) = match o.drain.format.as_str() {
            "syslog" => return self.send_syslog(&o.url, batch).await.map(|()| None),
            "s3" => return self.put_s3(o, batch, delivery).await.map(Some),
            "otlp" => {
                url = signal_url(&o.url, signal);
                let v = match signal {
                    Signal::Logs => otlp_logs(batch),
                    Signal::Traces => otlp_traces(batch),
                    Signal::Metrics => otlp_metrics(batch),
                };
                (serde_json::to_vec(&v).expect("JSON"), "application/json")
            }
            _ => {
                let mut body = Vec::new();
                for l in batch {
                    serde_json::to_writer(&mut body, l).expect("JSON");
                    body.push(b'\n');
                }
                (body, "application/x-ndjson")
            }
        };
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
            .post(&url)
            .header("content-type", content_type)
            .header("x-roll-delivery", delivery)
            .header("x-roll-timestamp", ts)
            .header("x-roll-signature-256", format!("sha256={sig}"));
        for (k, v) in &o.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        match req.body(body).send().await {
            Ok(r) => Ok(Some(r.status().as_u16())),
            Err(e) => Err(e.without_url().to_string()),
        }
    }

    /// An `s3` drain's batch: one NDJSON object, `<prefix>/YYYY/MM/DD/HH/<time>-<delivery>.ndjson`,
    /// PUT to its bucket signed with SigV4 (its sealed keys; roll storage's buckets, AWS's,
    /// any S3-compatible), through the public-only client. Its status, or why it didn't go.
    async fn put_s3(&self, o: &OpenedDrain, batch: &[Value], delivery: &str) -> std::result::Result<u16, String> {
        use sha2::Digest;
        let key = |k: &str| o.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        let (Some(access), Some(secret)) = (key(S3_KEYS[0]), key(S3_KEYS[1])) else { return Err("the drain has no S3 keys".into()) };
        let region = key(S3_KEYS[2]).unwrap_or_else(|| "us-east-1".into());
        let mut body = Vec::new();
        for l in batch {
            serde_json::to_writer(&mut body, l).expect("JSON");
            body.push(b'\n');
        }
        let now = chrono::Utc::now();
        let url = format!("{}/{}/{}-{delivery}.ndjson", o.url.trim_end_matches('/'), now.format("%Y/%m/%d/%H"), now.format("%Y%m%dT%H%M%SZ"));
        let parsed = url::Url::parse(&url).map_err(|e| e.to_string())?;
        let host = match parsed.port() {
            Some(p) => format!("{}:{p}", parsed.host_str().unwrap_or_default()),
            None => parsed.host_str().unwrap_or_default().to_string(),
        };
        let hash = hex::encode(sha2::Sha256::digest(&body));
        let date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let unsigned = [("host", host.as_str()), ("x-amz-content-sha256", hash.as_str()), ("x-amz-date", date.as_str())];
        let identity: aws_smithy_runtime_api::client::identity::Identity = aws_credential_types::Credentials::new(access, secret, None, None, "roll-drain").into();
        let params: aws_sigv4::http_request::SigningParams = aws_sigv4::sign::v4::SigningParams::builder()
            .identity(&identity)
            .region(&region)
            .name("s3")
            .time(now.into())
            .settings(aws_sigv4::http_request::SigningSettings::default())
            .build()
            .map_err(|e| e.to_string())?
            .into();
        let signable = aws_sigv4::http_request::SignableRequest::new("PUT", url.as_str(), unsigned.iter().copied(), aws_sigv4::http_request::SignableBody::Precomputed(hash.clone())).map_err(|e| e.to_string())?;
        let (instructions, _) = aws_sigv4::http_request::sign(signable, &params).map_err(|e| e.to_string())?.into_parts();
        let mut req = self.notify_client.put(&url).header("content-type", "application/x-ndjson").header("x-amz-content-sha256", &hash).header("x-amz-date", &date);
        for (k, v) in instructions.headers() {
            req = req.header(k, v);
        }
        match req.body(body).send().await {
            Ok(r) => Ok(r.status().as_u16()),
            Err(e) => Err(e.without_url().to_string()),
        }
    }

    /// A syslog drain's URL: `syslog+tls://host:port` (`syslog://`, plain TCP, only where private
    /// targets are allowed: local development and tests). Its host, port and whether it's TLS.
    fn syslog_target(&self, url: &str) -> Result<(String, u16, bool)> {
        let bad = |why: &str| Err(Error::BadRequest(format!("url: {why}")));
        let u = url::Url::parse(url).map_err(|e| Error::BadRequest(format!("url: {e}")))?;
        let tls = match u.scheme() {
            "syslog+tls" => true,
            "syslog" if self.cfg.allow_private => false,
            _ => return bad("a syslog drain is syslog+tls://host:port"),
        };
        let Some(host) = u.host_str().map(str::to_ascii_lowercase) else { return bad("no host") };
        let Some(port) = u.port() else { return bad("a syslog drain names its port") };
        if !self.cfg.allow_private && crate::net::literal_private(&host) {
            return bad("a private address");
        }
        if (!u.path().is_empty() && u.path() != "/") || u.query().is_some() || !u.username().is_empty() {
            return bad("a syslog drain is only syslog+tls://host:port");
        }
        Ok((host, port, tls))
    }

    /// A batch as RFC 5424 messages on one connection, each framed by its length (RFC 5425).
    async fn send_syslog(&self, url: &str, batch: &[Value]) -> std::result::Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let (host, port, tls) = self.syslog_target(url).map_err(|e| e.to_string())?;
        let mut frames = Vec::new();
        for l in batch {
            let m = syslog_message(l);
            frames.extend_from_slice(format!("{} ", m.len()).as_bytes());
            frames.extend_from_slice(m.as_bytes());
        }
        let host_ = host.clone();
        let send = async move {
            // Public addresses only, as the HTTP drains' resolver allows (IP literals checked above).
            let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host_.as_str(), port)).await.map_err(|e| format!("{host_}: {e}"))?.collect();
            let addr = addrs.into_iter().find(|a| self.cfg.allow_private || !crate::model::private_ip(a.ip())).ok_or_else(|| format!("{host_} resolves to a private address"))?;
            let tcp = tokio::net::TcpStream::connect(addr).await.map_err(|e| format!("{host_}: {e}"))?;
            if tls {
                let name = tokio_rustls::rustls::pki_types::ServerName::try_from(host_.clone()).map_err(|e| format!("{host_}: {e}"))?;
                let mut s = tokio_rustls::TlsConnector::from(syslog_tls()).connect(name, tcp).await.map_err(|e| format!("{host_}: TLS: {e}"))?;
                s.write_all(&frames).await.map_err(|e| e.to_string())?;
                s.shutdown().await.map_err(|e| e.to_string())
            } else {
                let mut s = tcp;
                s.write_all(&frames).await.map_err(|e| e.to_string())?;
                s.shutdown().await.map_err(|e| e.to_string())
            }
        };
        tokio::time::timeout(Duration::from_secs(10), send).await.map_err(|_| format!("{host}: timed out"))?
    }
}

/// The TLS settings syslog drains connect with: the system's roots, ring, TLS 1.2 and up.
/// An `s3` drain: its URL names a bucket and maybe a prefix (`https://host/bucket[/prefix]`,
/// path-style, letters, digits and `-_./` only), and its headers are its keys and nothing else.
fn check_s3(url: &str, headers: &[(String, String)]) -> Result<()> {
    let u = url::Url::parse(url).map_err(|e| Error::BadRequest(format!("url: {e}")))?;
    let path = u.path().trim_matches('/');
    if path.is_empty() || u.query().is_some() || !path.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/')) || path.contains("//") {
        return Err(Error::BadRequest("url: an s3 drain's is https://<endpoint>/<bucket>[/<prefix>] (path-style; letters, digits, - _ . /)".into()));
    }
    let names: Vec<String> = headers.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
    if let Some(n) = names.iter().find(|n| !S3_KEYS.contains(&n.as_str())) {
        return Err(Error::BadRequest(format!("headers: an s3 drain takes only {} ({n} isn't one)", S3_KEYS.join(", "))));
    }
    if !names.iter().any(|n| n == S3_KEYS[0]) || !names.iter().any(|n| n == S3_KEYS[1]) {
        return Err(Error::BadRequest(format!("headers: an s3 drain needs {} and {}", S3_KEYS[0], S3_KEYS[1])));
    }
    Ok(())
}

fn syslog_tls() -> Arc<tokio_rustls::rustls::ClientConfig> {
    use tokio_rustls::rustls;
    static CONFIG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            for c in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(c);
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            Arc::new(rustls::ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().expect("ring's TLS versions").with_root_certificates(roots).with_no_client_auth())
        })
        .clone()
}

/// A drain's line as an RFC 5424 message: facility user, `err` for stderr and `info` otherwise;
/// the service as HOSTNAME, its environment as APP-NAME, the instance as PROCID, the source
/// (`runtime`, `http`, `build`) as MSGID.
fn syslog_message(l: &Value) -> String {
    // A header field: printable ASCII without spaces, at most `max` characters, `-` when empty.
    let field = |v: &Value, max: usize| -> String {
        let s: String = v.as_str().unwrap_or("").chars().filter(|c| c.is_ascii_graphic()).take(max).collect();
        if s.is_empty() { "-".into() } else { s }
    };
    let severity = if l["stream"] == "stderr" { 3 } else { 6 };
    let t = l["t"].as_i64().and_then(chrono::DateTime::from_timestamp_millis).unwrap_or_else(chrono::Utc::now);
    // The message itself is one line: a newline would end it for most receivers.
    let text = l["text"].as_str().unwrap_or("").replace(['\r', '\n'], " ");
    format!(
        "<{}>1 {} {} {} {} {} - {}",
        8 + severity,
        t.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
        field(&l["service_name"], 255),
        field(&l["environment"], 48),
        field(&l["instance"], 128),
        field(&l["source"], 32),
        text
    )
}

/// A batch as an OTLP/HTTP JSON `ExportLogsServiceRequest`: a resource per service and
/// environment (`service.name`, `deployment.environment.name`, `roll.service.id`), each line a
/// log record with its stream, source, instance and deployment as attributes.
/// An OTLP drain's address for `signal`: its URL is the logs' (`…/v1/logs`), traces and metrics
/// beside it (`…/v1/traces`, `…/v1/metrics`); any other URL gets the signal's path after it.
fn signal_url(url: &str, signal: Signal) -> String {
    let path = match signal {
        Signal::Logs => return url.to_string(),
        Signal::Traces => "v1/traces",
        Signal::Metrics => "v1/metrics",
    };
    match url.strip_suffix("v1/logs") {
        Some(base) => format!("{base}{path}"),
        None => format!("{}/{path}", url.trim_end_matches('/')),
    }
}

/// A resource per service and environment, as [`otlp_logs`] has them.
fn otlp_resource(batch: &[Value], service: &str, env: &str) -> Value {
    let attr = |k: &str, v: &Value| v.as_str().filter(|s| !s.is_empty()).map(|s| json!({ "key": k, "value": { "stringValue": s } }));
    let name = batch.iter().find(|l| l["service"] == service && l["environment"] == env).map(|l| l["service_name"].clone()).unwrap_or(Value::Null);
    let attributes: Vec<Value> = [attr("service.name", &name), attr("deployment.environment.name", &json!(env)), attr("roll.service.id", &json!(service))].into_iter().flatten().collect();
    json!({ "attributes": attributes })
}

/// Lines grouped by their service and environment, each parsed as a `T` (a span, a point).
fn by_resource<T: serde::de::DeserializeOwned>(batch: &[Value]) -> Vec<((String, String), Vec<T>)> {
    let mut groups: Vec<((String, String), Vec<T>)> = Vec::new();
    for l in batch {
        let Some(item) = l["text"].as_str().and_then(|t| serde_json::from_str::<T>(t).ok()) else { continue };
        let key = (l["service"].as_str().unwrap_or("").to_string(), l["environment"].as_str().unwrap_or("").to_string());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, items)) => items.push(item),
            None => groups.push((key, vec![item])),
        }
    }
    groups
}

fn otlp_attrs(attrs: &[(String, String)]) -> Vec<Value> {
    attrs.iter().map(|(k, v)| json!({ "key": k, "value": { "stringValue": v } })).collect()
}

/// Spans (the `trace:` log's) as OTLP/HTTP JSON traces.
fn otlp_traces(batch: &[Value]) -> Value {
    let resource_spans: Vec<Value> = by_resource::<crate::otlp::Span>(batch)
        .into_iter()
        .map(|((service, env), spans)| {
            let spans: Vec<Value> = spans
                .iter()
                .map(|s| {
                    let mut v = json!({ "traceId": s.trace, "spanId": s.span, "name": s.name, "startTimeUnixNano": s.start.to_string(), "endTimeUnixNano": s.end.to_string(), "attributes": otlp_attrs(&s.attrs) });
                    if !s.parent.is_empty() {
                        v["parentSpanId"] = json!(s.parent);
                    }
                    if s.error {
                        v["status"] = json!({ "code": 2 });
                    }
                    v
                })
                .collect();
            json!({ "resource": otlp_resource(batch, &service, &env), "scopeSpans": [{ "scope": { "name": "roll-apps" }, "spans": spans }] })
        })
        .collect();
    json!({ "resourceSpans": resource_spans })
}

/// Data points (the `metric:` log's) as OTLP/HTTP JSON metrics: a gauge, a sum (cumulative and
/// monotonic when it counts up) or a histogram's sum and count.
fn otlp_metrics(batch: &[Value]) -> Value {
    let resource_metrics: Vec<Value> = by_resource::<crate::otlp::Point>(batch)
        .into_iter()
        .map(|((service, env), points)| {
            let metrics: Vec<Value> = points
                .iter()
                .map(|p| {
                    let point = json!({ "timeUnixNano": p.time.to_string(), "asDouble": p.value, "attributes": otlp_attrs(&p.attrs) });
                    let mut m = json!({ "name": p.name, "unit": p.unit });
                    match p.kind.as_str() {
                        "sum" => m["sum"] = json!({ "dataPoints": [point], "aggregationTemporality": 2, "isMonotonic": p.counter }),
                        "histogram" => m["histogram"] = json!({ "dataPoints": [{ "timeUnixNano": p.time.to_string(), "sum": p.value, "count": p.count.unwrap_or(0).to_string(), "attributes": otlp_attrs(&p.attrs) }], "aggregationTemporality": 2 }),
                        _ => m["gauge"] = json!({ "dataPoints": [point] }),
                    }
                    m
                })
                .collect();
            json!({ "resource": otlp_resource(batch, &service, &env), "scopeMetrics": [{ "scope": { "name": "roll-apps" }, "metrics": metrics }] })
        })
        .collect();
    json!({ "resourceMetrics": resource_metrics })
}

fn otlp_logs(batch: &[Value]) -> Value {
    let attr = |k: &str, v: &Value| v.as_str().filter(|s| !s.is_empty()).map(|s| json!({ "key": k, "value": { "stringValue": s } }));
    let mut groups: Vec<((String, String), Vec<Value>)> = Vec::new();
    for l in batch {
        let key = (l["service"].as_str().unwrap_or("").to_string(), l["environment"].as_str().unwrap_or("").to_string());
        let nanos = (l["t"].as_i64().unwrap_or(0).max(0) as u128 * 1_000_000).to_string();
        let attributes: Vec<Value> = [attr("log.iostream", &l["stream"]), attr("roll.source", &l["source"]), attr("roll.instance", &l["instance"]), attr("roll.deployment", &l["deployment"])].into_iter().flatten().collect();
        let record = json!({ "timeUnixNano": nanos, "observedTimeUnixNano": nanos, "body": { "stringValue": l["text"].as_str().unwrap_or("") }, "attributes": attributes });
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, records)) => records.push(record),
            None => groups.push((key, vec![record])),
        }
    }
    let resource_logs: Vec<Value> = groups
        .into_iter()
        .map(|((service, env), records)| {
            let first = batch.iter().find(|l| l["service"] == service.as_str() && l["environment"] == env.as_str());
            let name = first.map(|l| l["service_name"].clone()).unwrap_or(Value::Null);
            let attributes: Vec<Value> = [attr("service.name", &name), attr("deployment.environment.name", &json!(env)), attr("roll.service.id", &json!(service))].into_iter().flatten().collect();
            json!({ "resource": { "attributes": attributes }, "scopeLogs": [{ "scope": { "name": "roll-apps" }, "logRecords": records }] })
        })
        .collect();
    json!({ "resourceLogs": resource_logs })
}

/// `line` onto drain `id`'s queue: past [`BUFFER`], its oldest line goes (counted).
fn enqueue(queues: &mut HashMap<String, Queue>, id: &str, project: &str, line: Value) {
    let q = queues.entry(id.to_string()).or_insert_with(|| Queue::new(project));
    if q.lines.is_empty() {
        q.first = Instant::now();
    }
    if q.lines.len() == BUFFER {
        q.lines.pop_front();
        q.dropped += 1;
    }
    q.lines.push_back(line);
}

/// One drain's lines waiting for their batch.
struct Queue {
    /// Its drain's project.
    project: String,
    lines: VecDeque<Value>,
    /// When the oldest waiting line came (a batch goes after LINGER even if it's short).
    first: Instant,
    /// A batch is on its way: the next waits for it, so batches arrive in order.
    out: bool,
    dropped: u64,
}

impl Queue {
    fn new(project: &str) -> Queue {
        Queue { project: project.to_string(), lines: VecDeque::new(), first: Instant::now(), out: false, dropped: 0 }
    }
}
