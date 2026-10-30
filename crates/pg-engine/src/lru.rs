//! Bounded caches shared by every session of the process, for results that are a pure
//! function of their key (parse trees by text, ...): what PostgreSQL computes again in each
//! backend, computed once here. Each is bounded by bytes and evicts by CLOCK (second
//! chance, an approximation of least recently used); `footprints` reports them all.

use parking_lot::Mutex;
use std::borrow::Borrow;
use std::hash::Hash;
use std::sync::Arc;

/// A cache's size and use, as `footprints` reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    pub entries: usize,
    /// The entries' estimated bytes (the estimates the callers give).
    pub bytes: usize,
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
}

/// A process-wide cache of at most `capacity` estimated bytes.
pub struct Shared<K, V> {
    name: &'static str,
    /// (`set_capacity`: the engine's options size it.)
    capacity: std::sync::atomic::AtomicUsize,
    inner: Mutex<Option<Clock<K, V>>>,
}

struct Slot<K, V> {
    key: Arc<K>,
    value: V,
    bytes: usize,
    used: bool,
}

struct Clock<K, V> {
    map: hashbrown::HashMap<Arc<K>, usize>,
    slots: Vec<Option<Slot<K, V>>>,
    free: Vec<usize>,
    hand: usize,
    use_: Footprint,
}

impl<K: Hash + Eq, V: Clone> Shared<K, V> {
    pub const fn new(name: &'static str, capacity: usize) -> Self {
        Shared {
            name,
            capacity: std::sync::atomic::AtomicUsize::new(capacity),
            inner: Mutex::new(None),
        }
    }

    /// The value cached for `key`, marked as used.
    pub fn get<Q>(&self, key: &Q) -> Option<V>
    where
        Arc<K>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let mut g = self.inner.lock();
        let c = g.as_mut()?;
        match c.map.get(key) {
            Some(&i) => {
                let s = c.slots[i].as_mut().expect("a mapped slot");
                s.used = true;
                c.use_.hits += 1;
                Some(s.value.clone())
            }
            None => {
                c.use_.misses += 1;
                None
            }
        }
    }

    /// Cache `value` for `key`, estimated at `bytes`. Returns what it evicted, for the
    /// caller to drop outside the lock (deep trees drop with stack of their own). A value
    /// bigger than an eighth of the capacity isn't kept (it is handed back), nor one whose
    /// key another session cached meanwhile.
    pub fn insert(&self, key: K, value: V, bytes: usize) -> Vec<V> {
        let mut out = Vec::new();
        let capacity = self.capacity();
        if bytes > capacity / 8 {
            out.push(value);
            return out;
        }
        let mut g = self.inner.lock();
        let c = g.get_or_insert_with(|| Clock {
            map: hashbrown::HashMap::new(),
            slots: Vec::new(),
            free: Vec::new(),
            hand: 0,
            use_: Footprint::default(),
        });
        if c.map.contains_key(&key) {
            out.push(value);
            return out;
        }
        while c.use_.bytes + bytes > capacity && c.use_.entries > 0 {
            out.push(c.evict());
        }
        let key = Arc::new(key);
        let slot = Slot {
            key: key.clone(),
            value,
            bytes,
            used: false,
        };
        let i = match c.free.pop() {
            Some(i) => {
                c.slots[i] = Some(slot);
                i
            }
            None => {
                c.slots.push(Some(slot));
                c.slots.len() - 1
            }
        };
        c.map.insert(key, i);
        c.use_.entries += 1;
        c.use_.bytes += bytes;
        out
    }

    /// Drop every entry; returns them, as `insert` returns what it evicts.
    pub fn clear(&self) -> Vec<V> {
        let mut g = self.inner.lock();
        let Some(c) = g.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while c.use_.entries > 0 {
            out.push(c.evict());
        }
        out
    }

    pub fn capacity(&self) -> usize {
        self.capacity.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Bound the cache at `bytes` from now on; returns what that evicts, as `insert` does.
    pub fn set_capacity(&self, bytes: usize) -> Vec<V> {
        self.capacity.store(bytes, std::sync::atomic::Ordering::Relaxed);
        let mut g = self.inner.lock();
        let mut out = Vec::new();
        if let Some(c) = g.as_mut() {
            while c.use_.bytes > bytes && c.use_.entries > 0 {
                out.push(c.evict());
            }
        }
        out
    }

    pub fn footprint(&self) -> (&'static str, Footprint) {
        let g = self.inner.lock();
        let mut f = g.as_ref().map(|c| c.use_).unwrap_or_default();
        f.capacity = self.capacity();
        (self.name, f)
    }
}

impl<K: Hash + Eq, V> Clock<K, V> {
    /// Remove the first entry the hand finds not used since it last passed (clearing the
    /// marks it passes).
    fn evict(&mut self) -> V {
        loop {
            if self.hand >= self.slots.len() {
                self.hand = 0;
            }
            let i = self.hand;
            self.hand += 1;
            let Some(s) = self.slots[i].as_mut() else {
                continue;
            };
            if s.used {
                s.used = false;
                continue;
            }
            let s = self.slots[i].take().expect("a slot");
            self.map.remove(&s.key);
            self.free.push(i);
            self.use_.entries -= 1;
            self.use_.bytes -= s.bytes;
            self.use_.evictions += 1;
            return s.value;
        }
    }
}

/// The parse tree and PL/pgSQL compile caches' bound together, unless set.
pub const PLAN_CACHE_BYTES: usize = 36 << 20;

/// Bound the caches of parse trees and of PL/pgSQL compiles at `bytes` together (8 : 1),
/// for the whole process (an operator setting: `roll-engine --plan-cache-mb`). Lowering it
/// evicts at once.
pub fn set_plan_cache_bytes(bytes: usize) {
    crate::sql::parse::set_parse_cache_bytes(bytes / 9 * 8);
    crate::plpgsql::set_compiled_bytes(bytes / 9);
}

/// Every shared cache's footprint, by name.
pub fn footprints() -> Vec<(&'static str, Footprint)> {
    vec![
        crate::sql::parse::parsed_footprint(),
        crate::sql::literals::templates_footprint(),
        crate::plpgsql::compiled_footprint(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_by_bytes_and_keeps_used_entries() {
        let c: Shared<String, Arc<u32>> = Shared::new("test", 100);
        for i in 0..10u32 {
            assert!(c.insert(format!("k{i}"), Arc::new(i), 10).is_empty());
        }
        assert_eq!(c.footprint().1.bytes, 100);
        // k0 is used: the next insert evicts k1, the first entry not used since.
        assert_eq!(c.get(&"k0".to_string()).as_deref(), Some(&0));
        let out = c.insert("k10".to_string(), Arc::new(10), 10);
        assert_eq!(out.iter().map(|v| **v).collect::<Vec<_>>(), vec![1]);
        assert!(c.get(&"k1".to_string()).is_none());
        assert_eq!(c.get(&"k0".to_string()).as_deref(), Some(&0));
        let f = c.footprint().1;
        assert_eq!((f.entries, f.bytes, f.evictions), (10, 100, 1));
        // Too big for the cache, or cached already: handed back.
        assert_eq!(c.insert("big".to_string(), Arc::new(99), 50).len(), 1);
        assert_eq!(c.insert("k0".to_string(), Arc::new(7), 10).len(), 1);
        assert_eq!(c.get(&"k0".to_string()).as_deref(), Some(&0));
        assert_eq!(c.clear().len(), 10);
        assert_eq!(c.footprint().1.entries, 0);
    }
}
