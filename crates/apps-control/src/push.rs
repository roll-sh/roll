//! A roll stream pushed to a web service (`run.push`, Cloud Run's Pub/Sub push): each record of
//! a topic is POSTed to a path of one environment through its own proxy (waking it), and the
//! group's offset is committed on a 2xx, so delivery is at least once and in order within a
//! partition. A failed POST is tried again after 1 s, doubling to a minute; with a dead-letter
//! topic, a record that failed `max_attempts` times goes there and its partition moves on.
//!
//! One delivery loop runs per rule on the service's owner, while the environment has a live
//! deployment and isn't paused or in maintenance, and only for a stream on the service's own
//! project ([`Apps::stream_followable`], as `run.scale` reads one). The pass that starts loops
//! (with the schedules', every few seconds) stops one whose rule changed or went.

use crate::integrations::StreamRecord;
use crate::logs::Stream;
use crate::model::Push;
use crate::Apps;
use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Request, header};
use base64::Engine;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// How long a read waits for a record.
const WAIT_MS: u64 = 20_000;
/// Records a read takes at most.
const BATCH: usize = 100;
/// The group consumer roll reads as (one owner reads a service's pushes).
const CONSUMER: &str = "roll-apps";
/// The longest wait between tries.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// What of an answer's body is read (and dropped) before the next.
const ANSWER_BYTES: usize = 64 * 1024;

/// The delivery loops running on this node.
#[derive(Default)]
pub struct Pushers {
    /// Rule key → (what it runs with, the loop).
    running: parking_lot::Mutex<HashMap<String, (String, tokio::task::AbortHandle)>>,
    /// Rules said to be off (said once, until they're on again).
    off: parking_lot::Mutex<std::collections::HashSet<String>>,
}

impl Drop for Pushers {
    fn drop(&mut self) {
        for (_, (_, h)) in self.running.lock().drain() {
            h.abort();
        }
    }
}

/// One rule's loop as a pass wants it.
struct Wanted {
    /// What it runs with (a change starts it again).
    digest: String,
    rule: Push,
    service: String,
    env: String,
    host: String,
    secret: Option<String>,
}

/// The wait before try `attempt + 1` (1 s, doubling, a minute at most).
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(6)).min(MAX_BACKOFF)
}

/// The POST for one record: its value as the body, where it came from in `X-Roll-*` headers,
/// its own headers as `X-Roll-Header-<name>` (those HTTP can carry), the environment's
/// `CRON_SECRET` as a bearer token, as schedules' calls carry it.
pub fn request(rule: &Push, host: &str, r: &StreamRecord, attempt: u32, secret: Option<&str>) -> Option<Request<Body>> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let ctype = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).and_then(|(_, v)| HeaderValue::from_bytes(v).ok()).unwrap_or(HeaderValue::from_static("application/octet-stream"));
    let mut b = Request::builder()
        .method(axum::http::Method::POST)
        .uri(&rule.path)
        .header(header::HOST, host)
        .header(header::USER_AGENT, "roll-push/1.0")
        .header(header::CONTENT_TYPE, ctype)
        .header("x-roll-stream", &rule.stream)
        .header("x-roll-topic", &rule.topic)
        .header("x-roll-partition", r.partition.to_string())
        .header("x-roll-offset", r.offset.to_string())
        .header("x-roll-timestamp", r.timestamp.to_string())
        .header("x-roll-attempt", attempt.to_string());
    if let Some(k) = &r.key {
        b = b.header("x-roll-key", b64.encode(k));
    }
    for (k, v) in &r.headers {
        if k.eq_ignore_ascii_case("content-type") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(format!("x-roll-header-{}", k.to_ascii_lowercase()).as_bytes()), HeaderValue::from_bytes(v)) {
            b = b.header(name, value);
        }
    }
    if let Some(s) = secret {
        b = b.header(header::AUTHORIZATION, format!("Bearer {s}"));
    }
    let mut req = b.body(Body::from(r.value.clone().unwrap_or_default())).ok()?;
    req.extensions_mut().insert(crate::public::Scheduled);
    Some(req)
}

impl Apps {
    /// Whether a service of `project` may read `stream`: it's on the project's canvas, and the
    /// project is the operator's (anyone can put any reference on their own canvas, and streams
    /// don't know their owners yet).
    pub(crate) fn stream_followable(&self, project: &str, stream: &str) -> std::result::Result<(), String> {
        let p = self.deps.workspace.as_ref().and_then(|ws| ws.get(roll_workspace::Scope::All, project).ok());
        let placed = p.as_ref().is_some_and(|p| p.resources.iter().any(|r| r.kind == roll_workspace::Kind::Stream && r.r#ref == stream));
        if !placed {
            return Err(format!("stream {stream} isn't on this service's project"));
        }
        if !p.is_some_and(|p| p.owner.is_empty()) {
            return Err("a person's project can't follow a stream until streams know their owners".into());
        }
        Ok(())
    }

    /// Start the delivery loops owned services' rules want, and stop the rest.
    pub(crate) async fn push_pass(self: &Arc<Self>, services: &[crate::store::ServiceRow]) {
        let Ok(all_envs) = self.db(|s| s.all_environments()).await else { return };
        let mut wanted: HashMap<String, Wanted> = HashMap::new();
        for svc in services.iter().filter(|s| self.owns(&s.id) && !self.capped(&s.id)) {
            for e in all_envs.iter().filter(|e| e.service == svc.id && e.active.is_some() && !e.ephemeral && !e.paused && e.maintenance.is_none()) {
                // What the live deployment runs with (roll.toml's rules are in its template).
                let (run, secret) = match self.lives.lock().get(&crate::env_key(&svc.id, &e.name)).and_then(|l| l.template()) {
                    Some(t) => (t.run.clone(), t.spec.vars.iter().find(|(k, _)| k == "CRON_SECRET").map(|(_, v)| v.clone())),
                    None => continue,
                };
                for rule in run.push.iter().filter(|p| p.environment() == e.name) {
                    let key = format!("{}/{}/{}/{}/{}", svc.id, e.name, rule.stream, rule.topic, rule.group);
                    let log = || self.logs.sink(format!("env:{}/{}", svc.id, e.name), Stream::System);
                    if let Err(why) = self.stream_followable(&svc.project, &rule.stream) {
                        if self.pushers.off.lock().insert(key.clone()) {
                            log().line(format!("push {} of {} is off: {why}", rule.topic, rule.stream));
                        }
                        continue;
                    }
                    self.pushers.off.lock().remove(&key);
                    let host = url::Url::parse(&self.url(&e.slug)).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
                    // A changed rule, address or secret starts the loop again with it.
                    let digest = {
                        use sha2::Digest;
                        hex::encode(sha2::Sha256::digest(format!("{}|{host}|{}", serde_json::to_string(rule).unwrap_or_default(), secret.as_deref().unwrap_or_default())))
                    };
                    wanted.insert(key, Wanted { digest, rule: rule.clone(), service: svc.id.clone(), env: e.name.clone(), host, secret: secret.clone() });
                }
            }
        }
        let mut running = self.pushers.running.lock();
        running.retain(|k, (digest, h)| {
            let keep = wanted.get(k).is_some_and(|w| &w.digest == digest) && !h.is_finished();
            if !keep {
                h.abort();
            }
            keep
        });
        for (key, w) in wanted {
            if running.contains_key(&key) {
                continue;
            }
            let this = self.clone();
            let h = tokio::spawn(async move { this.deliver(w.rule, w.service, w.env, w.host, w.secret).await }).abort_handle();
            running.insert(key, (w.digest, h));
        }
    }

    /// One rule's loop: read, POST each record until it's taken (or dead-lettered), commit.
    async fn deliver(self: Arc<Self>, rule: Push, service: String, env: String, host: String, secret: Option<String>) {
        let log = self.logs.sink(format!("env:{service}/{env}"), Stream::System);
        let resources = self.deps.resources.clone();
        let timeout = Duration::from_secs(rule.timeout_s.unwrap_or(30) as u64);
        let mut read_failures = 0u32;
        loop {
            let records = match resources.read_group(&rule.stream, &rule.topic, &rule.group, CONSUMER, WAIT_MS, BATCH).await {
                Ok(r) => {
                    read_failures = 0;
                    r
                }
                Err(e) => {
                    read_failures += 1;
                    if read_failures == 1 {
                        log.line(format!("push {}: reading {}: {e:#}", rule.topic, rule.stream));
                    }
                    tokio::time::sleep(backoff(read_failures)).await;
                    continue;
                }
            };
            for r in records {
                let mut attempt = 1;
                loop {
                    let Some(req) = request(&rule, &host, &r, attempt, secret.as_deref()) else { break };
                    let status = tokio::time::timeout(timeout, async {
                        let resp = crate::public::proxy(self.clone(), &service, &env, None, req).await;
                        let s = resp.status().as_u16();
                        // Read (and dropped) so its instance's lease and meter end with it.
                        let _ = http_body_util::BodyExt::collect(http_body_util::Limited::new(resp.into_body(), ANSWER_BYTES)).await;
                        s
                    })
                    .await;
                    let why = match status {
                        Ok(s) if (200..300).contains(&s) => None,
                        Ok(s) => Some(format!("answered {s}")),
                        Err(_) => Some(format!("didn't answer within {} s", timeout.as_secs())),
                    };
                    let Some(why) = why else {
                        self.commit_push(&rule, &r, &log).await;
                        break;
                    };
                    if let Some(dl) = rule.dead_letter.as_deref().filter(|_| attempt >= rule.max_attempts.unwrap_or(5)) {
                        let mut dead = r.clone();
                        dead.headers.push(("x-roll-error".into(), format!("{} {why} after {attempt} tries", rule.path).into_bytes()));
                        dead.headers.push(("x-roll-source".into(), format!("{}/{}/{}", rule.topic, r.partition, r.offset).into_bytes()));
                        match resources.produce(&rule.stream, dl, &dead).await {
                            Ok(()) => {
                                log.line(format!("push {}: offset {} of partition {} {why} {attempt} times; sent to {dl}", rule.topic, r.offset, r.partition));
                                self.commit_push(&rule, &r, &log).await;
                                break;
                            }
                            Err(e) => log.line(format!("push {}: sending offset {} to {dl}: {e:#}", rule.topic, r.offset)),
                        }
                    }
                    let wait = backoff(attempt);
                    // Said on the first failure and then each minute's worth, not every try.
                    if attempt == 1 || wait == MAX_BACKOFF {
                        log.line(format!("push {}: {} {why} for offset {} of partition {}; trying again in {} s", rule.topic, rule.path, r.offset, r.partition, wait.as_secs()));
                    }
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
            }
        }
    }

    /// The group past `r`: retried until the stream takes it (a lost commit delivers it again).
    async fn commit_push(&self, rule: &Push, r: &StreamRecord, log: &crate::logs::LogSink) {
        for attempt in 1.. {
            match self.deps.resources.commit(&rule.stream, &rule.topic, &rule.group, CONSUMER, r.partition, r.offset + 1).await {
                Ok(()) => return,
                Err(e) if attempt == 1 => log.line(format!("push {}: committing offset {}: {e:#}", rule.topic, r.offset)),
                Err(_) => {}
            }
            tokio::time::sleep(backoff(attempt)).await;
            if attempt >= 10 {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tries_back_off_to_a_minute() {
        let waits: Vec<u64> = (1..=9).map(|a| backoff(a).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[test]
    fn a_record_is_its_value_with_where_it_came_from() {
        let rule = Push { stream: "st_aaaaaaaaaaaa".into(), topic: "orders".into(), group: "g".into(), path: "/events".into(), environment: None, dead_letter: None, max_attempts: None, timeout_s: None };
        let r = StreamRecord { partition: 2, offset: 41, timestamp: 7, key: Some(b"k1".to_vec()), value: Some(b"{\"id\":1}".to_vec()), headers: vec![("content-type".into(), b"application/json".to_vec()), ("trace id".into(), b"x".to_vec()), ("tenant".into(), b"acme".to_vec())] };
        let req = request(&rule, "web.roll.test", &r, 3, Some("s3cret")).unwrap();
        let h = req.headers();
        assert_eq!((req.method().as_str(), req.uri().path()), ("POST", "/events"));
        assert_eq!(h["content-type"], "application/json");
        assert_eq!((h["x-roll-partition"].to_str().unwrap(), h["x-roll-offset"].to_str().unwrap(), h["x-roll-attempt"].to_str().unwrap()), ("2", "41", "3"));
        assert_eq!((h["x-roll-key"].to_str().unwrap(), h["x-roll-header-tenant"].to_str().unwrap()), ("azE=", "acme"));
        assert_eq!(h["authorization"], "Bearer s3cret");
        assert!(!h.keys().any(|k| k.as_str().contains("trace")), "a name HTTP can't carry is left out");
    }
}
