//! One logical database (`SELECT n`): the keyspace plus the index of keys with a TTL.

use crate::dict::Dict;
use crate::value::{Entry, KEY_OVERHEAD, Str, key_mem};

#[derive(Default)]
pub struct Db {
    pub keys: Dict<Str, Entry>,
    /// Keys that have a TTL, for active expiry and `volatile-*` eviction.
    pub volatile: Dict<Str, ()>,
    /// Approximate bytes used by this database (keys, values and overheads).
    pub mem: usize,
    /// Cursor of the active-expiry scan over `volatile`.
    pub expire_cursor: u64,
    /// Running average TTL of sampled keys (INFO keyspace `avg_ttl`).
    pub avg_ttl: i64,
}

impl Db {
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn expires(&self) -> usize {
        self.volatile.len()
    }

    /// Bytes a key costs on top of its value.
    #[inline]
    pub fn key_cost(key: &Str) -> usize {
        KEY_OVERHEAD + key_mem(key)
    }

    /// Insert a brand-new key (caller checked it is absent).
    pub fn insert_new(&mut self, key: &[u8], entry: Entry) -> &mut Entry {
        let k = Str::from_slice(key);
        self.mem += Self::key_cost(&k) + entry.value.mem();
        if entry.expire != 0 {
            self.volatile.insert(k.clone(), ());
            self.mem += Self::key_cost(&k);
        }
        self.keys.insert_new(k, entry)
    }

    /// Remove a key, returning its entry.
    pub fn remove(&mut self, key: &[u8]) -> Option<Entry> {
        let (k, e) = self.keys.remove_entry(key)?;
        self.mem = self.mem.saturating_sub(Self::key_cost(&k) + e.value.mem());
        if e.expire != 0 {
            self.volatile.remove(key);
            self.mem = self.mem.saturating_sub(Self::key_cost(&k));
        }
        Some(e)
    }

    /// Set or clear (0) a key's expiry, keeping the volatile index in sync. Returns false when the
    /// key doesn't exist.
    pub fn set_expire(&mut self, key: &[u8], at: i64) -> bool {
        let Some(e) = self.keys.get_mut(key) else { return false };
        let had = e.expire != 0;
        e.expire = at;
        match (had, at != 0) {
            (false, true) => {
                let k = Str::from_slice(key);
                self.mem += Self::key_cost(&k);
                self.volatile.insert(k, ());
            }
            (true, false) => {
                self.volatile.remove(key);
                self.mem = self.mem.saturating_sub(KEY_OVERHEAD + key.len().saturating_sub(22));
            }
            _ => {}
        }
        true
    }

    pub fn clear(&mut self) {
        self.keys.clear();
        self.volatile.clear();
        self.mem = 0;
        self.expire_cursor = 0;
        self.avg_ttl = 0;
    }

    /// Total bytes including the hash tables themselves.
    pub fn used_bytes(&self) -> usize {
        self.mem + self.keys.table_bytes() + self.volatile.table_bytes()
    }
}
