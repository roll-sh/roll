//! Iteration orders of Java hash collections, for messages Cassandra builds from a `HashSet` or
//! `HashMap` (`CreateViewStatement`'s column lists, a custom index's options). Keys go to buckets
//! by their spread hash (`HashMap.hash`), in insertion order within a bucket; resizes keep that
//! relative order, so the order only depends on the final capacity.

/// `ColumnIdentifier.hashCode` (the hash of the name's UTF-8 `ByteBuffer`).
pub fn identifier_hash(name: &str) -> u32 {
    name.bytes().rev().fold(1u32, |h, b| {
        h.wrapping_mul(31).wrapping_add(b as i8 as i32 as u32)
    })
}

/// `String.hashCode`.
pub fn string_hash(s: &str) -> u32 {
    s.encode_utf16()
        .fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32))
}

/// A `java.util.HashMap` / `HashSet` of string keys.
pub struct HashOrder {
    buckets: Vec<Vec<String>>,
    len: usize,
    hash: fn(&str) -> u32,
}

impl HashOrder {
    /// `new HashMap<>()` (16 buckets).
    pub fn new(hash: fn(&str) -> u32) -> Self {
        Self::with_capacity(16, hash)
    }

    /// `new HashMap<>(capacity)`.
    pub fn with_capacity(capacity: usize, hash: fn(&str) -> u32) -> Self {
        HashOrder {
            buckets: vec![Vec::new(); capacity.max(1).next_power_of_two()],
            len: 0,
            hash,
        }
    }

    /// `new HashMap<>(m)` / `new HashSet<>(c)`: sized for the copy, then filled in `m`'s order.
    pub fn copy(other: &HashOrder, min: usize) -> Self {
        let wanted = ((other.len as f32 / 0.75) as usize + 1).max(min);
        let mut out = Self::with_capacity(wanted, other.hash);
        for k in other.keys() {
            out.insert(k);
        }
        out
    }

    fn index(&self, k: &str) -> usize {
        let h = (self.hash)(k);
        ((h ^ (h >> 16)) as usize) & (self.buckets.len() - 1)
    }

    /// Adds `k`; false if it was there.
    pub fn insert(&mut self, k: &str) -> bool {
        let i = self.index(k);
        if self.buckets[i].iter().any(|x| x == k) {
            return false;
        }
        self.buckets[i].push(k.to_string());
        self.len += 1;
        if self.len * 4 > self.buckets.len() * 3 {
            let old = std::mem::take(&mut self.buckets);
            self.buckets = vec![Vec::new(); old.len() * 2];
            for k in old.into_iter().flatten() {
                let i = self.index(&k);
                self.buckets[i].push(k);
            }
        }
        true
    }

    pub fn remove(&mut self, k: &str) {
        let i = self.index(k);
        let before = self.buckets[i].len();
        self.buckets[i].retain(|x| x != k);
        self.len -= before - self.buckets[i].len();
    }

    /// The keys in iteration order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.buckets.iter().flatten().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_sets() {
        // Orders the reference printed (`CreateViewStatement`).
        let order = |names: &[&str]| {
            let mut s = HashOrder::new(identifier_hash);
            for n in names {
                s.insert(n);
            }
            s.keys().map(str::to_string).collect::<Vec<_>>()
        };
        assert_eq!(
            order(&["v", "x", "w", "pk", "ck"]),
            ["v", "w", "pk", "x", "ck"]
        );
        assert_eq!(order(&["d", "a", "b", "c"]), ["a", "b", "c", "d"]);
        assert_eq!(order(&["v", "pk", "ck"]), ["v", "pk", "ck"]);
    }

    #[test]
    fn option_maps() {
        let mut m = HashOrder::new(string_hash);
        for k in ["mode", "max_compaction_flush_memory_in_mb", "class_name"] {
            m.insert(k);
        }
        let mut c = HashOrder::copy(&m, 0);
        c.insert("target");
        assert_eq!(
            c.keys().collect::<Vec<_>>(),
            [
                "mode",
                "max_compaction_flush_memory_in_mb",
                "class_name",
                "target"
            ]
        );
    }
}
