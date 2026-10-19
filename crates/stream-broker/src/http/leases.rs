//! Server-managed consumer groups for HTTP clients.
//!
//! Partitions are leased to consumers: each read renews the reader's leases and claims a fair
//! share (`ceil(partitions / live consumers)`) of free ones, so consumers can come and go
//! between requests with no join/sync protocol. Offsets go to the same table Kafka groups
//! use, so `kafka-consumer-groups` shows HTTP groups and their lag.
//!
//! Commit modes:
//! - `auto` (default): what a consumer received is committed on its next read or when it
//!   leaves — at-least-once, like the Kafka client's auto-commit. If the consumer vanishes,
//!   its leases expire and the records are delivered again.
//! - `read`: committed as delivered (at-most-once).
//! - `manual`: the client POSTs `/groups/{g}/commit`.

use super::data::{ReadQuery, read_many};
use super::{Api, ApiError, ApiResult};
use crate::broker::{Stream, Topic};
use crate::error::KafkaError;
use crate::ids::{now, random_string};
use crate::store::OffsetRow;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_LEASE_MS: u64 = 30_000;
const MAX_LEASE_MS: u64 = 600_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Read,
    Manual,
}

struct Lease {
    consumer: String,
    until: Instant,
    /// Next offset to deliver; `None` until resolved from the committed offset.
    pos: Option<i64>,
    /// Delivered but not yet committed (`auto` mode).
    pending: Option<i64>,
    /// A read is in progress; the lease can't move.
    busy: bool,
}

#[derive(Default)]
struct Group {
    /// `(topic, partition) → lease`.
    leases: HashMap<(String, i32), Lease>,
    /// Consumers and when their lease window ends.
    consumers: HashMap<String, Instant>,
}

impl Group {
    fn expire(&mut self, now: Instant) {
        self.leases.retain(|_, l| l.busy || l.until > now);
        self.consumers.retain(|_, until| *until > now);
    }

    /// Pending positions of `consumer`, taken for committing.
    fn take_pending(&mut self, consumer: &str) -> Vec<(String, i32, i64)> {
        self.leases
            .iter_mut()
            .filter(|(_, l)| l.consumer == consumer)
            .filter_map(|((t, p), l)| l.pending.take().map(|o| (t.clone(), *p, o)))
            .collect()
    }
}

#[derive(Default)]
pub struct Leases(Mutex<HashMap<(String, String), Group>>);

fn rows(items: Vec<(String, i32, i64)>) -> Vec<OffsetRow> {
    let at = now();
    items.into_iter().map(|(topic, partition, offset)| OffsetRow { topic, partition, offset, leader_epoch: -1, metadata: None, committed_at: at }).collect()
}

impl Leases {
    /// Renew `consumer`'s leases on `topic` and claim its share; returns `(partition, position)`
    /// to read (marked busy) and offsets to commit first.
    #[allow(clippy::too_many_arguments)]
    fn claim(&self, stream: &str, group: &str, consumer: &str, topic: &str, partitions: i32, lease: Duration, mode: Mode) -> (Vec<(i32, Option<i64>)>, Vec<OffsetRow>) {
        let now = Instant::now();
        let mut map = self.0.lock().unwrap();
        let g = map.entry((stream.to_string(), group.to_string())).or_default();
        g.expire(now);
        let commit = if mode == Mode::Auto { g.take_pending(consumer) } else { vec![] };
        g.consumers.insert(consumer.to_string(), now + lease);
        let fair = (partitions as usize).div_ceil(g.consumers.len().max(1));

        // Partitions held by this consumer; busy ones belong to its concurrent reads.
        let held = g.leases.iter().filter(|((t, _), l)| t == topic && l.consumer == consumer).count();
        let mut mine: Vec<i32> = g.leases.iter().filter(|((t, _), l)| t == topic && l.consumer == consumer && !l.busy).map(|((_, p), _)| *p).collect();
        mine.sort_unstable();
        // Others joined: give back the extras (their pending offsets were just committed).
        let mut extra = held.saturating_sub(fair);
        while extra > 0
            && let Some(p) = mine.pop()
        {
            g.leases.remove(&(topic.to_string(), p));
            extra -= 1;
        }
        let mut want = fair.saturating_sub(held);
        if want > 0 {
            let start = crate::partitioner::partition_for_key(consumer.as_bytes(), partitions.max(1));
            for i in 0..partitions {
                if want == 0 {
                    break;
                }
                let p = (start + i) % partitions;
                if let std::collections::hash_map::Entry::Vacant(e) = g.leases.entry((topic.to_string(), p)) {
                    e.insert(Lease { consumer: consumer.to_string(), until: now + lease, pos: None, pending: None, busy: false });
                    mine.push(p);
                    want -= 1;
                }
            }
        }
        mine.sort_unstable();
        let out = mine
            .into_iter()
            .map(|p| {
                let l = g.leases.get_mut(&(topic.to_string(), p)).expect("leased");
                l.busy = true;
                l.until = now + lease;
                (p, l.pos)
            })
            .collect();
        (out, rows(commit))
    }

    /// A read finished: store positions, release the busy flag; returns rows to commit now.
    #[allow(clippy::too_many_arguments)]
    fn finish(&self, stream: &str, group: &str, consumer: &str, topic: &str, pos: &BTreeMap<i32, i64>, lease: Duration, mode: Mode) -> Vec<OffsetRow> {
        let now = Instant::now();
        let mut map = self.0.lock().unwrap();
        let Some(g) = map.get_mut(&(stream.to_string(), group.to_string())) else { return vec![] };
        let mut commit = vec![];
        for (p, o) in pos {
            let Some(l) = g.leases.get_mut(&(topic.to_string(), *p)) else { continue };
            if l.consumer != consumer {
                continue;
            }
            let moved = l.pos != Some(*o);
            l.busy = false;
            l.until = now + lease;
            l.pos = Some(*o);
            match mode {
                Mode::Auto if moved => l.pending = Some(*o),
                Mode::Read if moved => commit.push((topic.to_string(), *p, *o)),
                _ => {}
            }
        }
        rows(commit)
    }

    /// Release busy flags after a failed read.
    fn abort(&self, stream: &str, group: &str, topic: &str, parts: &[i32]) {
        let mut map = self.0.lock().unwrap();
        if let Some(g) = map.get_mut(&(stream.to_string(), group.to_string())) {
            for p in parts {
                if let Some(l) = g.leases.get_mut(&(topic.to_string(), *p)) {
                    l.busy = false;
                }
            }
        }
    }

    /// Offsets were committed explicitly: nothing is pending below them any more.
    pub fn committed(&self, stream: &str, group: &str, consumer: Option<&str>, committed: &[OffsetRow]) {
        let mut map = self.0.lock().unwrap();
        let Some(g) = map.get_mut(&(stream.to_string(), group.to_string())) else { return };
        for r in committed {
            if let Some(l) = g.leases.get_mut(&(r.topic.clone(), r.partition))
                && consumer.is_none_or(|c| c == l.consumer)
                && l.pending.is_some_and(|p| p <= r.offset)
            {
                l.pending = None;
            }
        }
    }

    /// A consumer leaves: its pending offsets (to commit) and its partitions are released.
    pub fn leave(&self, stream: &str, group: &str, consumer: &str) -> Vec<OffsetRow> {
        let mut map = self.0.lock().unwrap();
        let Some(g) = map.get_mut(&(stream.to_string(), group.to_string())) else { return vec![] };
        let pending = g.take_pending(consumer);
        g.leases.retain(|_, l| l.consumer != consumer || l.busy);
        g.consumers.remove(consumer);
        rows(pending)
    }

    pub fn consumers(&self, stream: &str, group: &str) -> Vec<Value> {
        let now = Instant::now();
        let mut map = self.0.lock().unwrap();
        let Some(g) = map.get_mut(&(stream.to_string(), group.to_string())) else { return vec![] };
        g.expire(now);
        let mut by: BTreeMap<String, Vec<Value>> = g.consumers.keys().map(|c| (c.clone(), vec![])).collect();
        for ((t, p), l) in &g.leases {
            by.entry(l.consumer.clone()).or_default().push(json!({ "topic": t, "partition": p, "position": l.pos, "pending_commit": l.pending }));
        }
        by.into_iter()
            .map(|(c, parts)| {
                let left = g.consumers.get(&c).map(|u| u.saturating_duration_since(now).as_millis() as u64).unwrap_or(0);
                json!({ "consumer": c, "partitions": parts, "lease_remaining_ms": left })
            })
            .collect()
    }

    pub fn groups(&self, stream: &str) -> Vec<String> {
        let now = Instant::now();
        let mut map = self.0.lock().unwrap();
        map.iter_mut()
            .filter(|((s, _), _)| s == stream)
            .filter_map(|((_, g), group)| {
                group.expire(now);
                (!group.consumers.is_empty()).then(|| g.clone())
            })
            .collect()
    }

    pub fn is_active(&self, stream: &str, group: &str) -> bool {
        let now = Instant::now();
        let mut map = self.0.lock().unwrap();
        map.get_mut(&(stream.to_string(), group.to_string())).is_some_and(|g| {
            g.expire(now);
            !g.consumers.is_empty()
        })
    }

    pub fn drop_group(&self, stream: &str, group: &str) {
        self.0.lock().unwrap().remove(&(stream.to_string(), group.to_string()));
    }

    pub fn drop_stream(&self, stream: &str) {
        self.0.lock().unwrap().retain(|(s, _), _| s != stream);
    }

    pub fn topic_deleted(&self, stream: &str, topic: &str) {
        for ((s, _), g) in self.0.lock().unwrap().iter_mut() {
            if s == stream {
                g.leases.retain(|(t, _), _| t != topic);
            }
        }
    }
}

/// `GET .../topics/{t}/records?group=...`.
pub async fn consume(api: &Api, stream: &Stream, topic: &Topic, group: &str, q: &ReadQuery) -> ApiResult<Value> {
    if group.is_empty() || group.len() > 255 {
        return Err(ApiError::bad("group ids are 1-255 characters"));
    }
    if api.b.groups.is_active(&stream.id, group) {
        return Err(ApiError::conflict(format!("group {group} has Kafka protocol members; use another group id for HTTP consumers")));
    }
    let mode = match q.commit.as_deref().unwrap_or("auto") {
        "auto" => Mode::Auto,
        "read" => Mode::Read,
        "manual" => Mode::Manual,
        other => return Err(ApiError::bad(format!("commit must be auto, read or manual, not {other}"))),
    };
    let reset_latest = match q.reset.as_deref().unwrap_or("earliest") {
        "earliest" => false,
        "latest" => true,
        other => return Err(ApiError::bad(format!("reset must be earliest or latest, not {other}"))),
    };
    let committed = q.committed()?;
    let lease = Duration::from_millis(q.lease_ms.unwrap_or(DEFAULT_LEASE_MS).clamp(1_000, MAX_LEASE_MS));
    let consumer = q.consumer.clone().unwrap_or_else(|| format!("http-{}", random_string(12)));
    let b = &api.b;

    let (claimed, first_commit) = api.leases.claim(&stream.id, group, &consumer, &topic.name, topic.partition_count(), lease, mode);
    if !first_commit.is_empty() {
        b.store.commit_offsets(&stream.id, group, &first_commit).map_err(KafkaError::internal)?;
    }
    let parts: Vec<i32> = claimed.iter().map(|(p, _)| *p).collect();
    let result = async {
        let mut pos = BTreeMap::new();
        let stored: Option<HashMap<i32, i64>> = if claimed.iter().any(|(_, o)| o.is_none()) {
            Some(b.store.offsets(&stream.id, group).map_err(KafkaError::internal)?.into_iter().filter(|r| r.topic == topic.name).map(|r| (r.partition, r.offset)).collect())
        } else {
            None
        };
        for (p, known) in &claimed {
            let o = match known {
                Some(o) => *o,
                None => {
                    let part = topic.partition(*p).ok_or_else(|| ApiError::not_found("partition vanished"))?;
                    let log = b.log(topic, &part)?;
                    match stored.as_ref().and_then(|m| m.get(p)) {
                        Some(o) => (*o).clamp(log.log_start(), log.high_watermark()),
                        None if reset_latest => if committed { log.last_stable() } else { log.high_watermark() },
                        None => log.log_start(),
                    }
                }
            };
            pos.insert(*p, o);
        }
        let records = if pos.is_empty() {
            // More consumers than partitions: wait out the poll so idle consumers don't spin.
            tokio::time::sleep(q.wait()).await;
            vec![]
        } else {
            read_many(api, stream, topic, &mut pos, q.opts()?, q.wait()).await?
        };
        Ok::<_, ApiError>((records, pos))
    }
    .await;
    let (records, pos) = match result {
        Ok(x) => x,
        Err(e) => {
            api.leases.abort(&stream.id, group, &topic.name, &parts);
            return Err(e);
        }
    };
    let now_commit = api.leases.finish(&stream.id, group, &consumer, &topic.name, &pos, lease, mode);
    if !now_commit.is_empty() {
        b.store.commit_offsets(&stream.id, group, &now_commit).map_err(KafkaError::internal)?;
    }
    let commit: Vec<Value> = pos.iter().map(|(p, o)| json!({ "topic": topic.name, "partition": p, "offset": o })).collect();
    Ok(json!({
        "topic": topic.name,
        "group": group,
        "consumer": consumer,
        "partitions": parts,
        "records": records,
        "commit": commit,
        "lease_ms": lease.as_millis() as u64,
    }))
}
