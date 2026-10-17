//! One logical database (`SELECT n`): the keyspace plus the index of keys with a TTL.

use crate::dict::Dict;
use crate::value::{Entry, KEY_OVERHEAD, NO_SIZE, SIZE_BINS, SIZE_TYPES, Str, Value, key_mem};

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
    /// Hashes with field TTLs (Redis' `subexpires`): key → the next field expiry the hash is
    /// registered with ([`crate::types::Hash::subexpiry`]), for active field expiry and INFO
    /// keyspace `subexpiry`. Kept in sync on insert/remove and by [`Db::sync_subexpiry`].
    pub subexpires: Dict<Str, i64>,
    /// Cursor of the active field-expiry scan over `subexpires`.
    pub subexpire_cursor: u64,
    /// INFO keysizes: keys per [`Value::size_slot`].
    pub keysizes: Vec<u64>,
}

impl Default for Db {
    fn default() -> Self {
        Self {
            keys: Default::default(),
            volatile: Default::default(),
            mem: 0,
            expire_cursor: 0,
            avg_ttl: 0,
            subexpires: Default::default(),
            subexpire_cursor: 0,
            keysizes: vec![0; SIZE_TYPES * SIZE_BINS],
        }
    }
}

/// The subexpiry registration of a value (0 = none).
#[inline]
fn subexpiry_of(v: &Value) -> i64 {
    match v {
        Value::Hash(h) => h.subexpiry(),
        _ => 0,
    }
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
    pub fn insert_new(&mut self, key: &[u8], mut entry: Entry) -> &mut Entry {
        let k = Str::from_slice(key);
        entry.sized = entry.value.size_slot();
        if entry.sized != NO_SIZE {
            self.keysizes[entry.sized as usize] += 1;
        }
        self.mem += Self::key_cost(&k) + entry.value.mem();
        if entry.expire != 0 {
            self.volatile.insert(k.clone(), ());
            self.mem += Self::key_cost(&k);
        }
        let sub = subexpiry_of(&entry.value);
        if sub != 0 {
            self.subexpires.insert(k.clone(), sub);
            self.mem += Self::key_cost(&k);
        }
        self.keys.insert_new(k, entry)
    }

    /// Make the template hash at `key` a plain hash (`LoadItem::Untemplate`).
    pub fn untemplate(&mut self, key: &[u8], lim: crate::types::hash::Limits) {
        if let Some(Entry { value: crate::value::Value::Hash(h), .. }) = self.keys.get_mut(key) {
            let old = h.mem();
            h.untemplate(false, lim);
            self.mem = (self.mem + h.mem()).saturating_sub(old);
        }
    }

    /// Remove a key, returning its entry.
    pub fn remove(&mut self, key: &[u8]) -> Option<Entry> {
        let (k, e) = self.keys.remove_entry(key)?;
        self.mem = self.mem.saturating_sub(Self::key_cost(&k) + e.value.mem());
        if e.sized != NO_SIZE {
            self.keysizes[e.sized as usize] -= 1;
        }
        if e.expire != 0 {
            self.volatile.remove(key);
            self.mem = self.mem.saturating_sub(Self::key_cost(&k));
        }
        if matches!(e.value, Value::Hash(_)) && !self.subexpires.is_empty() && self.subexpires.remove(key).is_some() {
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

    /// Bring the `subexpires` index in line with the registration of the value at `key` (after a
    /// hash changed its registration, or the key's value was replaced).
    pub fn sync_subexpiry(&mut self, key: &[u8]) {
        let sub = self.keys.get(key).map_or(0, |e| subexpiry_of(&e.value));
        if sub == 0 {
            if !self.subexpires.is_empty() && let Some((k, _)) = self.subexpires.remove_entry(key) {
                self.mem = self.mem.saturating_sub(Self::key_cost(&k));
            }
        } else if let Some(at) = self.subexpires.get_mut(key) {
            *at = sub;
        } else {
            let k = Str::from_slice(key);
            self.mem += Self::key_cost(&k);
            self.subexpires.insert_new(k, sub);
        }
    }

    /// Re-count a key whose value may have changed size (after a command touched it).
    pub fn resize(&mut self, key: &[u8]) {
        let Some(e) = self.keys.get_mut(key) else { return };
        let slot = e.value.size_slot();
        if slot != e.sized {
            if e.sized != NO_SIZE {
                self.keysizes[e.sized as usize] -= 1;
            }
            if slot != NO_SIZE {
                self.keysizes[slot as usize] += 1;
            }
            e.sized = slot;
        }
    }

    /// Recount the keysizes histogram from scratch (after the keyspace was replaced wholesale).
    pub fn recount_sizes(&mut self) {
        self.keysizes.iter_mut().for_each(|n| *n = 0);
        let mut counts = std::mem::take(&mut self.keysizes);
        self.keys.for_each_mut(|_, e| {
            e.sized = e.value.size_slot();
            if e.sized != NO_SIZE {
                counts[e.sized as usize] += 1;
            }
        });
        self.keysizes = counts;
    }

    pub fn clear(&mut self) {
        self.keysizes.iter_mut().for_each(|n| *n = 0);
        self.keys.clear();
        self.volatile.clear();
        self.subexpires.clear();
        self.subexpire_cursor = 0;
        self.mem = 0;
        self.expire_cursor = 0;
        self.avg_ttl = 0;
    }

    /// Total bytes including the hash tables themselves.
    pub fn used_bytes(&self) -> usize {
        self.mem + self.keys.table_bytes() + self.volatile.table_bytes() + self.subexpires.table_bytes()
    }
}
