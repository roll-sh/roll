//! Live rates: every 5 s the sampler reads the broker's counters, turns the deltas into
//! per-second rates (globally, per vhost, per queue, per exchange) and feeds metering.

use mq_core::{Broker, VHost};
use parking_lot::RwLock;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub const STEP_S: u64 = 5;
/// One hour of 5 s points.
const KEEP: usize = 720;
/// Per-queue history: 5 minutes, for the busiest queues only (bounded with millions of queues).
const QUEUE_KEEP: usize = 60;
const QUEUE_HISTORIES: usize = 5000;

/// One point of a queue's history.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct QueuePoint {
    pub t: u64,
    pub publish: f32,
    pub deliver: f32,
    pub ack: f32,
    pub ready: u64,
    pub unacked: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct RatePoint {
    pub t: u64,
    pub publish: f64,
    pub deliver: f64,
    pub ack: f64,
    pub ready: u64,
    pub unacked: u64,
    pub connections: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct QueueRates {
    pub publish: f64,
    pub deliver: f64,
    pub ack: f64,
    pub redeliver: f64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct ExchangeRates {
    pub publish_in: f64,
    pub publish_out: f64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct VHostRates {
    pub publish: f64,
    pub deliver: f64,
}

/// What a vhost holds; kept while it sleeps (its data stays on disk, but it isn't loaded).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct VHostTotals {
    pub queues: u64,
    pub messages_ready: u64,
    pub messages_unacked: u64,
    pub bytes: u64,
}

impl VHostTotals {
    fn of(vh: &VHost) -> Self {
        let (messages_ready, messages_unacked) = vh.messages();
        VHostTotals { queues: vh.queue_count() as u64, messages_ready, messages_unacked, bytes: vh.bytes_stored() }
    }
}

/// What one sample adds to the meters.
#[derive(Clone, Debug, Default)]
pub struct Delta {
    /// Per vhost: (published, delivered, stored bytes now, connection-seconds).
    pub vhosts: HashMap<String, (u64, u64, u64, f64)>,
}

/// Counter snapshot of one object; `id` changes when the object is recreated (counters restart).
#[derive(Clone, Copy, Default)]
struct Snap {
    id: usize,
    c: [u64; 4],
}

/// Per-object state keyed by vhost, then name. Lookups borrow `&str`, so a sample allocates
/// nothing for objects it has seen before: with a million idle queues a sample is a walk over
/// their counters, not a million string copies. Each entry remembers the sample that last saw
/// it, and `sweep` forgets the rest (deleted objects).
struct Nested<V> {
    map: HashMap<String, HashMap<String, (u64, V)>>,
}

impl<V> Default for Nested<V> {
    fn default() -> Self {
        Nested { map: HashMap::new() }
    }
}

impl<V> Nested<V> {
    fn get(&self, vhost: &str, name: &str) -> Option<&V> {
        self.map.get(vhost)?.get(name).map(|e| &e.1)
    }

    fn get_mut(&mut self, vhost: &str, name: &str) -> Option<&mut (u64, V)> {
        self.map.get_mut(vhost)?.get_mut(name)
    }

    /// The entry for `vhost`/`name`, created with `init` if it is new; marked as seen by `pass`.
    fn upsert(&mut self, pass: u64, vhost: &str, name: &str, init: impl FnOnce() -> V) -> &mut V {
        if !self.map.contains_key(vhost) {
            self.map.insert(vhost.to_string(), HashMap::new());
        }
        let m = self.map.get_mut(vhost).expect("just inserted");
        if !m.contains_key(name) {
            m.insert(name.to_string(), (pass, init()));
        }
        let e = m.get_mut(name).expect("just inserted");
        e.0 = pass;
        &mut e.1
    }

    fn sweep(&mut self, pass: u64) {
        self.map.retain(|_, m| {
            m.retain(|_, e| e.0 == pass);
            !m.is_empty()
        });
    }

    fn len(&self) -> usize {
        self.map.values().map(HashMap::len).sum()
    }
}

#[derive(Default)]
struct State {
    points: VecDeque<RatePoint>,
    queues: Nested<(Snap, QueueRates)>,
    exchanges: Nested<(Snap, ExchangeRates)>,
    vhosts: HashMap<String, (Snap, VHostRates)>,
    /// Last known totals of every vhost, reported for the ones that are asleep.
    last: HashMap<String, VHostTotals>,
    /// Recent points of active queues.
    queue_history: Nested<VecDeque<QueuePoint>>,
    last_ms: u64,
    /// Number of the current sample (see [`Nested`]).
    pass: u64,
}

#[derive(Default)]
pub struct Sampler {
    state: RwLock<State>,
}

fn diff(old: &Snap, id: usize, now: [u64; 4]) -> [u64; 4] {
    let mut d = [0; 4];
    for i in 0..4 {
        d[i] = if old.id == id { now[i].saturating_sub(old.c[i]) } else { now[i] };
    }
    d
}

impl Sampler {
    pub fn new() -> Arc<Self> {
        Arc::new(Sampler::default())
    }

    /// Take a sample. Returns what to meter.
    pub fn sample(&self, broker: &Broker) -> Delta {
        let now = mq_core::now_ms();
        let mut st = self.state.write();
        let first = st.last_ms == 0;
        let dt = if first { STEP_S as f64 } else { (now.saturating_sub(st.last_ms) as f64 / 1000.0).max(0.001) };
        st.last_ms = now;

        let mut delta = Delta::default();
        let mut point = RatePoint { t: now / 1000, ..Default::default() };
        st.pass += 1;
        let pass = st.pass;
        let mut seen_v = HashMap::new();
        let conns = broker.connections();
        // Connection-seconds since the last sample, each connection counted from when it opened
        // (the gap before a sample can be long: the loop parks while the broker is quiet).
        let mut conn_s_per_vhost: HashMap<&str, f64> = HashMap::new();
        for c in &conns {
            let open_s = now.saturating_sub(c.connected_at_ms) as f64 / 1000.0;
            *conn_s_per_vhost.entry(c.vhost.as_str()).or_default() += open_s.min(dt);
        }
        point.connections = conns.len() as u64;

        let st = &mut *st;
        for vh in broker.vhosts() {
            let vid = Arc::as_ptr(&vh) as usize;
            let mut v_deliver = 0u64;
            let mut totals = VHostTotals::default();
            vh.for_each_queue(|q| {
                let s = &q.shared;
                let id = Arc::as_ptr(q) as usize;
                let c = [
                    s.published.load(Ordering::Relaxed),
                    s.delivered.load(Ordering::Relaxed) + s.got.load(Ordering::Relaxed),
                    s.acked.load(Ordering::Relaxed),
                    s.redelivered.load(Ordering::Relaxed),
                ];
                let (ready, unacked) = (s.ready.load(Ordering::Relaxed), s.unacked.load(Ordering::Relaxed));
                let entry = st.queues.upsert(pass, &vh.name, &q.meta.name, Default::default);
                let d = if first { [0; 4] } else { diff(&entry.0, id, c) };
                let r = QueueRates { publish: d[0] as f64 / dt, deliver: d[1] as f64 / dt, ack: d[2] as f64 / dt, redeliver: d[3] as f64 / dt };
                *entry = (Snap { id, c }, r);
                v_deliver += d[1];
                point.deliver += d[1] as f64;
                point.ack += d[2] as f64;
                point.ready += ready;
                point.unacked += unacked;
                totals.queues += 1;
                totals.messages_ready += ready;
                totals.messages_unacked += unacked;
                let active = d[0] + d[1] + d[2] + ready + unacked > 0;
                let p = QueuePoint { t: now / 1000, publish: r.publish as f32, deliver: r.deliver as f32, ack: r.ack as f32, ready, unacked };
                let h = match st.queue_history.get_mut(&vh.name, &q.meta.name) {
                    Some(e) => {
                        e.0 = pass;
                        Some(&mut e.1)
                    }
                    None if active => Some(st.queue_history.upsert(pass, &vh.name, &q.meta.name, VecDeque::new)),
                    None => None,
                };
                if let Some(h) = h {
                    h.push_back(p);
                    if h.len() > QUEUE_KEEP {
                        h.pop_front();
                    }
                }
            });
            vh.for_each_exchange(|x| {
                let id = Arc::as_ptr(x) as usize;
                let c = [x.stats.publish_in.load(Ordering::Relaxed), x.stats.publish_out.load(Ordering::Relaxed), 0, 0];
                let entry = st.exchanges.upsert(pass, &vh.name, &x.name, Default::default);
                let d = if first { [0; 4] } else { diff(&entry.0, id, c) };
                *entry = (Snap { id, c }, ExchangeRates { publish_in: d[0] as f64 / dt, publish_out: d[1] as f64 / dt });
            });
            let c = [vh.stats.published.load(Ordering::Relaxed), 0, 0, 0];
            let old = st.vhosts.get(&vh.name).map(|e| e.0).unwrap_or_default();
            let d = if first { [0; 4] } else { diff(&old, vid, c) };
            point.publish += d[0] as f64;
            seen_v.insert(vh.name.clone(), (Snap { id: vid, c }, VHostRates { publish: d[0] as f64 / dt, deliver: v_deliver as f64 / dt }));

            let bytes = vh.bytes_stored();
            totals.bytes = bytes;
            st.last.insert(vh.name.clone(), totals);
            let conn_s = conn_s_per_vhost.get(vh.name.as_str()).copied().unwrap_or(0.0);
            delta.vhosts.insert(vh.name.clone(), (d[0], v_deliver, bytes, conn_s));
        }
        // Sleeping vhosts still store their data.
        let loaded: Vec<String> = delta.vhosts.keys().cloned().collect();
        for (info, vh) in broker.vhost_infos() {
            if vh.is_none() && !loaded.contains(&info.name) {
                let bytes = st.last.get(&info.name).map_or(0, |t| t.bytes);
                delta.vhosts.insert(info.name, (0, 0, bytes, 0.0));
            }
        }
        st.last.retain(|k, _| delta.vhosts.contains_key(k));

        point.publish /= dt;
        point.deliver /= dt;
        point.ack /= dt;
        // Forget deleted objects and, past the cap, the quietest histories.
        st.queues.sweep(pass);
        st.exchanges.sweep(pass);
        st.queue_history.sweep(pass);
        if st.queue_history.len() > QUEUE_HISTORIES {
            let mut quiet: Vec<(String, String, f64)> = st
                .queue_history
                .map
                .iter()
                .flat_map(|(v, m)| {
                    m.iter().map(move |(q, (_, h))| {
                        (v.clone(), q.clone(), h.iter().map(|p| (p.publish + p.deliver) as f64 + (p.ready + p.unacked) as f64 * 1e-6).sum())
                    })
                })
                .collect();
            quiet.sort_by(|a, b| a.2.total_cmp(&b.2));
            let excess = st.queue_history.len() - QUEUE_HISTORIES;
            for (v, q, _) in quiet.into_iter().take(excess) {
                if let Some(m) = st.queue_history.map.get_mut(&v) {
                    m.remove(&q);
                }
            }
        }
        st.vhosts = seen_v;
        st.points.push_back(point);
        while st.points.len() > KEEP {
            st.points.pop_front();
        }
        delta
    }

    pub fn points(&self, seconds: u64) -> Vec<RatePoint> {
        let st = self.state.read();
        let n = (seconds / STEP_S).max(1) as usize;
        st.points.iter().rev().take(n).rev().copied().collect()
    }

    pub fn latest(&self) -> RatePoint {
        self.state.read().points.back().copied().unwrap_or_default()
    }

    pub fn queue(&self, vhost: &str, name: &str) -> QueueRates {
        self.state.read().queues.get(vhost, name).map(|e| e.1).unwrap_or_default()
    }

    /// Recent points of a queue (empty for queues that have been idle).
    pub fn queue_history(&self, vhost: &str, name: &str, seconds: u64) -> Vec<QueuePoint> {
        let n = (seconds / STEP_S).max(1) as usize;
        let st = self.state.read();
        st.queue_history.get(vhost, name).map(|h| h.iter().rev().take(n).rev().copied().collect()).unwrap_or_default()
    }

    pub fn exchange(&self, vhost: &str, name: &str) -> ExchangeRates {
        self.state.read().exchanges.get(vhost, name).map(|e| e.1).unwrap_or_default()
    }

    pub fn vhost(&self, vhost: &str) -> VHostRates {
        self.state.read().vhosts.get(vhost).map(|e| e.1).unwrap_or_default()
    }

    /// Last known stored bytes of a vhost (also while it sleeps).
    pub fn stored_bytes(&self, vhost: &str) -> u64 {
        self.totals(vhost).bytes
    }

    /// Last known totals of a vhost: from the latest sample, or from just before it was put to
    /// sleep. Zero for a vhost never seen loaded (asleep since the node started).
    pub fn totals(&self, vhost: &str) -> VHostTotals {
        self.state.read().last.get(vhost).copied().unwrap_or_default()
    }

    /// Record a vhost's totals now (call just before suspending it, so what it holds while
    /// asleep is exact rather than up to one sample old).
    pub fn remember(&self, vh: &VHost) {
        self.state.write().last.insert(vh.name.clone(), VHostTotals::of(vh));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mq_core::log::MemStore;
    use mq_core::meta::MemMeta;
    use mq_core::vhost::{DeclareQueue, VHostInfo};
    use mq_core::{BrokerConfig, FieldTable, Message, Properties};

    #[tokio::test]
    async fn sleeping_vhosts_keep_their_last_known_totals() {
        let b = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
        let vh = b.create_vhost(VHostInfo { name: "shop".into(), ..Default::default() }).await.unwrap();
        for q in ["a", "b"] {
            let d = DeclareQueue { name: q.into(), durable: true, exclusive_owner: None, auto_delete: false, args: FieldTable::default(), passive: false };
            vh.declare_queue(d).await.unwrap();
        }
        let q = vh.queue("a").unwrap();
        for i in 0..3 {
            let m = Message::new("", "a", Properties::default(), format!("m{i}").into_bytes().into());
            q.publish_wait(Arc::new(m), None).await.unwrap();
        }
        let s = Sampler::new();
        s.sample(&b);
        let t = s.totals("shop");
        assert_eq!((t.queues, t.messages_ready, t.messages_unacked), (2, 3, 0));

        s.remember(&vh);
        drop((q, vh));
        assert!(b.suspend_vhost("shop").await);
        s.sample(&b);
        assert_eq!(s.totals("shop"), t, "kept while asleep");
        assert_eq!(s.stored_bytes("shop"), t.bytes);
        b.delete_vhost("shop").await.unwrap();
        s.sample(&b);
        assert_eq!(s.totals("shop"), VHostTotals::default(), "forgotten once deleted");
    }

    #[tokio::test]
    async fn rates_follow_queues_and_forget_deleted_ones() {
        let b = Broker::start(BrokerConfig::default(), MemMeta::new(), MemStore::new()).await.unwrap();
        let vh = b.open_vhost("/").await.unwrap();
        for q in ["busy", "idle"] {
            let d = DeclareQueue { name: q.into(), durable: true, exclusive_owner: None, auto_delete: false, args: FieldTable::default(), passive: false };
            vh.declare_queue(d).await.unwrap();
        }
        let s = Sampler::new();
        s.sample(&b);
        let q = vh.queue("busy").unwrap();
        for i in 0..10 {
            let m = Message::new("", "busy", Properties::default(), format!("m{i}").into_bytes().into());
            q.publish_wait(Arc::new(m), None).await.unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        s.sample(&b);
        assert!(s.queue("/", "busy").publish > 0.0, "publish rate");
        assert_eq!(s.queue("/", "idle").publish, 0.0);
        assert_eq!(s.queue_history("/", "busy", 300).last().map(|p| p.ready), Some(10));
        assert!(s.queue_history("/", "idle", 300).is_empty(), "idle queues keep no history");

        // The next sample sees no new publishes: the rate drops to zero, the history goes on.
        std::thread::sleep(std::time::Duration::from_millis(20));
        s.sample(&b);
        assert_eq!(s.queue("/", "busy").publish, 0.0);
        assert_eq!(s.queue_history("/", "busy", 300).len(), 2);

        drop(q);
        vh.delete_queue("busy", false, false, None).await.unwrap();
        s.sample(&b);
        let st = s.state.read();
        assert!(st.queues.get("/", "busy").is_none() && st.queue_history.get("/", "busy").is_none(), "deleted queues are forgotten");
        assert!(st.queues.get("/", "idle").is_some());
    }
}
