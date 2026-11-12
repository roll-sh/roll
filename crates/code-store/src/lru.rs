//! A small byte-budgeted LRU for immutable, content-addressed values.

use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;

pub struct Lru<K, V> {
    inner: Mutex<Inner<K, V>>,
    budget: usize,
}

struct Inner<K, V> {
    map: HashMap<K, (V, usize, u64)>,
    order: BTreeMap<u64, K>,
    tick: u64,
    bytes: usize,
}

impl<K: Hash + Eq + Clone, V: Clone> Lru<K, V> {
    pub fn new(budget: usize) -> Self {
        Lru { inner: Mutex::new(Inner { map: HashMap::new(), order: BTreeMap::new(), tick: 0, bytes: 0 }), budget }
    }

    pub fn get(&self, k: &K) -> Option<V> {
        let mut g = self.inner.lock();
        g.tick += 1;
        let tick = g.tick;
        let (v, old) = {
            let e = g.map.get_mut(k)?;
            let old = std::mem::replace(&mut e.2, tick);
            (e.0.clone(), old)
        };
        g.order.remove(&old);
        g.order.insert(tick, k.clone());
        Some(v)
    }

    pub fn insert(&self, k: K, v: V, size: usize) {
        if size > self.budget / 4 {
            return;
        }
        let mut g = self.inner.lock();
        g.tick += 1;
        let tick = g.tick;
        if let Some((_, s, t)) = g.map.insert(k.clone(), (v, size, tick)) {
            g.order.remove(&t);
            g.bytes -= s;
        }
        g.order.insert(tick, k);
        g.bytes += size;
        while g.bytes > self.budget {
            let Some((_, k)) = g.order.pop_first() else { break };
            if let Some((_, s, _)) = g.map.remove(&k) {
                g.bytes -= s;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_within_the_budget() {
        let l: Lru<u32, &str> = Lru::new(100);
        l.insert(1, "a", 20);
        l.insert(2, "b", 20);
        l.insert(3, "c", 20);
        assert_eq!(l.get(&1), Some("a"), "touching 1 makes 2 the oldest");
        l.insert(4, "d", 20);
        l.insert(5, "e", 20);
        l.insert(6, "f", 20);
        assert_eq!(l.get(&2), None);
        assert_eq!(l.get(&1), Some("a"));
        assert_eq!(l.get(&6), Some("f"));
        // Replacing a key frees its old size.
        l.insert(6, "g", 5);
        assert_eq!(l.get(&6), Some("g"));
        // Values larger than a quarter of the budget aren't cached at all.
        l.insert(7, "big", 26);
        assert_eq!(l.get(&7), None);
        let bytes = l.inner.lock().bytes;
        assert!(bytes <= 100, "{bytes}");
    }
}
