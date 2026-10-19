//! Workers: find due messages, lease them, deliver them, and record the outcome.
//!
//! Lanes (due-index partitions) are spread over live workers by rendezvous hashing on their
//! heartbeats, so adding or removing a worker moves only its share. Claims are compare-and-set
//! with a lease: if a worker dies mid-delivery, the lease lapses and another worker retries.
//! Delivery is therefore at least once; receivers dedupe on `webhook-id`.

use crate::engine::Tasks;
use crate::flow::{self, Acquire};
use crate::model::*;
use crate::store::{self, Expect};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore, watch};
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, info, warn};

pub(crate) struct Inflight {
    pub msg: Option<Arc<Mutex<Message>>>,
    pub slots: Vec<String>,
    pub abort: Option<AbortHandle>,
}

/// How a delivery went.
pub(crate) enum Done {
    Ok(Outcome),
    Failed {
        outcome: Outcome,
        /// Server-requested delay (`Retry-After`).
        after_ms: Option<Ms>,
        retryable: bool,
    },
    /// Nothing left to do (stale tick, finished run …).
    Skip(String),
}

// --- HTTP client ---

pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10
                || o[0] == 0
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private(IpAddr::V4(v4));
            }
            let s = v6.segments();
            v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Resolves names and drops private addresses, so a public name can't point inside the network.
struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0)).await?.filter(|a| !is_private(a.ip())).collect();
            if addrs.is_empty() {
                return Err(format!("{} resolves only to private addresses", name.as_str()).into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

pub(crate) fn client(config: &crate::engine::Config) -> anyhow::Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .user_agent(&config.user_agent)
        .redirect(reqwest::redirect::Policy::limited(5))
        // Keep connections to endpoints open between deliveries: a new TLS handshake sends
        // more bytes (billed egress) than a typical message.
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10));
    if config.block_private_urls {
        b = b.dns_resolver(Arc::new(PublicResolver));
    }
    Ok(b.build()?)
}

// --- the worker loop ---

pub struct Worker {
    stop: watch::Sender<bool>,
    join: tokio::task::JoinHandle<()>,
}

impl Worker {
    /// Stop taking work, let deliveries in flight finish (up to `drain_ms`), hand the rest back.
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.join.await;
    }

    /// Stop abruptly, as if the process died: leases are left to lapse.
    pub fn kill(self) {
        self.join.abort();
    }
}

pub fn start(t: Arc<Tasks>) -> Worker {
    let (stop, rx) = watch::channel(false);
    // Everything a worker writes is redone if lost (at worst as a redelivery), so it never waits
    // for durability.
    let join = tokio::spawn(store::relaxed(run(t, rx)));
    Worker { stop, join }
}

fn owner<'a>(lane: &str, nodes: &'a [Node]) -> Option<&'a str> {
    nodes
        .iter()
        .max_by_key(|n| {
            let mut h: u64 = 0xcbf29ce484222325;
            for b in n.id.bytes().chain(*b"|").chain(lane.bytes()) {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            // murmur3 fmix64: FNV alone barely separates short, similar keys.
            h ^= h >> 33;
            h = h.wrapping_mul(0xff51afd7ed558ccd);
            h ^= h >> 33;
            h = h.wrapping_mul(0xc4ceb9fe1a85ec53);
            h ^ (h >> 33)
        })
        .map(|n| n.id.as_str())
}

async fn run(t: Arc<Tasks>, mut stop: watch::Receiver<bool>) {
    let started_at = now_ms();
    let sem = Arc::new(Semaphore::new(t.config.max_inflight));
    let mut lanes: HashMap<String, Ms> = HashMap::new();
    let mut set: JoinSet<()> = JoinSet::new();
    let mut next_heartbeat = 0;
    let mut next_sweep = now_ms() + 5_000;
    info!(node = %t.config.node_id, "worker started");
    loop {
        if *stop.borrow() {
            break;
        }
        while set.try_join_next().is_some() {}
        let now = now_ms();
        if now >= next_heartbeat {
            match heartbeat(&t, started_at, lanes.len()).await {
                Ok(owned) => {
                    lanes.retain(|l, _| owned.contains(l));
                    for l in owned {
                        lanes.entry(l).or_insert(now);
                    }
                }
                Err(e) => warn!(error = %e, "heartbeat failed"),
            }
            next_heartbeat = now + t.config.heartbeat_ms;
            if now >= next_sweep {
                next_sweep = now + 60_000;
                match t.store.sweep(now).await {
                    Ok(n) if n > 0 => debug!(n, "swept expired records"),
                    Err(e) => warn!(error = %e, "sweep failed"),
                    _ => {}
                }
            }
        }
        {
            let mut hints = t.hints.lock().unwrap();
            for (lane, at) in hints.drain() {
                if let Some(next) = lanes.get_mut(&lane) {
                    *next = (*next).min(at);
                }
            }
        }

        let mut order: Vec<(String, Ms)> = lanes.iter().filter(|(_, n)| **n <= now).map(|(l, n)| (l.clone(), *n)).collect();
        order.sort_by_key(|(_, n)| *n);
        for (lane, _) in order {
            if sem.available_permits() == 0 {
                break;
            }
            let next = poll_lane(&t, &lane, &sem, &mut set).await.unwrap_or_else(|e| {
                warn!(lane, error = %e, "poll failed");
                now_ms() + t.config.poll_ms
            });
            lanes.insert(lane, next);
        }

        let wake_at = lanes.values().copied().min().unwrap_or(i64::MAX).min(next_heartbeat);
        let sleep = (wake_at - now_ms()).clamp(1, t.config.poll_ms.max(1)) as u64;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(sleep)) => {}
            _ = t.wake.notified() => {}
            _ = stop.changed() => {}
        }
    }
    drain(&t, set).await;
}

/// Poll one lane; returns when to poll it next.
async fn poll_lane(t: &Arc<Tasks>, lane: &str, sem: &Arc<Semaphore>, set: &mut JoinSet<()>) -> anyhow::Result<Ms> {
    let now = now_ms();
    let idle = now + t.config.poll_ms;
    let mut limit = t.config.batch.min(sem.available_permits());
    if let Some(rest) = lane.strip_prefix("q:") {
        let name = rest.rsplit_once(':').map(|(n, _)| n).unwrap_or(rest);
        let Some(q) = t.queue_cached(name) else { return Ok(idle) };
        if q.paused {
            return Ok(idle);
        }
        if let Some(p) = q.parallelism {
            let busy = flow::slot_holders(t, &format!("q:{name}")).await?;
            let free = (p as usize).saturating_sub(busy);
            if free == 0 {
                return Ok(idle);
            }
            limit = limit.min(free);
        }
        if let Some(rate) = q.rate {
            let at = flow::next_free(t, &format!("q:{name}"), rate).await?;
            if at > now {
                return Ok(at.min(idle));
            }
            limit = limit.min(rate.limit as usize);
        }
    }
    if limit == 0 {
        return Ok(now + 10);
    }
    let due = t.store.msg_due(lane, now, limit).await?;
    let mut spawned = 0;
    for id in &due.ids {
        {
            let mut inflight = t.inflight.lock().unwrap();
            if inflight.contains_key(id) {
                continue;
            }
            inflight.insert(id.clone(), Inflight { msg: None, slots: vec![], abort: None });
        }
        let Ok(permit) = sem.clone().try_acquire_owned() else {
            t.inflight.lock().unwrap().remove(id);
            break;
        };
        let (t2, id2) = (t.clone(), id.clone());
        let abort = set.spawn(store::relaxed(async move {
            if let Err(e) = process(&t2, &id2).await {
                warn!(message = id2, error = %e, "processing failed");
            }
            t2.inflight.lock().unwrap().remove(&id2);
            drop(permit);
        }));
        if let Some(i) = t.inflight.lock().unwrap().get_mut(id) {
            i.abort = Some(abort);
        }
        spawned += 1;
    }
    Ok(if due.ids.len() >= limit && spawned > 0 {
        now
    } else if due.ids.len() >= limit {
        now + 20
    } else {
        due.next_at.map(|n| n.min(idle)).unwrap_or(idle)
    })
}

async fn heartbeat(t: &Tasks, started_at: Ms, lanes: usize) -> anyhow::Result<Vec<String>> {
    let now = now_ms();
    let inflight = t.inflight.lock().unwrap().len();
    if t.config.emf_metrics {
        // Share of delivery slots in use: what the ECS service scales on.
        let saturation = 100.0 * inflight as f64 / t.config.max_inflight.max(1) as f64;
        println!(
            "{}",
            json!({
                "_aws": {
                    "Timestamp": now,
                    "CloudWatchMetrics": [{
                        "Namespace": "roll/tasks",
                        "Dimensions": [["Service"]],
                        "Metrics": [{ "Name": "Saturation", "Unit": "Percent" }],
                    }],
                },
                "Service": "worker",
                "Saturation": saturation,
                "Inflight": inflight,
                "Lanes": lanes,
            })
        );
    }
    let me = Node { id: t.config.node_id.clone(), seen_at: now, started_at, inflight, lanes };
    store::put_doc(&*t.store, crate::engine::KIND_NODE, &me.id, &me, Expect::Any).await?;
    let all: Vec<Node> = store::list_docs(&*t.store, crate::engine::KIND_NODE).await?;
    let mut live = Vec::new();
    for n in all {
        if n.seen_at > now - t.config.node_ttl_ms {
            live.push(n);
        } else if n.seen_at < now - 10 * t.config.node_ttl_ms {
            let _ = t.store.doc_delete(crate::engine::KIND_NODE, &n.id).await;
        }
    }
    if !live.iter().any(|n| n.id == me.id) {
        live.push(me);
    }
    live.sort_by(|a, b| a.id.cmp(&b.id));
    t.refresh().await?;
    t.cache.write().unwrap().nodes = live.clone();

    let mut all_lanes: Vec<String> = (0..t.store.shards()).map(|s| format!("s{s}")).collect();
    for q in t.cache.read().unwrap().queues.values() {
        all_lanes.extend(q.push_lanes());
    }
    let owned = all_lanes.into_iter().filter(|l| owner(l, &live) == Some(t.config.node_id.as_str())).collect();

    extend_leases(t).await;
    if let Err(e) = t.flush_usage().await {
        warn!(error = %e, "usage flush failed");
    }
    Ok(owned)
}

/// Keep leases of long deliveries alive.
async fn extend_leases(t: &Tasks) {
    let cells: Vec<(Arc<Mutex<Message>>, Vec<String>)> = t
        .inflight
        .lock()
        .unwrap()
        .values()
        .filter_map(|i| i.msg.clone().map(|m| (m, i.slots.clone())))
        .collect();
    let now = now_ms();
    for (cell, slots) in cells {
        let mut c = cell.lock().await;
        if c.state != MsgState::Active || c.due_at - now > t.config.lease_ms / 2 + t.config.heartbeat_ms {
            continue;
        }
        let v = c.version;
        let mut next = c.clone();
        next.due_at = now + t.config.lease_ms + t.config.heartbeat_ms;
        if let Ok(true) = t.store.msg_update(&next, v).await {
            next.version = v + 1;
            *c = next;
            for s in &slots {
                let _ = flow::extend(t, s, &c.id, c.due_at).await;
            }
        }
    }
}

async fn drain(t: &Arc<Tasks>, mut set: JoinSet<()>) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(t.config.drain_ms.max(0) as u64);
    let pending = set.len();
    if pending > 0 {
        info!(pending, "draining deliveries");
    }
    while !set.is_empty() {
        match tokio::time::timeout_at(deadline, set.join_next()).await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    // Whatever is still running goes back to the queue for another worker.
    let left: Vec<(String, Inflight)> = t.inflight.lock().unwrap().drain().collect();
    set.abort_all();
    for (id, inf) in left {
        if let Some(cell) = inf.msg {
            let mut c = cell.lock().await;
            if c.state == MsgState::Active {
                let v = c.version;
                c.state = MsgState::Pending;
                c.attempts = c.attempts.saturating_sub(1);
                c.lease = None;
                c.due_at = now_ms();
                let _ = t.store.msg_update(&c, v).await;
            }
        }
        for s in inf.slots {
            let _ = flow::release(t, &s, &id).await;
        }
    }
    let _ = t.flush_usage().await;
    let _ = t.store.doc_delete(crate::engine::KIND_NODE, &t.config.node_id).await;
    info!(node = %t.config.node_id, "worker stopped");
}

// --- one message ---

/// How long an invocation waits for its run to appear before it counts as an orphan.
const ORPHAN_GRACE_MS: Ms = 30_000;

pub(crate) async fn process(t: &Arc<Tasks>, id: &str) -> anyhow::Result<()> {
    let Some(m) = t.store.msg_get(id).await? else { return Ok(()) };
    let now = now_ms();
    if !matches!(m.state, MsgState::Pending | MsgState::Active) || m.due_at > now || m.target == Target::Pull {
        return Ok(());
    }
    let v = m.version;
    let mut c = m;
    c.state = MsgState::Active;
    c.attempts += 1;
    c.lease = Some(crate::ids::ulid());
    // Short requests are covered by the first lease; longer ones get it extended by heartbeats.
    c.due_at = now + t.config.lease_ms.max(c.timeout_ms.min(t.config.lease_ms * 4) + t.config.lease_ms / 2);
    c.updated_at = now;
    if !t.store.msg_update(&c, v).await? {
        return Ok(());
    }
    c.version = v + 1;
    let cell = Arc::new(Mutex::new(c.clone()));
    if let Some(i) = t.inflight.lock().unwrap().get_mut(id) {
        i.msg = Some(cell.clone());
    }

    // A run is written just after its invocation (so a crash leaves an orphan message, never a
    // run nobody calls); a fresh invocation may get here first. Give the run a moment.
    if let Target::Run { run_id } = &c.target
        && now - c.created_at < ORPHAN_GRACE_MS
        && t.store.run_get(run_id).await?.is_none()
    {
        return park(t, &cell, (50i64 << c.deferrals.min(6)).min(2_000)).await;
    }

    if let Some(g) = &c.group
        && t.store.msg_group_head(g).await?.as_deref() != Some(id)
    {
        // Not this message's turn; the one ahead wakes it when it finishes.
        let wait = (5_000i64 << c.deferrals.min(6)).min(t.config.max_park_ms.max(5_000) * 5);
        return park(t, &cell, wait).await;
    }

    let mut held = Vec::new();
    if let Some(wait) = acquire_flow(t, &c, &mut held).await? {
        release_slots(t, id, &held).await;
        return park(t, &cell, wait).await;
    }
    if let Some(i) = t.inflight.lock().unwrap().get_mut(id) {
        i.slots = held.clone();
    }

    let done = match &c.target {
        Target::Http { url, method, headers } => Ok(deliver_http(t, &c, url, method, headers).await),
        Target::Tick { .. } => crate::schedules::fire(t, &c).await,
        Target::Run { .. } => crate::workflow::invoke(t, &cell).await,
        Target::Timer { .. } => crate::workflow::timer(t, &c).await,
        Target::Flush { .. } => crate::workflow::flush(t, &c).await,
        Target::Pull => Ok(Done::Skip("pull".into())),
    };
    let done = done.unwrap_or_else(|e| Done::Failed {
        outcome: Outcome { at: now_ms(), error: Some(format!("{e:#}")), ..Default::default() },
        after_ms: None,
        retryable: true,
    });
    let result = finish(t, &cell, done).await;
    release_slots(t, id, &held).await;
    if let Some(i) = t.inflight.lock().unwrap().get_mut(id) {
        i.slots.clear();
    }
    result
}

/// Take every flow-control slot and rate unit the message needs. `Some(wait)`: try again later.
async fn acquire_flow(t: &Tasks, c: &Message, held: &mut Vec<String>) -> anyhow::Result<Option<Ms>> {
    let queue = c.queue.as_deref().and_then(|q| t.queue_cached(q));
    let mut slots: Vec<(String, u32)> = Vec::new();
    let mut rates: Vec<(String, Rate)> = Vec::new();
    if let Some(q) = &queue {
        if let Some(p) = q.parallelism {
            slots.push((format!("q:{}", q.name), p));
        }
        if let Some(r) = q.rate {
            rates.push((format!("q:{}", q.name), r));
        }
    }
    for f in &c.flows {
        if let Some(p) = f.parallelism {
            slots.push((format!("f:{}", f.key), p));
        }
        if let Some(r) = f.rate {
            rates.push((format!("f:{}", f.key), r));
        }
    }
    let park_full = t.config.max_park_ms;
    for (name, limit) in slots {
        match flow::acquire(t, &name, limit, &c.id, c.due_at).await? {
            Acquire::Granted => held.push(name),
            Acquire::Full => return Ok(Some(park_full)),
        }
    }
    for (name, rate) in rates {
        let wait = flow::take(t, &name, rate).await?;
        if wait > 0 {
            return Ok(Some(wait));
        }
    }
    Ok(None)
}

async fn release_slots(t: &Tasks, id: &str, held: &[String]) {
    for name in held {
        match flow::release(t, name, id).await {
            Ok(Some(next)) => t.wake_message(&next).await,
            Ok(None) => {}
            Err(e) => warn!(slot = name, error = %e, "slot release failed"),
        }
        if let Some(q) = name.strip_prefix("q:")
            && let Some(queue) = t.queue_cached(q)
        {
            for lane in queue.push_lanes() {
                t.nudge_lane(&lane, now_ms());
            }
        }
    }
}

/// Hand a claimed message back without counting the attempt.
async fn park(t: &Tasks, cell: &Arc<Mutex<Message>>, wait_ms: Ms) -> anyhow::Result<()> {
    let mut c = cell.lock().await;
    let v = c.version;
    let mut next = c.clone();
    let now = now_ms();
    next.state = MsgState::Pending;
    next.attempts = next.attempts.saturating_sub(1);
    next.lease = None;
    next.deferrals += 1;
    next.due_at = now + wait_ms.max(1);
    next.updated_at = now;
    if t.store.msg_update(&next, v).await? {
        next.version = v + 1;
        *c = next;
    }
    Ok(())
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Ms> {
    let v = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim();
    if let Ok(s) = v.parse::<f64>() {
        return Some((s * 1000.0) as i64);
    }
    let at = chrono::DateTime::parse_from_rfc2822(v).ok()?.timestamp_millis();
    Some((at - now_ms()).max(0))
}

async fn read_limited(mut resp: reqwest::Response, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        let room = max.saturating_sub(out.len());
        out.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if out.len() >= max {
            break;
        }
    }
    out
}

pub(crate) fn text_or_base64(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => format!("base64:{}", B64.encode(b)),
    }
}

async fn deliver_http(t: &Tasks, c: &Message, url: &str, method: &str, headers: &[(String, String)]) -> Done {
    let body = match t.store.msg_body(&c.id).await {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => {
            return Done::Failed {
                outcome: Outcome { at: now_ms(), error: Some(format!("reading body: {e}")), ..Default::default() },
                after_ms: None,
                retryable: true,
            };
        }
    };
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::POST);
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut h = HeaderMap::new();
    for (k, v) in headers {
        if let (Ok(k), Ok(v)) = (HeaderName::try_from(k.as_str()), HeaderValue::try_from(v.as_str())) {
            h.append(k, v);
        }
    }
    // Ours replace any the publisher set, so receivers can trust them.
    let mut set = |k: &str, v: &str| {
        if let (Ok(k), Ok(v)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
            h.insert(k, v);
        }
    };
    if let Some(ct) = &c.content_type {
        set("content-type", ct);
    }
    for (k, v) in t.sign_headers(&c.id, &body) {
        set(&k, &v);
    }
    set("roll-message-id", &c.id);
    set("roll-retried", &c.attempts.saturating_sub(1).to_string());
    set("roll-retries", &c.retries.to_string());
    for (k, v) in [("roll-queue", &c.queue), ("roll-topic", &c.topic), ("roll-schedule-id", &c.schedule_id), ("roll-label", &c.label)] {
        if let Some(v) = v {
            set(k, v);
        }
    }
    let req = t.http.request(method, url).timeout(Duration::from_millis(c.timeout_ms.max(1) as u64)).headers(h);
    t.count(|u| {
        u.deliveries += 1;
        u.bytes_out += body.len() as u64;
    });
    let started = std::time::Instant::now();
    let at = now_ms();
    match req.body(body).send().await {
        Ok(resp) => {
            let status = resp.status();
            let after = retry_after(resp.headers());
            let non_retryable = status.as_u16() == 489 || resp.headers().contains_key("roll-non-retryable");
            let bytes = read_limited(resp, t.config.response_max_bytes).await;
            let outcome = Outcome {
                at,
                status: Some(status.as_u16()),
                error: None,
                body: (!bytes.is_empty()).then(|| text_or_base64(&bytes)),
                duration_ms: started.elapsed().as_millis() as i64,
            };
            if status.is_success() {
                Done::Ok(outcome)
            } else {
                Done::Failed { outcome, after_ms: after, retryable: !non_retryable }
            }
        }
        Err(e) => {
            let error = if e.is_timeout() {
                format!("timed out after {}ms", c.timeout_ms)
            } else if e.is_connect() {
                format!("connection failed: {}", root_cause(&e))
            } else {
                root_cause(&e)
            };
            Done::Failed {
                outcome: Outcome { at, error: Some(error), duration_ms: started.elapsed().as_millis() as i64, ..Default::default() },
                after_ms: None,
                retryable: true,
            }
        }
    }
}

fn root_cause(e: &dyn std::error::Error) -> String {
    let mut cur = e;
    while let Some(next) = cur.source() {
        cur = next;
    }
    cur.to_string()
}

/// Record a delivery's outcome: delivered, retry later, or dead-letter.
async fn finish(t: &Arc<Tasks>, cell: &Arc<Mutex<Message>>, done: Done) -> anyhow::Result<()> {
    let mut c = cell.lock().await;
    let v = c.version;
    let now = now_ms();
    let mut next = c.clone();
    next.lease = None;
    next.updated_at = now;
    next.deferrals = 0;
    let (outcome, log) = match done {
        Done::Ok(o) => {
            next.state = MsgState::Delivered;
            next.expires_at = Some(now + t.config.retention_ms);
            next.reason = None;
            (Some(o), true)
        }
        Done::Skip(reason) => {
            next.state = MsgState::Delivered;
            next.expires_at = Some(now + t.config.retention_ms);
            next.reason = Some(reason);
            (None, false)
        }
        Done::Failed { outcome, after_ms, retryable } => {
            if retryable && next.attempts <= next.retries {
                let delay = after_ms
                    .map(|a| a.min(next.backoff.max_ms.max(1000)))
                    .unwrap_or_else(|| next.backoff.delay_ms(next.attempts, crate::ids::random_f64()));
                next.state = MsgState::Pending;
                next.due_at = now + delay.max(0);
            } else {
                next.state = MsgState::Failed;
                next.expires_at = Some(now + t.config.retention_ms);
                next.reason = Some(match (&outcome.status, &outcome.error) {
                    (_, Some(e)) => e.clone(),
                    (Some(s), _) if !retryable => format!("HTTP {s} (not retryable)"),
                    (Some(s), _) => format!("HTTP {s}"),
                    _ => "failed".into(),
                });
            }
            (Some(outcome), true)
        }
    };
    next.last = outcome.clone();
    // Log first: the attempt happened whatever becomes of the outcome, and readers never see
    // a finished message without its log.
    if log && let Some(o) = &outcome {
        let attempt = Attempt {
            message_id: next.id.clone(),
            n: next.attempts,
            at: o.at,
            duration_ms: o.duration_ms,
            status: o.status,
            error: o.error.clone(),
            response: o.body.clone(),
            next_at: (next.state == MsgState::Pending).then_some(next.due_at),
        };
        if let Err(e) = t.store.attempt_add(&attempt).await {
            warn!(error = %e, "attempt log failed");
        }
    }
    if !t.store.msg_update(&next, v).await? {
        // Canceled meanwhile, or the lease was lost: someone else owns the outcome.
        debug!(message = %c.id, "outcome dropped; message changed during delivery");
        return Ok(());
    }
    next.version = v + 1;
    *c = next.clone();
    drop(c);

    match next.state {
        MsgState::Delivered => {
            if matches!(next.target, Target::Http { .. }) {
                t.count(|u| u.delivered += 1);
            }
        }
        MsgState::Pending => {
            t.count(|u| u.failed_attempts += 1);
            t.nudge_lane(&next.lane, next.due_at);
        }
        MsgState::Failed => {
            t.count(|u| {
                u.failed_attempts += 1;
                u.dead += 1;
            });
            if let Err(e) = crate::workflow::on_dead(t, &next).await {
                warn!(error = %e, "dead-letter hook failed");
            }
        }
        _ => {}
    }
    if next.state.is_final() {
        t.after_final(&next).await;
        if let Err(e) = callback(t, &next, outcome.as_ref()).await {
            warn!(message = %next.id, error = %e, "callback publish failed");
        }
    }
    Ok(())
}

/// Publish the success or failure callback of a finished message.
async fn callback(t: &Tasks, m: &Message, outcome: Option<&Outcome>) -> anyhow::Result<()> {
    let url = match m.state {
        MsgState::Delivered => m.callback.as_ref(),
        MsgState::Failed => m.failure_callback.as_ref(),
        _ => None,
    };
    let Some(url) = url else { return Ok(()) };
    let source = t.store.msg_body(&m.id).await?.unwrap_or_default();
    let body = json!({
        "message_id": m.id,
        "state": m.state,
        "url": m.target.url(),
        "status": outcome.and_then(|o| o.status),
        "body": outcome.and_then(|o| o.body.clone()),
        "error": outcome.and_then(|o| o.error.clone()),
        "attempts": m.attempts,
        "retries": m.retries,
        "queue": m.queue,
        "topic": m.topic,
        "schedule_id": m.schedule_id,
        "label": m.label,
        "source_body": B64.encode(&source),
    });
    t.publish(crate::engine::PublishRequest {
        url: Some(url.clone()),
        body: Some(body),
        label: m.label.clone(),
        headers: [("roll-callback-for".to_string(), m.id.clone())].into(),
        ..Default::default()
    })
    .await
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

pub(crate) fn json_body(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        for ip in ["127.0.0.1", "10.0.0.1", "172.16.5.4", "192.168.1.1", "169.254.169.254", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111", "172.32.0.1"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn lanes_spread_and_move_minimally() {
        let nodes = |ids: &[&str]| ids.iter().map(|id| Node { id: id.to_string(), seen_at: 0, started_at: 0, inflight: 0, lanes: 0 }).collect::<Vec<_>>();
        let three = nodes(&["a", "b", "c"]);
        let lanes: Vec<String> = (0..300).map(|i| format!("s{i}")).collect();
        let mut counts = HashMap::new();
        for l in &lanes {
            *counts.entry(owner(l, &three).unwrap()).or_insert(0) += 1;
        }
        assert!(counts.values().all(|c| *c > 60), "{counts:?}");
        // Removing a node only moves its own lanes.
        let two = nodes(&["a", "b"]);
        for l in &lanes {
            let before = owner(l, &three).unwrap();
            if before != "c" {
                assert_eq!(owner(l, &two).unwrap(), before);
            }
        }
        assert_eq!(owner("s1", &[]), None);
    }

    #[test]
    fn retry_after_header() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        assert_eq!(retry_after(&h), Some(3000));
        let later = chrono::Utc::now() + chrono::Duration::seconds(10);
        h.insert(reqwest::header::RETRY_AFTER, later.to_rfc2822().parse().unwrap());
        let ms = retry_after(&h).unwrap();
        assert!((8_000..=10_000).contains(&ms), "{ms}");
    }
}
