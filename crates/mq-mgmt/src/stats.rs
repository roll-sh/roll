//! Message-rate sampler. Every tick it snapshots the engine's counters (queues, exchanges,
//! vhosts, channels, connections), turns the deltas into per-second rates and keeps a short
//! history for the global and per-vhost series (`msg_rates_age` / `lengths_age`).

use mq_core::Broker;
use mq_core::broker::{ChannelInfo, ConnectionInfo};
use mq_core::exchange::Exchange;
use mq_core::queue::QueueHandle;
use mq_core::vhost::VHost;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

/// A sampled object.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Global,
    VHost(String),
    Queue(String, String),
    Exchange(String, String),
    Channel(u64),
    Conn(u64),
    Churn,
}

/// Named counter values of one object.
pub(crate) type Snapshot = Vec<(&'static str, f64)>;

/// Values that go down as well as up (their rates may be negative).
const GAUGES: &[&str] = &["messages", "messages_ready", "messages_unacknowledged"];
/// Byte counters (`data_rates_age`).
const DATA: &[&str] = &["recv_oct", "send_oct"];
/// Keep this much history for the global and vhost series (RabbitMQ's basic retention).
const HISTORY: Duration = Duration::from_secs(600);

fn is_gauge(n: &str) -> bool {
    GAUGES.contains(&n)
}

#[derive(Default)]
struct Series {
    prev: Snapshot,
    rates: Vec<(&'static str, f64)>,
    history: VecDeque<(u64, Snapshot)>,
}

/// Object churn, inferred from what appears and disappears between samples.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Churn {
    pub connection_created: u64,
    pub connection_closed: u64,
    pub channel_created: u64,
    pub channel_closed: u64,
    pub queue_declared: u64,
    pub queue_created: u64,
    pub queue_deleted: u64,
}

impl Churn {
    pub fn snapshot(&self) -> Snapshot {
        vec![
            ("connection_created", self.connection_created as f64),
            ("connection_closed", self.connection_closed as f64),
            ("channel_created", self.channel_created as f64),
            ("channel_closed", self.channel_closed as f64),
            ("queue_declared", self.queue_declared as f64),
            ("queue_created", self.queue_created as f64),
            ("queue_deleted", self.queue_deleted as f64),
        ]
    }
}

#[derive(Default)]
struct Inner {
    last: Option<Instant>,
    series: HashMap<Key, Series>,
    /// Counters of vhosts that were unloaded or deleted, so global totals stay monotonic.
    retired: HashMap<&'static str, f64>,
    /// vhost name → (identity of the loaded instance, last counters).
    vhosts: HashMap<String, (usize, Snapshot)>,
    conns: Option<HashSet<u64>>,
    chans: HashSet<u64>,
    queues: HashSet<(String, String)>,
    churn: Churn,
}

/// Which history to attach to `*_details` (`msg_rates_age`, `lengths_age`, `data_rates_age`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Ages {
    pub msg: Option<(u64, u64)>,
    pub lengths: Option<(u64, u64)>,
    pub data: Option<(u64, u64)>,
}

impl Ages {
    fn for_name(&self, n: &str) -> Option<(u64, u64)> {
        if is_gauge(n) {
            self.lengths
        } else if DATA.contains(&n) {
            self.data
        } else {
            self.msg
        }
    }
}

pub(crate) struct Stats {
    interval: Duration,
    inner: Mutex<Inner>,
}

impl Stats {
    pub fn new(interval: Duration) -> Self {
        Stats { interval: interval.max(Duration::from_millis(10)), inner: Mutex::default() }
    }

    /// While nobody uses the API: keep only what must not be lost (the counters of vhosts that
    /// unload, so global totals stay monotonic) and drop rates and history, so an idle broker
    /// with a million queues does no per-queue work and holds no per-queue series.
    pub fn sample_vhosts(&self, broker: &Broker) {
        let conns = broker.connections();
        let vh_now: HashMap<String, (usize, Snapshot)> =
            broker.vhosts().iter().map(|vh| (vh.name.clone(), (Arc::as_ptr(vh) as usize, vhost_snapshot(vh, &conns)))).collect();
        let mut g = self.inner.lock();
        let inner = &mut *g;
        retire(inner, &vh_now);
        inner.vhosts = vh_now;
        inner.last = None;
        inner.series.clear();
    }

    /// Take a baseline sample if rates were dropped while idle (see [`Stats::sample_vhosts`]).
    pub fn resume(&self, broker: &Broker) {
        if self.inner.lock().last.is_none() {
            self.sample(broker);
        }
    }

    /// Snapshot everything and update rates.
    pub fn sample(&self, broker: &Broker) {
        let conns = broker.connections();
        let chans = broker.channels();
        let mut snaps: Vec<(Key, Snapshot)> = Vec::new();
        let mut vh_now: HashMap<String, (usize, Snapshot)> = HashMap::new();
        let mut queue_names = HashSet::new();
        for vh in broker.vhosts() {
            for q in vh.queues() {
                queue_names.insert((vh.name.clone(), q.meta.name.clone()));
                snaps.push((Key::Queue(vh.name.clone(), q.meta.name.clone()), queue_snapshot(&q)));
            }
            for ex in vh.exchanges() {
                snaps.push((Key::Exchange(vh.name.clone(), ex.name.clone()), exchange_snapshot(&ex)));
            }
            let s = vhost_snapshot(&vh, &conns);
            vh_now.insert(vh.name.clone(), (Arc::as_ptr(&vh) as usize, s.clone()));
            snaps.push((Key::VHost(vh.name.clone()), s));
        }
        for c in &chans {
            snaps.push((Key::Channel(c.id), channel_snapshot(c)));
        }
        for c in &conns {
            snaps.push((Key::Conn(c.id), conn_snapshot(c)));
        }

        let now = Instant::now();
        let now_ms = mq_core::now_ms();
        let mut g = self.inner.lock();
        let inner = &mut *g;

        retire(inner, &vh_now);
        let global = global_from(vh_now.values().map(|(_, s)| s), &inner.retired);
        inner.vhosts = vh_now;

        // Churn.
        let conn_ids: HashSet<u64> = conns.iter().map(|c| c.id).collect();
        let chan_ids: HashSet<u64> = chans.iter().map(|c| c.id).collect();
        if let Some(prev) = &inner.conns {
            let c = &mut inner.churn;
            c.connection_created += conn_ids.difference(prev).count() as u64;
            c.connection_closed += prev.difference(&conn_ids).count() as u64;
            c.channel_created += chan_ids.difference(&inner.chans).count() as u64;
            c.channel_closed += inner.chans.difference(&chan_ids).count() as u64;
            let created = queue_names.difference(&inner.queues).count() as u64;
            c.queue_created += created;
            c.queue_declared += created;
            c.queue_deleted += inner.queues.difference(&queue_names).count() as u64;
        } else {
            // First sample: everything that exists was created at some point.
            let c = &mut inner.churn;
            c.connection_created = conn_ids.len() as u64;
            c.channel_created = chan_ids.len() as u64;
            c.queue_created = queue_names.len() as u64;
            c.queue_declared = queue_names.len() as u64;
        }
        inner.conns = Some(conn_ids);
        inner.chans = chan_ids;
        inner.queues = queue_names;
        snaps.push((Key::Global, global));
        snaps.push((Key::Churn, inner.churn.snapshot()));

        let dt = inner.last.map(|l| now.duration_since(l).as_secs_f64()).filter(|d| *d > 0.0);
        inner.last = Some(now);
        let keep = (HISTORY.as_millis() / self.interval.as_millis().max(1)) as usize + 1;
        let mut live: HashSet<Key> = HashSet::with_capacity(snaps.len());
        for (key, snap) in snaps {
            let s = inner.series.entry(key.clone()).or_default();
            s.rates = match dt {
                Some(dt) => snap
                    .iter()
                    .map(|(n, v)| {
                        let prev = s.prev.iter().find(|(p, _)| p == n).map(|(_, p)| *p).unwrap_or(*v);
                        let r = (v - prev) / dt;
                        (*n, if is_gauge(n) { r } else { r.max(0.0) })
                    })
                    .collect(),
                None => snap.iter().map(|(n, _)| (*n, 0.0)).collect(),
            };
            if matches!(key, Key::Global | Key::VHost(_)) {
                s.history.push_front((now_ms, snap.clone()));
                s.history.truncate(keep);
            }
            s.prev = snap;
            live.insert(key);
        }
        inner.series.retain(|k, _| live.contains(k));
    }

    /// Global counters (live vhosts plus retired ones).
    pub fn global(&self, broker: &Broker) -> Snapshot {
        let conns = broker.connections();
        let snaps: Vec<Snapshot> = broker.vhosts().iter().map(|vh| vhost_snapshot(vh, &conns)).collect();
        let g = self.inner.lock();
        global_from(snaps.iter(), &g.retired)
    }

    pub fn churn(&self) -> Churn {
        self.inner.lock().churn
    }

    /// Add `name` and `name_details` for each value of `snap` to `o`.
    pub fn put(&self, o: &mut Map<String, Value>, key: &Key, snap: &Snapshot, ages: &Ages) {
        let g = self.inner.lock();
        let series = g.series.get(key);
        for (name, v) in snap {
            o.insert((*name).into(), num(*v));
            let rate = series.and_then(|s| s.rates.iter().find(|(n, _)| n == name)).map(|(_, r)| *r).unwrap_or(0.0);
            let mut d = Map::new();
            d.insert("rate".into(), json!(round(rate)));
            if let (Some((age, incr)), Some(s)) = (ages.for_name(name), series) {
                add_samples(&mut d, s, name, *v, age, incr);
            }
            o.insert(format!("{name}_details"), Value::Object(d));
        }
    }
}

fn add_samples(d: &mut Map<String, Value>, s: &Series, name: &str, current: f64, age_s: u64, incr_s: u64) {
    let now = mq_core::now_ms();
    let from = now.saturating_sub(age_s * 1000);
    let incr_ms = incr_s.max(1) * 1000;
    let mut samples: Vec<(u64, f64)> = vec![(now, current)];
    let mut next_at = now.saturating_sub(incr_ms);
    for (ts, snap) in &s.history {
        if *ts < from {
            break;
        }
        if *ts <= next_at {
            let v = snap.iter().find(|(n, _)| *n == name).map(|(_, v)| *v).unwrap_or(0.0);
            samples.push((*ts, v));
            next_at = ts.saturating_sub(incr_ms);
        }
    }
    let avg = samples.iter().map(|(_, v)| v).sum::<f64>() / samples.len() as f64;
    let avg_rate = match (samples.first(), samples.last()) {
        (Some(a), Some(b)) if a.0 > b.0 => (a.1 - b.1) / ((a.0 - b.0) as f64 / 1000.0),
        _ => 0.0,
    };
    d.insert("samples".into(), Value::Array(samples.iter().map(|(t, v)| json!({"sample": num(*v), "timestamp": t})).collect()));
    d.insert("avg_rate".into(), json!(round(avg_rate)));
    d.insert("avg".into(), json!(round(avg)));
}

/// Integral values render as JSON integers, like RabbitMQ's counters.
pub(crate) fn num(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.0e15 { json!(v as i64) } else { json!(v) }
}

fn round(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn global_from<'a>(vhosts: impl Iterator<Item = &'a Snapshot>, retired: &HashMap<&'static str, f64>) -> Snapshot {
    let mut out: Snapshot = Vec::new();
    for s in vhosts {
        for (k, v) in s {
            match out.iter_mut().find(|(n, _)| n == k) {
                Some(slot) => slot.1 += v,
                None => out.push((k, *v)),
            }
        }
    }
    if out.is_empty() {
        out = vhost_names().iter().map(|n| (*n, 0.0)).collect();
    }
    for (k, v) in retired {
        if let Some(slot) = out.iter_mut().find(|(n, _)| n == k) {
            slot.1 += v;
        }
    }
    out
}

fn vhost_names() -> &'static [&'static str] {
    &[
        "publish",
        "publish_in",
        "confirm",
        "return_unroutable",
        "drop_unroutable",
        "deliver_get",
        "deliver",
        "deliver_no_ack",
        "get",
        "get_no_ack",
        "get_empty",
        "ack",
        "redeliver",
        "routed",
        "recv_oct",
        "send_oct",
        "messages",
        "messages_ready",
        "messages_unacknowledged",
    ]
}

/// The message-stats part of a snapshot (everything but lengths and bytes).
pub(crate) fn is_message_stat(n: &str) -> bool {
    !is_gauge(n) && !DATA.contains(&n)
}

/// Keep the counters of vhosts that went away or were reloaded.
fn retire(inner: &mut Inner, vh_now: &HashMap<String, (usize, Snapshot)>) {
    for (name, (ptr, snap)) in &inner.vhosts {
        if vh_now.get(name).is_none_or(|(p, _)| p != ptr) {
            for (k, v) in snap.iter().filter(|(k, _)| !is_gauge(k)) {
                *inner.retired.entry(k).or_default() += v;
            }
        }
    }
}

pub(crate) fn queue_snapshot(q: &QueueHandle) -> Snapshot {
    let s = &q.shared;
    let (delivered, got) = (s.delivered.load(Relaxed) as f64, s.got.load(Relaxed) as f64);
    let (ready, unacked) = (s.ready.load(Relaxed) as f64, s.unacked.load(Relaxed) as f64);
    vec![
        ("publish", s.published.load(Relaxed) as f64),
        ("deliver", delivered),
        ("deliver_no_ack", 0.0),
        ("get", got),
        ("get_no_ack", 0.0),
        ("deliver_get", delivered + got),
        ("ack", s.acked.load(Relaxed) as f64),
        ("redeliver", s.redelivered.load(Relaxed) as f64),
        ("messages", ready + unacked),
        ("messages_ready", ready),
        ("messages_unacknowledged", unacked),
    ]
}

pub(crate) fn vhost_snapshot(vh: &VHost, conns: &[Arc<ConnectionInfo>]) -> Snapshot {
    let st = &vh.stats;
    let (mut got, mut redeliver, mut ready, mut unacked) = (0u64, 0u64, 0u64, 0u64);
    vh.for_each_queue(|q| {
        got += q.shared.got.load(Relaxed);
        redeliver += q.shared.redelivered.load(Relaxed);
        ready += q.shared.ready.load(Relaxed);
        unacked += q.shared.unacked.load(Relaxed);
    });
    let (mut recv, mut send) = (0u64, 0u64);
    for c in conns.iter().filter(|c| c.vhost == vh.name) {
        recv += c.recv_oct.load(Relaxed);
        send += c.send_oct.load(Relaxed);
    }
    let returned = st.returned.load(Relaxed);
    let delivered = st.delivered.load(Relaxed);
    let published = st.published.load(Relaxed) as f64;
    vec![
        ("publish", published),
        ("publish_in", published),
        ("confirm", st.confirmed.load(Relaxed) as f64),
        ("return_unroutable", returned as f64),
        ("drop_unroutable", st.unroutable.load(Relaxed).saturating_sub(returned) as f64),
        ("deliver_get", delivered as f64),
        ("deliver", delivered.saturating_sub(got) as f64),
        ("deliver_no_ack", 0.0),
        ("get", got as f64),
        ("get_no_ack", 0.0),
        ("get_empty", 0.0),
        ("ack", st.acked.load(Relaxed) as f64),
        ("redeliver", redeliver as f64),
        ("routed", st.routed.load(Relaxed) as f64),
        ("recv_oct", recv as f64),
        ("send_oct", send as f64),
        ("messages", (ready + unacked) as f64),
        ("messages_ready", ready as f64),
        ("messages_unacknowledged", unacked as f64),
    ]
}

pub(crate) fn exchange_snapshot(ex: &Exchange) -> Snapshot {
    vec![("publish_in", ex.stats.publish_in.load(Relaxed) as f64), ("publish_out", ex.stats.publish_out.load(Relaxed) as f64)]
}

pub(crate) fn channel_snapshot(c: &ChannelInfo) -> Snapshot {
    vec![
        ("publish", c.published.load(Relaxed) as f64),
        ("confirm", c.confirmed.load(Relaxed) as f64),
        ("return_unroutable", c.returned.load(Relaxed) as f64),
        ("deliver_get", c.delivered.load(Relaxed) as f64),
        ("ack", c.acked.load(Relaxed) as f64),
        ("redeliver", c.redelivered.load(Relaxed) as f64),
    ]
}

pub(crate) fn conn_snapshot(c: &ConnectionInfo) -> Snapshot {
    vec![("recv_oct", c.recv_oct.load(Relaxed) as f64), ("send_oct", c.send_oct.load(Relaxed) as f64)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_sampling_drops_rates_but_keeps_totals() {
        use mq_core::log::MemStore;
        use mq_core::meta::MemMeta;
        use mq_core::vhost::VHostInfo;
        let b = Broker::start(mq_core::BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
        let vh = b.create_vhost(VHostInfo { name: "shop".into(), ..Default::default() }).await.unwrap();
        let s = Stats::new(Duration::from_millis(10));
        s.sample(&b);
        assert!(!s.inner.lock().series.is_empty());

        s.sample_vhosts(&b);
        assert!(s.inner.lock().series.is_empty(), "no series while idle");
        // A vhost that is busy and then unloads while nobody watches keeps its counts.
        vh.stats.published.fetch_add(3, Relaxed);
        s.sample_vhosts(&b);
        drop(vh);
        assert!(b.suspend_vhost("shop").await);
        s.sample_vhosts(&b);
        let publish = |snap: Snapshot| snap.iter().find(|(n, _)| *n == "publish").map(|(_, v)| *v);
        assert_eq!(publish(s.global(&b)), Some(3.0));

        s.resume(&b);
        assert!(s.inner.lock().last.is_some(), "the first request after idle takes a baseline");
    }

    #[test]
    fn global_sums_and_retires() {
        let a: Snapshot = vec![("publish", 3.0), ("messages", 2.0)];
        let b: Snapshot = vec![("publish", 4.0), ("messages", 1.0)];
        let mut retired = HashMap::new();
        retired.insert("publish", 10.0);
        let g = global_from([&a, &b].into_iter(), &retired);
        assert_eq!(g, vec![("publish", 17.0), ("messages", 3.0)]);
        assert!(global_from(std::iter::empty(), &HashMap::new()).iter().any(|(n, _)| *n == "publish"));
    }

    #[test]
    fn samples_follow_the_requested_increment() {
        let now = mq_core::now_ms();
        let mut s = Series::default();
        for i in 1..=12u64 {
            s.history.push_back((now - i * 5_000, vec![("publish", (100 - i * 5) as f64)]));
        }
        let mut d = Map::new();
        add_samples(&mut d, &s, "publish", 100.0, 32, 10);
        let samples = d["samples"].as_array().unwrap();
        assert_eq!(samples[0]["sample"], json!(100));
        assert_eq!(samples.len(), 4, "now, -10s, -20s, -30s: {samples:?}");
        assert_eq!(d["avg_rate"], json!(1.0));
    }

    #[test]
    fn numbers_render_like_rabbitmq() {
        assert_eq!(num(3.0), json!(3));
        assert_eq!(num(0.5), json!(0.5));
        assert_eq!(round(1.26), 1.3);
    }
}
