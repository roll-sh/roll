//! Log drains (docs/apps/README.md, "Log drains"; Render's log streams, Fly's and Heroku's
//! drains): a project's services' runtime output, as it's written, POSTed in batches of NDJSON to
//! HTTPS endpoints the project names. Each node sends what its own instances write (the log tap):
//! nothing crosses nodes. Batches are signed as webhooks are (`X-Roll-Signature-256` over
//! `<timestamp>.<body>`, `X-Roll-Delivery` the batch's id). Best effort, as drains are: a batch
//! is tried three times, then dropped and counted; a drain that can't keep up loses its oldest
//! lines, never a writer's time or the node's memory.

use crate::model::LogDrain;
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
        if let Some(k) = o.keys().find(|k| k.as_str() != "url") {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
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
        let d = self.db(move |s| s.add_drain(&p, &host, &url, &s2)).await?;
        let mut v = serde_json::to_value(&d).expect("JSON");
        v["secret"] = json!(secret);
        Ok(v)
    }

    pub async fn drains(self: &Arc<Self>, project: &str) -> Result<Vec<LogDrain>> {
        let p = project.to_string();
        self.db(move |s| s.drains(&p)).await
    }

    pub async fn delete_drain(self: &Arc<Self>, project: &str, id: &str) -> Result<()> {
        let (p, i) = (project.to_string(), id.to_string());
        self.db(move |s| s.delete_drain(&p, &i)).await
    }

    /// Every node: read the log tap, batch each project's lines per drain, and send them.
    pub(crate) async fn drainer(self: Arc<Self>) {
        let mut tap = self.logs.tap(BUFFER);
        // service → (project, name), and project → its drains, each read again after REFRESH.
        let mut services: HashMap<String, (Instant, Option<(String, String)>)> = HashMap::new();
        let mut drains: HashMap<String, (Instant, Vec<Opened>)> = HashMap::new();
        let mut queues: HashMap<String, Queue> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tokio::select! {
                got = tap.recv() => {
                    let Some((key, line)) = got else { return };
                    let Some((service, env)) = key.strip_prefix("env:").and_then(|k| k.split_once('/')) else { continue };
                    let (service, env) = (service.to_string(), env.to_string());
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
                    for (d, _, _) in &drains[&project].1 {
                        let q = queues.entry(d.id.clone()).or_insert_with(Queue::new);
                        if q.lines.is_empty() {
                            q.first = Instant::now();
                        }
                        if q.lines.len() == BUFFER {
                            q.lines.pop_front();
                            q.dropped += 1;
                        }
                        q.lines.push_back(json!({ "t": line.t, "service": service, "service_name": name, "environment": env, "instance": line.instance, "deployment": line.deployment, "stream": line.stream, "text": line.text }));
                    }
                }
                _ = tick.tick() => {
                    for (_, list) in drains.values() {
                        for (d, url, secret) in list {
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
                            let (this, id, url, secret) = (self.clone(), d.id.clone(), url.clone(), secret.clone());
                            let done = self.drain_done.clone();
                            tokio::spawn(async move {
                                this.send_batch(&id, &url, &secret, &batch).await;
                                done.lock().push(id);
                            });
                        }
                    }
                    for id in std::mem::take(&mut *self.drain_done.lock()) {
                        if let Some(q) = queues.get_mut(&id) {
                            q.out = false;
                        }
                    }
                    // Drains deleted (or of projects no longer seen) stop holding lines.
                    queues.retain(|id, _| drains.values().any(|(_, l)| l.iter().any(|(d, _, _)| &d.id == id)));
                }
                _ = self.stop.cancelled() => return,
            }
        }
    }

    /// One batch, tried three times (1 s, then 5 s apart), then dropped.
    async fn send_batch(&self, drain: &str, url: &str, secret: &str, batch: &[Value]) {
        let mut body = Vec::new();
        for l in batch {
            serde_json::to_writer(&mut body, l).expect("JSON");
            body.push(b'\n');
        }
        let delivery = format!("lgb_{}", crate::model::random(12));
        for (i, wait) in [0u64, 1, 5].into_iter().enumerate() {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            let ts = (code_sql::now_ms() / 1000).to_string();
            let sig = {
                use hmac::Mac;
                let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("any key");
                mac.update(ts.as_bytes());
                mac.update(b".");
                mac.update(&body);
                mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>()
            };
            let sent = self
                .notify_client
                .post(url)
                .header("content-type", "application/x-ndjson")
                .header("x-roll-delivery", &delivery)
                .header("x-roll-timestamp", ts)
                .header("x-roll-signature-256", format!("sha256={sig}"))
                .body(body.clone())
                .send()
                .await;
            match sent {
                Ok(r) if r.status().is_success() => return,
                Ok(r) if i == 2 => tracing::warn!(drain, status = r.status().as_u16(), lines = batch.len(), "apps: a log drain refused a batch; it's dropped"),
                Err(e) if i == 2 => tracing::warn!(drain, error = %e.without_url(), lines = batch.len(), "apps: a log drain didn't answer; the batch is dropped"),
                _ => {}
            }
        }
    }
}

/// A drain with its URL and signing secret opened.
type Opened = (LogDrain, String, String);

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
