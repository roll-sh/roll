//! Pub/Sub for one database (shared by all of its shards): channels, patterns, shard channels
//! and MONITOR feeds. Messages are encoded once per protocol version and handed to client sinks.

use crate::util::glob_match;
use bytes::Bytes;
use parking_lot::Mutex;
use redis_proto::Reply;
use std::collections::HashMap;
use std::sync::Arc;

/// Where out-of-band data for a client goes (its connection writer).
pub trait Sink: Send + Sync {
    /// Pub/Sub messages, invalidations, MONITOR lines.
    fn push(&self, msg: Bytes);
    /// The reply to a command the client was blocked on.
    fn unblock(&self, reply: Bytes);
    /// The client's ACL user was deleted or lost channel permissions: the connection should
    /// check [`crate::acl::must_disconnect`].
    fn acl_changed(&self) {}
}

#[derive(Clone)]
pub struct Sub {
    pub sink: Arc<dyn Sink>,
    pub resp: u8,
}

type Subs = HashMap<u64, Sub>;

#[derive(Default)]
struct Inner {
    channels: HashMap<Bytes, Subs>,
    patterns: HashMap<Bytes, Subs>,
    shard: HashMap<Bytes, Subs>,
    monitors: Subs,
}

#[derive(Default)]
pub struct Hub {
    inner: Mutex<Inner>,
}

/// Encode a push lazily, once per protocol version.
struct Encoded {
    items: Vec<Reply>,
    v2: Option<Bytes>,
    v3: Option<Bytes>,
}

impl Encoded {
    fn new(items: Vec<Reply>) -> Self {
        Self { items, v2: None, v3: None }
    }
    fn get(&mut self, resp: u8) -> Bytes {
        let slot = if resp >= 3 { &mut self.v3 } else { &mut self.v2 };
        slot.get_or_insert_with(|| Reply::Push(self.items.clone()).to_bytes(resp)).clone()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Channel,
    Pattern,
    Shard,
}

impl Hub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn map<'a>(inner: &'a mut Inner, kind: &Kind) -> &'a mut HashMap<Bytes, Subs> {
        match kind {
            Kind::Channel => &mut inner.channels,
            Kind::Pattern => &mut inner.patterns,
            Kind::Shard => &mut inner.shard,
        }
    }

    /// Returns true if newly subscribed.
    pub fn subscribe(&self, kind: Kind, name: &Bytes, client: u64, sub: Sub) -> bool {
        let mut inner = self.inner.lock();
        Self::map(&mut inner, &kind).entry(name.clone()).or_default().insert(client, sub).is_none()
    }

    pub fn unsubscribe(&self, kind: Kind, name: &[u8], client: u64) -> bool {
        let mut inner = self.inner.lock();
        let map = Self::map(&mut inner, &kind);
        let Some(subs) = map.get_mut(name) else { return false };
        let removed = subs.remove(&client).is_some();
        if subs.is_empty() {
            map.remove(name);
        }
        removed
    }

    /// `PUBLISH`: number of clients that received the message.
    pub fn publish(&self, channel: &[u8], msg: &[u8]) -> usize {
        let inner = self.inner.lock();
        let ch = Bytes::copy_from_slice(channel);
        let msg = Bytes::copy_from_slice(msg);
        let mut n = 0;
        if let Some(subs) = inner.channels.get(channel) {
            let mut e = Encoded::new(vec![Reply::bulk("message"), Reply::Bulk(ch.clone()), Reply::Bulk(msg.clone())]);
            for s in subs.values() {
                s.sink.push(e.get(s.resp));
                n += 1;
            }
        }
        for (pat, subs) in &inner.patterns {
            if glob_match(pat, channel, false) {
                let mut e = Encoded::new(vec![
                    Reply::bulk("pmessage"),
                    Reply::Bulk(pat.clone()),
                    Reply::Bulk(ch.clone()),
                    Reply::Bulk(msg.clone()),
                ]);
                for s in subs.values() {
                    s.sink.push(e.get(s.resp));
                    n += 1;
                }
            }
        }
        n
    }

    /// `SPUBLISH`.
    pub fn spublish(&self, channel: &[u8], msg: &[u8]) -> usize {
        let inner = self.inner.lock();
        let Some(subs) = inner.shard.get(channel) else { return 0 };
        let mut e = Encoded::new(vec![Reply::bulk("smessage"), Reply::bulk(channel), Reply::bulk(msg)]);
        for s in subs.values() {
            s.sink.push(e.get(s.resp));
        }
        subs.len()
    }

    /// Is anyone listening at all? (Lets keyspace notifications skip formatting.)
    pub fn has_subscribers(&self) -> bool {
        let inner = self.inner.lock();
        !inner.channels.is_empty() || !inner.patterns.is_empty()
    }

    pub fn numsub(&self, kind: Kind, name: &[u8]) -> usize {
        let mut inner = self.inner.lock();
        Self::map(&mut inner, &kind).get(name).map_or(0, |s| s.len())
    }

    /// Number of distinct patterns (`PUBSUB NUMPAT`).
    pub fn numpat(&self) -> usize {
        self.inner.lock().patterns.len()
    }

    /// Active channels matching an optional pattern.
    pub fn channels(&self, kind: Kind, pattern: Option<&[u8]>) -> Vec<Bytes> {
        let mut inner = self.inner.lock();
        Self::map(&mut inner, &kind)
            .keys()
            .filter(|c| pattern.is_none_or(|p| glob_match(p, c, false)))
            .cloned()
            .collect()
    }

    pub fn add_monitor(&self, client: u64, sub: Sub) {
        self.inner.lock().monitors.insert(client, sub);
    }

    pub fn remove_monitor(&self, client: u64) {
        self.inner.lock().monitors.remove(&client);
    }

    pub fn has_monitors(&self) -> bool {
        !self.inner.lock().monitors.is_empty()
    }

    /// Feed a MONITOR line (`+1700000000.123456 [0 127.0.0.1:5000] "SET" "k" "v"`).
    pub fn monitor(&self, line: &str) {
        let inner = self.inner.lock();
        if inner.monitors.is_empty() {
            return;
        }
        let bytes = Reply::Status(line.to_string()).to_bytes(2);
        for s in inner.monitors.values() {
            s.sink.push(bytes.clone());
        }
    }

    /// Drop every subscription of a disconnected client.
    pub fn remove_client(&self, client: u64) {
        let mut guard = self.inner.lock();
        let Inner { channels, patterns, shard, monitors } = &mut *guard;
        for map in [channels, patterns, shard] {
            map.retain(|_, subs| {
                subs.remove(&client);
                !subs.is_empty()
            });
        }
        monitors.remove(&client);
    }
}

/// A sink that records what it receives; for tests and embedding.
#[derive(Default)]
pub struct VecSink {
    pub pushes: Mutex<Vec<Bytes>>,
    pub unblocks: Mutex<Vec<Bytes>>,
}

impl Sink for VecSink {
    fn push(&self, msg: Bytes) {
        self.pushes.lock().push(msg);
    }
    fn unblock(&self, reply: Bytes) {
        self.unblocks.lock().push(reply);
    }
}
