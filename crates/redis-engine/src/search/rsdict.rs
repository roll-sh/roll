//! RediSearch's copy of the Redis `dict` (`util/dict/dict.c`): chained hash tables of power-of-two
//! sizes, grown at a 1:1 load by incremental rehashing, hashed with SipHash-1-2 under the module's
//! seed, which nothing ever sets (all zeros). The index registry, aliases, synonym groups and
//! spelling dictionaries live in such dicts, so their iteration order (FT._LIST, FT.SYNDUMP, the
//! RDB aux data) follows the buckets.

use bytes::Bytes;

const INITIAL_SIZE: usize = 4;

/// SipHash-1-2 (`siphash.c.inc`) with the zero key.
pub fn siphash(input: &[u8]) -> u64 {
    let (mut v0, mut v1, mut v2, mut v3) = (0x736f6d6570736575u64, 0x646f72616e646f6du64, 0x6c7967656e657261u64, 0x7465646279746573u64);
    macro_rules! round {
        () => {
            v0 = v0.wrapping_add(v1);
            v1 = v1.rotate_left(13);
            v1 ^= v0;
            v0 = v0.rotate_left(32);
            v2 = v2.wrapping_add(v3);
            v3 = v3.rotate_left(16);
            v3 ^= v2;
            v0 = v0.wrapping_add(v3);
            v3 = v3.rotate_left(21);
            v3 ^= v0;
            v2 = v2.wrapping_add(v1);
            v1 = v1.rotate_left(17);
            v1 ^= v2;
            v2 = v2.rotate_left(32);
        };
    }
    let (chunks, rest) = input.as_chunks::<8>();
    for c in chunks {
        let m = u64::from_le_bytes(*c);
        v3 ^= m;
        round!();
        v0 ^= m;
    }
    let mut b = (input.len() as u64) << 56;
    for (i, &x) in rest.iter().enumerate() {
        b |= (x as u64) << (8 * i);
    }
    v3 ^= b;
    round!();
    v0 ^= b;
    v2 ^= 0xff;
    round!();
    round!();
    v0 ^ v1 ^ v2 ^ v3
}

#[derive(Clone, Debug)]
struct Table<V> {
    /// Each bucket's chain, head first.
    buckets: Vec<Vec<(Bytes, V)>>,
    used: usize,
}

impl<V> Table<V> {
    fn empty() -> Self {
        Table { buckets: Vec::new(), used: 0 }
    }

    fn with_size(n: usize) -> Self {
        Table { buckets: (0..n).map(|_| Vec::new()).collect(), used: 0 }
    }

    fn mask(&self) -> usize {
        self.buckets.len().wrapping_sub(1)
    }
}

/// A dict keyed by byte strings.
#[derive(Clone, Debug)]
pub struct RsDict<V> {
    ht: [Table<V>; 2],
    /// `rehashidx`: the next bucket of `ht[0]` to move (`None` = not rehashing).
    rehash: Option<usize>,
}

impl<V> Default for RsDict<V> {
    fn default() -> Self {
        RsDict { ht: [Table::empty(), Table::empty()], rehash: None }
    }
}

impl<V> RsDict<V> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.ht[0].used + self.ht[1].used
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `dictRehash(d, n)`.
    fn rehash(&mut self, mut n: usize) {
        let Some(mut idx) = self.rehash else { return };
        let mut empty_visits = n * 10;
        while n > 0 && self.ht[0].used != 0 {
            n -= 1;
            while self.ht[0].buckets[idx].is_empty() {
                idx += 1;
                empty_visits -= 1;
                if empty_visits == 0 {
                    self.rehash = Some(idx);
                    return;
                }
            }
            let chain = std::mem::take(&mut self.ht[0].buckets[idx]);
            for (k, v) in chain {
                let h = siphash(&k) as usize & self.ht[1].mask();
                self.ht[1].buckets[h].insert(0, (k, v));
                self.ht[0].used -= 1;
                self.ht[1].used += 1;
            }
            idx += 1;
        }
        if self.ht[0].used == 0 {
            self.ht[0] = std::mem::replace(&mut self.ht[1], Table::empty());
            self.rehash = None;
        } else {
            self.rehash = Some(idx);
        }
    }

    /// `_dictRehashStep`: the step every add, find and delete takes while rehashing.
    fn step(&mut self) {
        if self.rehash.is_some() {
            self.rehash(1);
        }
    }

    /// `dictExpand`.
    pub fn expand(&mut self, size: usize) {
        if self.rehash.is_some() || self.ht[0].used > size {
            return;
        }
        let real = size.max(INITIAL_SIZE).next_power_of_two();
        if real == self.ht[0].buckets.len() {
            return;
        }
        if self.ht[0].buckets.is_empty() {
            self.ht[0] = Table::with_size(real);
            return;
        }
        self.ht[1] = Table::with_size(real);
        self.rehash = Some(0);
    }

    fn expand_if_needed(&mut self) {
        if self.rehash.is_some() {
            return;
        }
        if self.ht[0].buckets.is_empty() {
            self.expand(INITIAL_SIZE);
        } else if self.ht[0].used >= self.ht[0].buckets.len() {
            self.expand(self.ht[0].used * 2);
        }
    }

    fn position(&self, key: &[u8], h: u64) -> Option<(usize, usize, usize)> {
        for t in 0..2 {
            if self.ht[t].buckets.is_empty() {
                break;
            }
            let b = h as usize & self.ht[t].mask();
            if let Some(i) = self.ht[t].buckets[b].iter().position(|(k, _)| k.as_ref() == key) {
                return Some((t, b, i));
            }
            if self.rehash.is_none() {
                break;
            }
        }
        None
    }

    /// `dictAdd`: false (and nothing changes but a rehash step) when the key exists.
    pub fn add(&mut self, key: Bytes, val: V) -> bool {
        self.step();
        self.expand_if_needed();
        let h = siphash(&key);
        if self.position(&key, h).is_some() {
            return false;
        }
        let t = if self.rehash.is_some() { 1 } else { 0 };
        let b = h as usize & self.ht[t].mask();
        self.ht[t].buckets[b].insert(0, (key, val));
        self.ht[t].used += 1;
        true
    }

    /// `dictFind` (a rehash step included).
    pub fn find(&mut self, key: &[u8]) -> Option<&V> {
        self.find_mut(key).map(|v| &*v)
    }

    pub fn find_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        if self.is_empty() {
            return None;
        }
        self.step();
        let (t, b, i) = self.position(key, siphash(key))?;
        Some(&mut self.ht[t].buckets[b][i].1)
    }

    /// A lookup the reference doesn't make (no rehash step).
    pub fn peek(&self, key: &[u8]) -> Option<&V> {
        let (t, b, i) = self.position(key, siphash(key))?;
        Some(&self.ht[t].buckets[b][i].1)
    }

    pub fn peek_key_value(&self, key: &[u8]) -> Option<(&Bytes, &V)> {
        let (t, b, i) = self.position(key, siphash(key))?;
        let (k, v) = &self.ht[t].buckets[b][i];
        Some((k, v))
    }

    pub fn peek_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        let (t, b, i) = self.position(key, siphash(key))?;
        Some(&mut self.ht[t].buckets[b][i].1)
    }

    /// Map-style aliases of the effect-free lookups.
    pub fn get(&self, key: &[u8]) -> Option<&V> {
        self.peek(key)
    }

    pub fn get_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        self.peek_mut(key)
    }

    pub fn get_key_value(&self, key: &[u8]) -> Option<(&Bytes, &V)> {
        self.peek_key_value(key)
    }

    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.peek(key).is_some()
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }

    /// `dictDelete`.
    pub fn delete(&mut self, key: &[u8]) -> Option<V> {
        if self.is_empty() {
            return None;
        }
        self.step();
        let (t, b, i) = self.position(key, siphash(key))?;
        self.ht[t].used -= 1;
        Some(self.ht[t].buckets[b].remove(i).1)
    }

    /// `dictEmpty`.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// `dictNext` order: `ht[0]`'s buckets, then `ht[1]`'s while rehashing; chains head first.
    pub fn iter(&self) -> impl Iterator<Item = (&Bytes, &V)> {
        self.ht.iter().flat_map(|t| t.buckets.iter()).flat_map(|c| c.iter().map(|(k, v)| (k, v)))
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.ht.iter_mut().flat_map(|t| t.buckets.iter_mut()).flat_map(|c| c.iter_mut().map(|(_, v)| v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &Bytes> {
        self.iter().map(|(k, _)| k)
    }

    /// The entries in `dictNext` order.
    pub fn into_entries(self) -> Vec<(Bytes, V)> {
        self.ht.into_iter().flat_map(|t| t.buckets).flatten().collect()
    }
}

impl<V> std::ops::Index<&Bytes> for RsDict<V> {
    type Output = V;

    fn index(&self, key: &Bytes) -> &V {
        self.peek(key).expect("key in dict")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siphash_vectors() {
        // The module's SipHash-1-2 with the zero key.
        assert_eq!(siphash(b""), 3604268065564183773);
        assert_eq!(siphash(b"a"), 12523601488019100690);
        assert_eq!(siphash(b"idx"), 6513584534150223813);
        assert_eq!(siphash(b"hello world"), 5632111399726606718);
        assert_eq!(siphash(b"0123456789abcdef!"), 12058675581253119361);
    }

    #[test]
    fn bucket_order_and_incremental_rehash() {
        let names = |d: &RsDict<()>| d.keys().map(|k| String::from_utf8_lossy(k).into_owned()).collect::<Vec<_>>();
        let mut d = RsDict::new();
        for i in 1..=4 {
            d.add(Bytes::from(format!("ord{i}")), ());
        }
        // Four buckets: ord2 | - | ord3 | ord4, ord1 (head insertion), as FT._LIST shows on a fresh server.
        assert_eq!(names(&d), ["ord2", "ord3", "ord4", "ord1"]);
        // The fifth key starts a rehash into eight buckets and lands in the new table.
        d.add(Bytes::from_static(b"ord5"), ());
        assert_eq!(names(&d), ["ord2", "ord3", "ord4", "ord1", "ord5"]);
        // Each lookup moves one bucket (skipping empty ones); bucket 3's chain is reversed.
        d.find(b"ord1");
        assert_eq!(names(&d), ["ord3", "ord4", "ord1", "ord5", "ord2"]);
        d.find(b"ord1");
        d.find(b"ord1");
        assert_eq!(names(&d), ["ord5", "ord2", "ord3", "ord1", "ord4"]);
    }

    #[test]
    fn grows_and_keeps_everything() {
        let mut d = RsDict::new();
        for i in 0..100u32 {
            assert!(d.add(Bytes::from(format!("k{i}")), i));
            assert!(!d.add(Bytes::from(format!("k{i}")), i));
        }
        assert_eq!(d.len(), 100);
        for i in 0..100u32 {
            assert_eq!(d.find(format!("k{i}").as_bytes()), Some(&i));
        }
        assert_eq!(d.iter().count(), 100);
        for i in (0..100u32).step_by(2) {
            assert_eq!(d.delete(format!("k{i}").as_bytes()), Some(i));
        }
        assert_eq!(d.len(), 50);
        assert_eq!(d.iter().count(), 50);
        assert!(d.peek(b"k0").is_none());
        assert_eq!(d.peek(b"k1"), Some(&1));
    }
}
