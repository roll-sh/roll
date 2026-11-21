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
/// The prefix of the group consumers roll reads as: a fresh one each time a loop starts or a read
/// fails, the one before it left. The broker moves a consumer's position past what it handed out,
/// so a consumer whose records may not all have been delivered is never read with again: a new
/// one starts from the group's committed offset.
const CONSUMER: &str = "roll-apps-";
/// The longest wait between tries.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// How long a loop works on what one read handed it: under the 60 s lease rolld reads with
/// (`RollResources::read_group`), so a record refused past it is read again from the committed
/// offset by a fresh consumer, never after a sibling took the partition and delivered past it.
const HOLD: Duration = Duration::from_secs(45);
/// What of an answer's body is read (and dropped) before the next.
const ANSWER_BYTES: usize = 64 * 1024;

/// The delivery loops running on this node.
#[derive(Default)]
pub struct Pushers {
    /// Rule key → (what it runs with, the loop).
    running: parking_lot::Mutex<HashMap<String, (String, tokio::task::AbortHandle)>>,
    /// Rules said to be off (said once, until they're on again).
    off: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// Loop key → the consumer it reads as, left when the next loop starts.
    consumers: parking_lot::Mutex<HashMap<String, String>>,
    /// Rule key → how its deliveries have gone since this node began them.
    stats: parking_lot::Mutex<HashMap<String, Stats>>,
}

/// How a rule's deliveries have gone on this node (`GET …/environments/{env}/push`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Stats {
    /// Records its app took (a 2xx).
    pub delivered: u64,
    /// Records sent to the dead-letter topic.
    pub dead_lettered: u64,
    /// When its app last took one (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_delivered_at: Option<i64>,
    /// Since when a record has been refused, while one is (unix ms), and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failing_since: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// One rule as it stands: its setting, how far its group is behind, and how its deliveries go.
#[derive(Clone, Debug, serde::Serialize)]
pub struct PushState {
    #[serde(flatten)]
    pub rule: Push,
    /// Records its group hasn't committed, every partition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lag: Option<u64>,
    /// Why nothing is delivered (a stream not on the project, the environment paused or not
    /// deployed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub off: Option<String>,
    #[serde(flatten)]
    pub stats: Stats,
}

/// Loops a rule runs at once by default (`max_in_flight`).
pub const IN_FLIGHT: u32 = 4;

/// A rule's key: the service, environment, stream, topic and group.
fn rule_key(service: &str, env: &str, rule: &Push) -> String {
    format!("{service}/{env}/{}/{}/{}", rule.stream, rule.topic, rule.group)
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
    project: String,
    env: String,
    host: String,
    secret: Option<String>,
    /// The environment's URL: an `oidc` rule's tokens name it as their audience.
    aud: String,
}

/// The wait before try `attempt + 1` (1 s, doubling, a minute at most).
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.saturating_sub(1).min(6)).min(MAX_BACKOFF)
}

/// CloudEvents attributes roll sets itself: a record's own header never takes their place.
const CE_OURS: [&str; 11] = ["specversion", "id", "source", "type", "time", "subject", "datacontenttype", "dataschema", "partitionkey", "rollpartition", "rolloffset"];

/// The POST for one record, a CloudEvent in binary mode (as Eventarc delivers one; `X-Roll-*`
/// never reaches an app): its value the body, its `content-type` header the `Content-Type`
/// (else `application/octet-stream`), `ce-id` (`<topic>/<partition>/<offset>`: what to dedupe
/// on), `ce-source` (`/streams/<stream>/topics/<topic>`), `ce-type` `sh.roll.stream.record`,
/// `ce-time`, `ce-partitionkey` (its key, base64), `ce-rollpartition`, `ce-rolloffset`,
/// `ce-rollattempt`; its own headers as `ce-<name>` where CloudEvents allows the name (a-z, 0-9,
/// at most 20) and it isn't one of roll's; the environment's `CRON_SECRET` as a bearer token,
/// as schedules' calls carry it.
pub fn request(rule: &Push, host: &str, r: &StreamRecord, attempt: u32, secret: Option<&str>) -> Option<Request<Body>> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let ctype = r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).and_then(|(_, v)| HeaderValue::from_bytes(v).ok()).unwrap_or(HeaderValue::from_static("application/octet-stream"));
    let time = chrono::DateTime::from_timestamp_millis(r.timestamp).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut b = Request::builder()
        .method(axum::http::Method::POST)
        .uri(&rule.path)
        .header(header::HOST, host)
        .header(header::USER_AGENT, "roll-push/1.0")
        .header(header::CONTENT_TYPE, ctype)
        .header("ce-specversion", "1.0")
        .header("ce-id", format!("{}/{}/{}", rule.topic, r.partition, r.offset))
        .header("ce-source", format!("/streams/{}/topics/{}", rule.stream, rule.topic))
        .header("ce-type", "sh.roll.stream.record")
        .header("ce-time", time)
        .header("ce-rollpartition", r.partition.to_string())
        .header("ce-rolloffset", r.offset.to_string())
        .header("ce-rollattempt", attempt.to_string());
    if let Some(k) = &r.key {
        b = b.header("ce-partitionkey", b64.encode(k));
    }
    for (k, v) in &r.headers {
        let name = k.to_ascii_lowercase();
        let ce_name = (1..=20).contains(&name.len()) && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        if !ce_name || CE_OURS.contains(&name.as_str()) || name == "rollattempt" {
            continue;
        }
        if let (Ok(n), Ok(value)) = (HeaderName::from_bytes(format!("ce-{name}").as_bytes()), HeaderValue::from_bytes(v)) {
            b = b.header(n, value);
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
                    let key = rule_key(&svc.id, &e.name, rule);
                    let log = || self.logs.sink(format!("env:{}/{}", svc.id, e.name), Stream::System);
                    if let Err(why) = self.stream_followable(&svc.project, &rule.stream) {
                        if self.pushers.off.lock().insert(key.clone()) {
                            log().line(format!("push {} of {} is off: {why}", rule.topic, rule.stream));
                        }
                        continue;
                    }
                    self.pushers.off.lock().remove(&key);
                    let aud = self.url(&e.slug);
                    let host = url::Url::parse(&aud).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
                    // A changed rule, address or secret starts the loop again with it.
                    let digest = {
                        use sha2::Digest;
                        hex::encode(sha2::Sha256::digest(format!("{}|{host}|{}", serde_json::to_string(rule).unwrap_or_default(), secret.as_deref().unwrap_or_default())))
                    };
                    // A consumer a loop: the broker gives each a fair share of the partitions, so a
                    // record that's refused holds only its own partition's.
                    for i in 0..rule.max_in_flight.unwrap_or(IN_FLIGHT) {
                        let w = Wanted { digest: digest.clone(), rule: rule.clone(), service: svc.id.clone(), project: svc.project.clone(), env: e.name.clone(), host: host.clone(), secret: secret.clone(), aud: aud.clone() };
                        wanted.insert(format!("{key}#{i}"), w);
                    }
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
            let k = key.clone();
            let digest = w.digest.clone();
            let h = tokio::spawn(async move { this.deliver(k, w).await }).abort_handle();
            running.insert(key, (digest, h));
        }
    }

    /// One rule's loop: read, POST each record until it's taken (or dead-lettered), commit.
    async fn deliver(self: Arc<Self>, key: String, w: Wanted) {
        let Wanted { rule, service, project, env, host, secret, aud, .. } = w;
        let log = self.logs.sink(format!("env:{service}/{env}"), Stream::System);
        let resources = self.deps.resources.clone();
        let timeout = Duration::from_secs(rule.timeout_s.unwrap_or(30) as u64);
        let mut read_failures = 0u32;
        let mut token_failures = 0u32;
        let stats = rule_key(&service, &env, &rule);
        let note = |f: &dyn Fn(&mut Stats)| f(self.pushers.stats.lock().entry(stats.clone()).or_default());
        // A consumer of its own, the last loop's left (its partitions free at once, unless a read
        // of it is still out: then they wait out its lease).
        let mut consumer = self.new_consumer(&key, &rule).await;
        // A record's tries so far, kept when its batch is read again (its dead letter still
        // comes after `max_attempts`).
        let mut tries: HashMap<(i32, i64), u32> = HashMap::new();
        'read: loop {
            // Another node took the service: it delivers from here (this loop stops at its pass).
            if !self.owns(&service) {
                return;
            }
            let read_at = std::time::Instant::now();
            let records = match resources.read_group(&rule.stream, &rule.topic, &rule.group, &consumer, WAIT_MS, BATCH).await {
                Ok(r) => {
                    read_failures = 0;
                    r
                }
                Err(e) => {
                    read_failures += 1;
                    if read_failures == 1 {
                        log.line(format!("push {}: reading {}: {e:#}", rule.topic, rule.stream));
                    }
                    // The broker may have handed out records this read never got.
                    consumer = self.new_consumer(&key, &rule).await;
                    tokio::time::sleep(backoff(read_failures)).await;
                    continue;
                }
            };
            for r in records {
                let mut attempt = tries.remove(&(r.partition, r.offset)).unwrap_or(1);
                loop {
                    if !self.owns(&service) {
                        return;
                    }
                    // `oidc`: roll's token for the service (cached, a fresh one well before it ends),
                    // else the environment's CRON_SECRET.
                    let bearer = match rule.auth.as_deref() {
                        Some(_) => match self.service_token(&project, &service, &env, &aud).await {
                            Ok(Some((jwt, _))) => Ok(Some(jwt)),
                            Ok(None) => Err("roll issues no token for this environment".to_string()),
                            Err(e) => Err(format!("its token: {e:#}")),
                        },
                        None => Ok(secret.clone()),
                    };
                    // No token: nothing is POSTed (or counted as a try); the batch is let go and
                    // read again once there's one.
                    let bearer = match bearer {
                        Ok(b) => {
                            token_failures = 0;
                            b
                        }
                        Err(why) => {
                            token_failures += 1;
                            if token_failures == 1 {
                                log.line(format!("push {}: {why}; trying again", rule.topic));
                            }
                            tries.insert((r.partition, r.offset), attempt);
                            tokio::time::sleep(backoff(token_failures)).await;
                            consumer = self.new_consumer(&key, &rule).await;
                            continue 'read;
                        }
                    };
                    let Some(req) = request(&rule, &host, &r, attempt, bearer.as_deref()) else { break };
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
                        self.commit_push(&rule, &consumer, &r, &log).await;
                        let now = code_sql::now_ms();
                        note(&|s| {
                            s.delivered += 1;
                            s.last_delivered_at = Some(now);
                            s.failing_since = None;
                            s.last_error = None;
                        });
                        break;
                    };
                    let now = code_sql::now_ms();
                    note(&|s| {
                        s.failing_since.get_or_insert(now);
                        s.last_error = Some(format!("offset {} of partition {}: {why}", r.offset, r.partition));
                    });
                    if let Some(dl) = rule.dead_letter.as_deref().filter(|_| attempt >= rule.max_attempts.unwrap_or(5)) {
                        let mut dead = r.clone();
                        dead.headers.push(("x-roll-error".into(), format!("{} {why} after {attempt} tries", rule.path).into_bytes()));
                        dead.headers.push(("x-roll-source".into(), format!("{}/{}/{}", rule.topic, r.partition, r.offset).into_bytes()));
                        match resources.produce(&rule.stream, dl, &dead).await {
                            Ok(()) => {
                                log.line(format!("push {}: offset {} of partition {} {why} {attempt} times; sent to {dl}", rule.topic, r.offset, r.partition));
                                self.commit_push(&rule, &consumer, &r, &log).await;
                                note(&|s| {
                                    s.dead_lettered += 1;
                                    s.failing_since = None;
                                });
                                break;
                            }
                            Err(e) => log.line(format!("push {}: sending offset {} to {dl}: {e:#}", rule.topic, r.offset)),
                        }
                    }
                    let wait = backoff(attempt);
                    // Its lease would lapse before the next try: let the batch go and read it again
                    // (this record first) as a fresh consumer.
                    if read_at.elapsed() + wait >= HOLD {
                        tries.insert((r.partition, r.offset), attempt + 1);
                        tokio::time::sleep(wait).await;
                        consumer = self.new_consumer(&key, &rule).await;
                        continue 'read;
                    }
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

    /// `GET …/environments/{env}/push`: the environment's rules (`run.push`) as they stand, read on
    /// the service's owner (where its loops run): each group's lag and how deliveries have gone
    /// since that node began them.
    pub async fn push_states(self: &Arc<Self>, service: &str, env: &str) -> crate::Result<Vec<PushState>> {
        let e = self.environment(service, env).await?;
        let sid = service.to_string();
        let svc = self.db(move |s| s.service(&sid)).await?;
        let run = self.lives.lock().get(&crate::env_key(service, env)).and_then(|l| l.template()).map(|t| t.run.clone()).unwrap_or(svc.run.clone());
        let mut out = Vec::new();
        for rule in run.push.into_iter().filter(|p| p.environment() == env) {
            let off = if let Err(why) = self.stream_followable(&svc.project, &rule.stream) {
                Some(why)
            } else if e.summary.active.is_none() {
                Some("nothing is deployed here yet".into())
            } else if e.paused {
                Some("the environment is paused".into())
            } else {
                None
            };
            let lag = if off.is_none() { self.deps.resources.backlog(&rule.stream, &rule.group).await.ok() } else { None };
            let stats = self.pushers.stats.lock().get(&rule_key(service, env, &rule)).cloned().unwrap_or_default();
            out.push(PushState { rule, lag, off, stats });
        }
        Ok(out)
    }

    /// A new consumer for `key`'s loop; the one before it leaves the group.
    async fn new_consumer(&self, key: &str, rule: &Push) -> String {
        let c = format!("{CONSUMER}{}", crate::model::random(12));
        let old = self.pushers.consumers.lock().insert(key.to_string(), c.clone());
        if let Some(old) = old {
            let _ = self.deps.resources.leave(&rule.stream, &rule.group, &old).await;
        }
        c
    }

    /// The group past `r`: retried until the stream takes it (a lost commit delivers it again).
    async fn commit_push(&self, rule: &Push, consumer: &str, r: &StreamRecord, log: &crate::logs::LogSink) {
        for attempt in 1.. {
            match self.deps.resources.commit(&rule.stream, &rule.topic, &rule.group, consumer, r.partition, r.offset + 1).await {
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
    fn a_record_is_a_cloudevent_with_its_value() {
        let rule = Push { stream: "st_aaaaaaaaaaaa".into(), topic: "orders".into(), group: "g".into(), path: "/events".into(), environment: None, dead_letter: None, max_attempts: None, timeout_s: None, max_in_flight: None, auth: None };
        let headers = vec![("content-type".into(), b"application/json".to_vec()), ("trace id".into(), b"x".to_vec()), ("tenant".into(), b"acme".to_vec()), ("id".into(), b"forged".to_vec())];
        let r = StreamRecord { partition: 2, offset: 41, timestamp: 7, key: Some(b"k1".to_vec()), value: Some(b"{\"id\":1}".to_vec()), headers };
        let req = request(&rule, "web.roll.test", &r, 3, Some("s3cret")).unwrap();
        let h = req.headers();
        let get = |k: &str| h.get(k).map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
        assert_eq!((req.method().as_str(), req.uri().path()), ("POST", "/events"));
        assert_eq!((get("content-type"), get("ce-id"), get("ce-source")), ("application/json".into(), "orders/2/41".into(), "/streams/st_aaaaaaaaaaaa/topics/orders".into()));
        assert_eq!((get("ce-rolloffset"), get("ce-rollattempt"), get("ce-partitionkey"), get("ce-tenant")), ("41".into(), "3".into(), "azE=".into(), "acme".into()));
        assert_eq!(get("ce-time"), "1970-01-01T00:00:00.007Z");
        assert_eq!(get("authorization"), "Bearer s3cret");
        assert!(!h.keys().any(|k| k.as_str().contains("trace")), "a name CloudEvents can't carry is left out");
        assert!(!h.keys().any(|k| k.as_str().starts_with("x-roll-")), "the proxy drops x-roll-*");
    }
}
