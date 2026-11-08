//! The in-process backend: membership and leases in a [`MemoryStore`] that nodes of one
//! process share. A node alone on its store (the default) wins every lease it asks for and never
//! loses one, so everything it touches is its own; several nodes on one store (tests) behave as
//! on Postgres, with the process's monotonic clock as the shared clock.

use super::{Backend, Confirmed, Holder, Member, Registration, Timing};
use crate::{Error, Node, Result, clock};
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

struct MemNode {
    url: String,
    zone: Option<String>,
    incarnation: u64,
    heartbeat_at: Duration,
    draining: bool,
}

struct MemLease {
    node: String,
    epoch: i64,
    expires_at: Duration,
    /// Its holder released it (a handoff), rather than letting it lapse.
    released: bool,
}

#[derive(Default)]
struct Tables {
    nodes: HashMap<String, MemNode>,
    leases: BTreeMap<String, MemLease>,
}

/// Membership and leases shared by the nodes of one process. Cheap to clone.
#[derive(Clone, Default)]
pub struct MemoryStore(Arc<Mutex<Tables>>);

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let t = self.0.lock();
        f.debug_struct("MemoryStore").field("nodes", &t.nodes.len()).field("leases", &t.leases.len()).finish()
    }
}

impl MemoryStore {
    pub fn new() -> MemoryStore {
        MemoryStore::default()
    }
}

/// One node's view of a [`MemoryStore`].
pub struct Memory {
    store: MemoryStore,
    me: Mutex<Option<(Registration, Timing, u64)>>,
    /// Sweep cursor: the last key looked at.
    cursor: Mutex<Option<String>>,
}

static INCARNATIONS: AtomicU64 = AtomicU64::new(1);

impl Memory {
    pub fn new(store: MemoryStore) -> Memory {
        Memory { store, me: Mutex::new(None), cursor: Mutex::new(None) }
    }

    /// A node alone on a store of its own.
    pub fn solo() -> Memory {
        Memory::new(MemoryStore::new())
    }

    fn me(&self) -> (String, Timing, u64) {
        let me = self.me.lock();
        let (r, t, i) = me.as_ref().expect("joined");
        (r.node_id.clone(), *t, *i)
    }
}

impl Backend for Memory {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn join(&self, reg: &Registration, t: Timing) -> Result<()> {
        let deadline = Instant::now() + t.node_ttl + t.lease_ttl + Duration::from_secs(1);
        loop {
            {
                let now = clock::now();
                let mut tb = self.store.0.lock();
                let alive = tb.nodes.get(&reg.node_id).is_some_and(|n| n.heartbeat_at + t.node_ttl > now);
                let held = tb.leases.values().any(|l| l.node == reg.node_id && l.expires_at > now);
                if !alive && !held {
                    let incarnation = INCARNATIONS.fetch_add(1, Ordering::Relaxed);
                    tb.nodes.insert(
                        reg.node_id.clone(),
                        MemNode { url: reg.url.clone(), zone: reg.zone.clone(), incarnation, heartbeat_at: now, draining: false },
                    );
                    // Disown what a previous process with this id left, so retaking it bumps the epoch.
                    for l in tb.leases.values_mut().filter(|l| l.node == reg.node_id) {
                        l.node.clear();
                    }
                    *self.me.lock() = Some((reg.clone(), t, incarnation));
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(Error::NodeIdInUse(reg.node_id.clone()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn heartbeat(&self, draining: bool) -> Result<()> {
        let (id, _, inc) = self.me();
        let reg = self.me.lock().as_ref().map(|(r, ..)| r.clone()).expect("joined");
        let mut tb = self.store.0.lock();
        match tb.nodes.get_mut(&id) {
            Some(n) if n.incarnation == inc => {
                n.heartbeat_at = clock::now();
                Ok(())
            }
            Some(_) => Err(Error::Superseded),
            None => {
                tb.nodes.insert(id, MemNode { url: reg.url, zone: reg.zone, incarnation: inc, heartbeat_at: clock::now(), draining });
                Ok(())
            }
        }
    }

    fn members(&self) -> Result<Vec<Member>> {
        let (_, t, _) = self.me();
        let now = clock::now();
        let tb = self.store.0.lock();
        let mut v: Vec<Member> = tb
            .nodes
            .iter()
            .filter(|(_, n)| n.heartbeat_at + t.node_ttl > now)
            .map(|(id, n)| Member { node: Node { id: id.clone(), url: n.url.clone() }, zone: n.zone.clone(), draining: n.draining })
            .collect();
        v.sort_by(|a, b| a.node.id.cmp(&b.node.id));
        Ok(v)
    }

    fn set_draining(&self) -> Result<()> {
        let (id, _, inc) = self.me();
        if let Some(n) = self.store.0.lock().nodes.get_mut(&id).filter(|n| n.incarnation == inc) {
            n.draining = true;
        }
        Ok(())
    }

    fn renew(&self) -> Result<Confirmed> {
        let (id, t, inc) = self.me();
        let basis = clock::now();
        let mut tb = self.store.0.lock();
        if tb.nodes.get(&id).is_none_or(|n| n.incarnation != inc) {
            return Ok(Confirmed { leases: Vec::new(), basis, taken_over: 0 });
        }
        let now = clock::now();
        let mut leases = Vec::new();
        for (k, l) in tb.leases.iter_mut().filter(|(_, l)| l.node == id && l.expires_at > now) {
            l.expires_at = now + t.lease_ttl;
            leases.push((k.clone(), l.epoch));
        }
        Ok(Confirmed { leases, basis, taken_over: 0 })
    }

    fn acquire(&self, keys: &[String]) -> Result<Confirmed> {
        let (id, t, inc) = self.me();
        let basis = clock::now();
        let mut tb = self.store.0.lock();
        if tb.nodes.get(&id).is_none_or(|n| n.incarnation != inc) {
            return Ok(Confirmed { leases: Vec::new(), basis, taken_over: 0 });
        }
        let now = clock::now();
        let (mut leases, mut taken_over) = (Vec::new(), 0);
        for k in keys {
            match tb.leases.get_mut(k) {
                None => {
                    tb.leases.insert(k.clone(), MemLease { node: id.clone(), epoch: 1, expires_at: now + t.lease_ttl, released: false });
                    leases.push((k.clone(), 1));
                }
                Some(l) if l.node == id || l.expires_at <= now => {
                    if l.node != id {
                        taken_over += usize::from(!l.released);
                        l.epoch += 1;
                        l.node = id.clone();
                    }
                    l.expires_at = now + t.lease_ttl;
                    l.released = false;
                    leases.push((k.clone(), l.epoch));
                }
                Some(_) => {}
            }
        }
        Ok(Confirmed { leases, basis, taken_over })
    }

    fn release(&self, leases: &[(String, i64)]) -> Result<()> {
        let (id, ..) = self.me();
        let now = clock::now();
        let mut tb = self.store.0.lock();
        for (k, e) in leases {
            if let Some(l) = tb.leases.get_mut(k).filter(|l| l.node == id && l.epoch == *e) {
                l.expires_at = now.saturating_sub(Duration::from_millis(1));
                l.released = true;
            }
        }
        Ok(())
    }

    fn delete(&self, leases: &[(String, i64)]) -> Result<()> {
        let (id, ..) = self.me();
        let mut tb = self.store.0.lock();
        for (k, e) in leases {
            if tb.leases.get(k).is_some_and(|l| l.node == id && l.epoch == *e) {
                tb.leases.remove(k);
            }
        }
        Ok(())
    }

    fn sweep(&self, page: usize) -> Result<(Vec<String>, bool)> {
        let now = clock::now();
        let tb = self.store.0.lock();
        let mut cursor = self.cursor.lock();
        let from = cursor.clone();
        let range = tb.leases.iter().filter(|(k, _)| from.as_ref().is_none_or(|c| *k > c));
        let mut keys = Vec::new();
        let mut last = None;
        let mut more = false;
        for (k, l) in range {
            if keys.len() == page {
                more = true;
                break;
            }
            last = Some(k.clone());
            if l.expires_at <= now {
                keys.push(k.clone());
            }
        }
        *cursor = if more { last } else { None };
        Ok((keys, more))
    }

    fn lookup(&self, key: &str) -> Result<Holder> {
        let (id, t, _) = self.me();
        let now = clock::now();
        let tb = self.store.0.lock();
        let Some(l) = tb.leases.get(key).filter(|l| l.expires_at > now) else { return Ok(Holder::Free) };
        let left = (l.expires_at - now).max(Duration::from_millis(1));
        if l.node == id {
            return Ok(Holder::Me { left });
        }
        let n = tb.nodes.get(&l.node);
        Ok(Holder::Other {
            id: l.node.clone(),
            url: n.map(|n| n.url.clone()),
            live: n.is_some_and(|n| n.heartbeat_at + t.node_ttl > now),
            left,
        })
    }

    fn leave(&self, _released_all: bool) -> Result<()> {
        let (id, _, inc) = self.me();
        let mut tb = self.store.0.lock();
        if tb.nodes.get(&id).is_some_and(|n| n.incarnation == inc) {
            tb.nodes.remove(&id);
        }
        Ok(())
    }

    fn cleanup(&self) -> Result<()> {
        let (_, t, _) = self.me();
        let age = (t.node_ttl * 12).max(Duration::from_secs(60));
        let now = clock::now();
        self.store.0.lock().nodes.retain(|_, n| n.heartbeat_at + age > now);
        Ok(())
    }

    fn holder(&self, key: &str) -> Result<Option<(String, i64)>> {
        Ok(self.store.0.lock().leases.get(key).map(|l| (l.node.clone(), l.epoch)))
    }
}
