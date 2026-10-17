//! The `hash` type: a field → value map where each field may carry its own expiry (Redis 7.4+
//! hash field expiration, "HFE").
//!
//! Encodings, named as `OBJECT ENCODING` reports them:
//!
//! - `listpack`: a [`Pack`] of field/value pairs in insertion order.
//! - `listpackex`: a listpack hash that has used field TTLs. Like Redis, pairs are kept ordered
//!   by expiry time: fields with a TTL first (ascending), then fields without one. Only the TTL
//!   prefix pays for an expiry (8 bytes each, in a side vector).
//! - `hashtable`: a [`Dict`] (Redis-exact HSCAN cursors) plus, once any field got a TTL, an index
//!   of the expiring fields (by field, and ordered by time for expiry).
//!
//! A hash starts as `listpack`, becomes `listpackex` the first time a field-expiration command
//! touches it, and becomes `hashtable` once it outgrows `hash-max-listpack-entries` /
//! `hash-max-listpack-value`. It never converts back.
//!
//! Expiry times are absolute Unix milliseconds; 0 means "no TTL". The type itself never looks at
//! the clock: readers that care pass `now` ([`Hash::get_live`], [`Hash::iter_live`]) and the
//! command layer decides when expired fields are deleted (lazily, or by active expiry through
//! [`Hash::expire_due`]).

use crate::config::Config;
use crate::dict::Dict;
use crate::pack::Pack;
use crate::util::Rng;
use crate::value::Str;
use std::collections::BTreeSet;

/// Listpack size limits (`hash-max-listpack-entries`, `hash-max-listpack-value`).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub entries: usize,
    pub value: usize,
}

impl Limits {
    pub fn of(cfg: &Config) -> Self {
        Self { entries: cfg.hash_max_listpack_entries, value: cfg.hash_max_listpack_value }
    }
}

#[derive(Clone, Debug)]
pub struct Hash {
    enc: Enc,
    /// The next field expiry this hash is registered with in its database's `subexpires` index
    /// (0 = not registered). Like Redis' per-hash `ExpireMeta` it is only refreshed at specific
    /// points, so it can be stale; active expiry fixes it up.
    reg: i64,
}

#[derive(Clone, Debug)]
enum Enc {
    Pack(Pack),
    PackEx(PackEx),
    Table(Box<Table>),
}

/// `listpackex`: pair `i` expires at `ttl[i]` for `i < ttl.len()`; later pairs have no TTL.
#[derive(Clone, Debug, Default)]
struct PackEx {
    pack: Pack,
    ttl: Vec<i64>,
}

#[derive(Clone, Debug)]
struct Table {
    map: Dict<Str, Str>,
    /// Expiring fields, allocated the first time a field gets a TTL.
    ttl: Option<Box<Ttls>>,
    /// Heap bytes of nodes, fields, values and TTL index entries (tables excluded).
    heap: usize,
}

#[derive(Clone, Debug, Default)]
struct Ttls {
    at: Dict<Str, i64>,
    order: BTreeSet<(i64, Str)>,
}

#[inline]
fn spill(s: &Str) -> usize {
    if s.spilled() { s.capacity() } else { 0 }
}

#[inline]
fn spill_len(n: usize) -> usize {
    if n > 22 { n } else { 0 }
}

/// Bytes of one field/value node of the hashtable encoding.
#[inline]
fn node_cost(f: &Str, v: &Str) -> usize {
    Dict::<Str, Str>::NODE_OVERHEAD + spill(f) + spill(v)
}

/// Bytes of one entry of the TTL index (dict node plus B-tree slot, field stored twice).
#[inline]
fn ttl_cost(f: &Str) -> usize {
    Dict::<Str, i64>::NODE_OVERHEAD + std::mem::size_of::<(i64, Str)>() + 16 + 2 * spill(f)
}

impl Ttls {
    fn get(&self, f: &[u8]) -> i64 {
        self.at.get(f).copied().unwrap_or(0)
    }
}

impl Table {
    fn with_capacity(n: usize) -> Self {
        Table { map: Dict::with_capacity(n), ttl: None, heap: 0 }
    }

    fn ttl_of(&self, f: &[u8]) -> i64 {
        self.ttl.as_ref().map_or(0, |t| t.get(f))
    }

    fn insert_new(&mut self, f: Str, v: Str) {
        self.heap += node_cost(&f, &v);
        self.map.insert_new(f, v);
    }

    /// Set or clear (0) the TTL of field `f`.
    fn set_ttl(&mut self, f: &[u8], at: i64) {
        let old = self.ttl_of(f);
        if old == at {
            return;
        }
        let t = self.ttl.get_or_insert_with(Box::default);
        let key = Str::from_slice(f);
        if old != 0 {
            t.order.remove(&(old, key.clone()));
        }
        if at == 0 {
            t.at.remove(f);
            self.heap = self.heap.saturating_sub(ttl_cost(&key));
        } else {
            t.order.insert((at, key.clone()));
            if old == 0 {
                self.heap += ttl_cost(&key);
            }
            t.at.insert(key, at);
        }
    }

    fn remove(&mut self, f: &[u8]) -> bool {
        let Some((k, v)) = self.map.remove_entry(f) else { return false };
        self.heap = self.heap.saturating_sub(node_cost(&k, &v));
        if self.ttl_of(f) != 0 {
            self.set_ttl(f, 0);
        }
        true
    }

    fn mem(&self) -> usize {
        self.heap
            + self.map.table_bytes()
            + self.ttl.as_ref().map_or(0, |t| std::mem::size_of::<Ttls>() + t.at.table_bytes())
    }
}

impl PackEx {
    fn index(&self, f: &[u8]) -> Option<usize> {
        self.pack.iter_pairs().position(|(k, _)| k == f)
    }

    fn ttl_at(&self, i: usize) -> i64 {
        self.ttl.get(i).copied().unwrap_or(0)
    }

    /// Remove pair `i`, returning its value.
    fn take(&mut self, i: usize) -> Vec<u8> {
        let v = self.pack.get(2 * i + 1).unwrap().to_vec();
        self.pack.pair_remove_at(i);
        if i < self.ttl.len() {
            self.ttl.remove(i);
        }
        v
    }

    /// Insert a pair at its place by expiry: before the first field with a TTL >= `at` (or with
    /// no TTL), exactly like `listpackExAddInternal`. `at == 0` appends.
    fn put(&mut self, f: &[u8], v: &[u8], at: i64) {
        if at == 0 {
            self.pack.push_back(f);
            self.pack.push_back(v);
            return;
        }
        let p = self.ttl.iter().position(|&t| t >= at).unwrap_or(self.ttl.len());
        self.pack.pair_insert(p, f, v);
        self.ttl.insert(p, at);
    }
}

impl Default for Hash {
    fn default() -> Self {
        Self::new()
    }
}

impl Hash {
    pub fn new() -> Self {
        Hash { enc: Enc::Pack(Pack::new()), reg: 0 }
    }

    /// Build from field/value pairs (later duplicates overwrite), choosing the encoding by `lim`.
    pub fn from_pairs<I, F, V>(pairs: I, lim: Limits) -> Self
    where
        I: IntoIterator<Item = (F, V)>,
        F: AsRef<[u8]>,
        V: AsRef<[u8]>,
    {
        let mut h = Hash::new();
        for (f, v) in pairs {
            let (f, v) = (f.as_ref(), v.as_ref());
            h.prepare(1, [f.len(), v.len()], lim);
            h.set(f, v, false, lim);
        }
        h
    }

    /// Number of stored fields, including expired ones not deleted yet (what HLEN reports).
    pub fn len(&self) -> usize {
        match &self.enc {
            Enc::Pack(p) => p.pairs(),
            Enc::PackEx(x) => x.pack.pairs(),
            Enc::Table(t) => t.map.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Heap bytes owned, maintained incrementally so this is O(1).
    pub fn mem(&self) -> usize {
        match &self.enc {
            Enc::Pack(p) => p.mem(),
            Enc::PackEx(x) => x.pack.mem() + x.ttl.capacity() * 8,
            Enc::Table(t) => std::mem::size_of::<Table>() + t.mem(),
        }
    }

    pub fn encoding(&self) -> &'static str {
        match &self.enc {
            Enc::Pack(_) => "listpack",
            Enc::PackEx(_) => "listpackex",
            Enc::Table(_) => "hashtable",
        }
    }

    pub fn is_listpack(&self) -> bool {
        !matches!(self.enc, Enc::Table(_))
    }

    // ---- reads ----

    /// Value and expiry (0 = none) of `field`, whether or not it has expired.
    pub fn get_entry(&self, field: &[u8]) -> Option<(&[u8], i64)> {
        match &self.enc {
            Enc::Pack(p) => p.pair_get(field).map(|v| (v, 0)),
            Enc::PackEx(x) => {
                let mut it = x.pack.iter();
                let mut i = 0;
                while let (Some(f), Some(v)) = (it.next(), it.next()) {
                    if f == field {
                        return Some((v, x.ttl_at(i)));
                    }
                    i += 1;
                }
                None
            }
            Enc::Table(t) => t.map.get(field).map(|v| (v.as_slice(), t.ttl_of(field))),
        }
    }

    /// Value of `field`, ignoring expiry.
    pub fn get(&self, field: &[u8]) -> Option<&[u8]> {
        self.get_entry(field).map(|(v, _)| v)
    }

    /// Value of `field` unless it expired before `now`.
    pub fn get_live(&self, field: &[u8], now: i64) -> Option<&[u8]> {
        self.get_entry(field).filter(|&(_, at)| !expired(at, now)).map(|(v, _)| v)
    }

    pub fn contains(&self, field: &[u8]) -> bool {
        self.get_entry(field).is_some()
    }

    /// Expiry of `field`: `None` if missing, `Some(0)` if it has no TTL.
    pub fn expire_of(&self, field: &[u8]) -> Option<i64> {
        self.get_entry(field).map(|(_, at)| at)
    }

    /// The earliest field expiry (0 if no field has a TTL). O(1).
    pub fn min_expire(&self) -> i64 {
        match &self.enc {
            Enc::Pack(_) => 0,
            Enc::PackEx(x) => x.ttl.first().copied().unwrap_or(0),
            Enc::Table(t) => t.ttl.as_ref().and_then(|t| t.order.first()).map_or(0, |(at, _)| *at),
        }
    }

    /// Does any field have a TTL?
    pub fn has_expiring(&self) -> bool {
        self.min_expire() != 0
    }

    /// Every stored field as `(field, value, expire)` (expire 0 = no TTL), including expired ones.
    /// Order: insertion order for `listpack`, expiry order for `listpackex`, table order for
    /// `hashtable` (what HGETALL returns).
    pub fn iter(&self) -> Iter<'_> {
        match &self.enc {
            Enc::Pack(p) => Iter(Inner::Pack(p.iter())),
            Enc::PackEx(x) => Iter(Inner::PackEx(x.pack.iter(), &x.ttl, 0)),
            Enc::Table(t) => Iter(Inner::Table(t.map.iter(), t.ttl.as_deref())),
        }
    }

    /// Fields that have not expired at `now`, as `(field, value)`.
    pub fn iter_live(&self, now: i64) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.iter().filter(move |&(_, _, at)| !expired(at, now)).map(|(f, v, _)| (f, v))
    }

    /// Number of fields not expired at `now`.
    pub fn live_len(&self, now: i64) -> usize {
        if self.min_expire() == 0 || self.min_expire() >= now {
            return self.len();
        }
        match &self.enc {
            Enc::Pack(p) => p.pairs(),
            Enc::PackEx(x) => x.pack.pairs() - x.ttl.iter().take_while(|&&t| t < now).count(),
            Enc::Table(t) => {
                let gone = t.ttl.as_ref().map_or(0, |t| t.order.iter().take_while(|(at, _)| *at < now).count());
                t.map.len() - gone
            }
        }
    }

    /// The pair at position `i` of a listpack encoding (`None` for a hashtable or out of range).
    pub fn pack_pair(&self, i: usize) -> Option<(&[u8], &[u8])> {
        let p = match &self.enc {
            Enc::Pack(p) => p,
            Enc::PackEx(x) => &x.pack,
            Enc::Table(_) => return None,
        };
        Some((p.get(2 * i)?, p.get(2 * i + 1)?))
    }

    /// A random field (uniform for listpacks, bucket-uniform for hashtables). Expired fields are
    /// not skipped: callers expire first.
    pub fn random(&self, rng: &mut Rng) -> Option<(&[u8], &[u8])> {
        if self.is_empty() {
            return None;
        }
        match &self.enc {
            Enc::Table(t) => t.map.random(rng).map(|(f, v)| (f.as_slice(), v.as_slice())),
            _ => self.pack_pair(rng.below(self.len())),
        }
    }

    /// One HSCAN step over a hashtable: calls `f(field, value, expire)` for the bucket(s) at
    /// `cursor` and returns the next cursor. `None` for listpack encodings (scanned in one go).
    pub fn scan_table(&self, cursor: u64, mut f: impl FnMut(&[u8], &[u8], i64)) -> Option<u64> {
        match &self.enc {
            Enc::Table(t) => {
                let ttl = t.ttl.as_deref();
                Some(t.map.scan(cursor, |k, v| f(k, v, ttl.map_or(0, |x| x.get(k)))))
            }
            _ => None,
        }
    }

    // ---- registration in the database's subexpires index ----

    /// The next field expiry this hash is registered with (0 = not registered).
    pub fn subexpiry(&self) -> i64 {
        self.reg
    }

    pub fn set_subexpiry(&mut self, at: i64) {
        self.reg = at;
    }

    /// Is active expiry due (registered and the registered time has passed)?
    #[inline]
    pub fn subexpiry_due(&self, now: i64) -> bool {
        self.reg != 0 && self.reg <= now
    }

    // ---- writes ----

    /// Convert to `hashtable` ahead of a write of `pairs` field/value pairs whose argument lengths
    /// are `lens`, like Redis' `hashTypeTryConversion`.
    pub fn prepare(&mut self, pairs: usize, lens: impl IntoIterator<Item = usize>, lim: Limits) {
        if !self.is_listpack() {
            return;
        }
        if pairs > lim.entries {
            self.convert_to_table(pairs);
            return;
        }
        if lens.into_iter().any(|l| l > lim.value) {
            self.convert_to_table(0);
        }
    }

    /// Start using field TTLs: `listpack` becomes `listpackex` (`hashTypeSetExInit`).
    pub fn enable_ttl(&mut self) {
        if let Enc::Pack(p) = &mut self.enc {
            let pack = std::mem::take(p);
            self.enc = Enc::PackEx(PackEx { pack, ttl: Vec::new() });
        }
    }

    fn convert_to_table(&mut self, hint: usize) {
        let mut t = Table::with_capacity(self.len().max(hint));
        match std::mem::replace(&mut self.enc, Enc::Pack(Pack::new())) {
            Enc::Pack(p) => {
                for (f, v) in p.iter_pairs() {
                    t.insert_new(Str::from_slice(f), Str::from_slice(v));
                }
            }
            Enc::PackEx(x) => {
                for (i, (f, v)) in x.pack.iter_pairs().enumerate() {
                    t.insert_new(Str::from_slice(f), Str::from_slice(v));
                    if let Some(&at) = x.ttl.get(i) {
                        t.set_ttl(f, at);
                    }
                }
            }
            Enc::Table(old) => t = *old,
        }
        self.enc = Enc::Table(Box::new(t));
    }

    /// HSET semantics (`hashTypeSet`): set `field` to `value`, returning true when the field is
    /// new. An existing TTL is removed unless `keep_ttl` (in `listpackex` the field then moves to
    /// the end). Converts to `hashtable` when the entry count exceeds `lim.entries`; value
    /// lengths are checked by [`Hash::prepare`].
    pub fn set(&mut self, field: &[u8], value: &[u8], keep_ttl: bool, lim: Limits) -> bool {
        let new = match &mut self.enc {
            Enc::Pack(p) => p.pair_set(field, value),
            Enc::PackEx(x) => match x.index(field) {
                Some(i) if i < x.ttl.len() && !keep_ttl => {
                    x.take(i);
                    x.put(field, value, 0);
                    false
                }
                Some(i) => {
                    x.pack.set(2 * i + 1, value);
                    false
                }
                None => {
                    x.put(field, value, 0);
                    true
                }
            },
            Enc::Table(t) => {
                match t.map.get_mut(field) {
                    Some(v) => {
                        let old = spill(v);
                        *v = Str::from_slice(value);
                        t.heap = (t.heap + spill_len(value.len())).saturating_sub(old);
                        if !keep_ttl && t.ttl_of(field) != 0 {
                            t.set_ttl(field, 0);
                        }
                        false
                    }
                    None => {
                        t.insert_new(Str::from_slice(field), Str::from_slice(value));
                        true
                    }
                }
            }
        };
        if new && self.is_listpack() && self.len() > lim.entries {
            self.convert_to_table(0);
        }
        new
    }

    /// Delete `field` (expired or not). Returns true if it existed.
    pub fn remove(&mut self, field: &[u8]) -> bool {
        match &mut self.enc {
            Enc::Pack(p) => p.pair_remove(field).is_some(),
            Enc::PackEx(x) => match x.index(field) {
                Some(i) => {
                    x.pack.pair_remove_at(i);
                    if i < x.ttl.len() {
                        x.ttl.remove(i);
                    }
                    true
                }
                None => false,
            },
            Enc::Table(t) => t.remove(field),
        }
    }

    /// Set (or clear, with 0) the expiry of an existing `field`. Turns a `listpack` into
    /// `listpackex`. In `listpackex` the field is re-inserted at its place by expiry time.
    /// Returns false if the field doesn't exist.
    pub fn set_expire(&mut self, field: &[u8], at: i64) -> bool {
        self.enable_ttl();
        match &mut self.enc {
            Enc::Pack(_) => unreachable!("converted above"),
            Enc::PackEx(x) => {
                let Some(i) = x.index(field) else { return false };
                let v = x.take(i);
                x.put(field, &v, at);
                true
            }
            Enc::Table(t) => {
                if !t.map.contains_key(field) {
                    return false;
                }
                t.set_ttl(field, at);
                true
            }
        }
    }

    /// Delete every field whose expiry is `<= now` (active expiry), returning them in expiry order.
    pub fn expire_due(&mut self, now: i64) -> Vec<Str> {
        let mut out = Vec::new();
        match &mut self.enc {
            Enc::Pack(_) => {}
            Enc::PackEx(x) => {
                let n = x.ttl.iter().take_while(|&&t| t <= now).count();
                if n > 0 {
                    out.extend(x.pack.iter().step_by(2).take(n).map(Str::from_slice));
                    x.pack.remove_range(0, 2 * n);
                    x.ttl.drain(..n);
                }
            }
            Enc::Table(t) => {
                while let Some((at, f)) = t.ttl.as_ref().and_then(|x| x.order.first().cloned()) {
                    if at > now {
                        break;
                    }
                    t.remove(&f);
                    out.push(f);
                }
            }
        }
        out
    }

    /// Release excess capacity (after bulk deletions).
    pub fn shrink(&mut self) {
        match &mut self.enc {
            Enc::Pack(p) => p.shrink_to_fit(),
            Enc::PackEx(x) => {
                x.pack.shrink_to_fit();
                x.ttl.shrink_to_fit();
            }
            Enc::Table(_) => {}
        }
    }
}

/// Is a field with expiry `at` expired at `now`? (Redis: visible while `at >= now`.)
#[inline]
pub fn expired(at: i64, now: i64) -> bool {
    at != 0 && at < now
}

/// Iterator over `(field, value, expire)`; see [`Hash::iter`].
pub struct Iter<'a>(Inner<'a>);

enum Inner<'a> {
    Pack(crate::pack::PackIter<'a>),
    PackEx(crate::pack::PackIter<'a>, &'a [i64], usize),
    Table(crate::dict::Iter<'a, Str, Str>, Option<&'a Ttls>),
}

impl<'a> Iterator for Iter<'a> {
    type Item = (&'a [u8], &'a [u8], i64);

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.0 {
            Inner::Pack(it) => Some((it.next()?, it.next()?, 0)),
            Inner::PackEx(it, ttl, i) => {
                let (f, v) = (it.next()?, it.next()?);
                let at = ttl.get(*i).copied().unwrap_or(0);
                *i += 1;
                Some((f, v, at))
            }
            Inner::Table(it, ttl) => {
                let (f, v) = it.next()?;
                Some((f.as_slice(), v.as_slice(), ttl.map_or(0, |t| t.get(f))))
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match &self.0 {
            Inner::Pack(it) | Inner::PackEx(it, _, _) => it.len() / 2,
            Inner::Table(it, _) => it.size_hint().0,
        };
        (n, Some(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIM: Limits = Limits { entries: 4, value: 8 };

    fn fields(h: &Hash) -> Vec<String> {
        h.iter().map(|(f, _, _)| String::from_utf8_lossy(f).into_owned()).collect()
    }

    #[test]
    fn listpack_keeps_insertion_order_and_converts() {
        let mut h = Hash::new();
        assert!(h.set(b"a", b"1", false, LIM));
        assert!(h.set(b"b", b"2", false, LIM));
        assert!(!h.set(b"a", b"11", false, LIM));
        assert_eq!(fields(&h), ["a", "b"]);
        assert_eq!(h.get(b"a"), Some(&b"11"[..]));
        assert_eq!(h.encoding(), "listpack");
        for f in [b"c", b"d", b"e"] {
            h.set(f, b"x", false, LIM);
        }
        assert_eq!(h.encoding(), "hashtable");
        assert_eq!(h.len(), 5);
        assert_eq!(h.get(b"a"), Some(&b"11"[..]));
        let mut long = Hash::new();
        long.prepare(1, [1, 9], LIM);
        assert_eq!(long.encoding(), "hashtable");
    }

    #[test]
    fn listpackex_orders_by_expiry() {
        let mut h = Hash::new();
        for f in [b"a", b"b", b"c", b"d"] {
            h.set(f, b"v", false, Limits { entries: 128, value: 64 });
        }
        h.set_expire(b"c", 100);
        assert_eq!(h.encoding(), "listpackex");
        assert_eq!(fields(&h), ["c", "a", "b", "d"]);
        h.set_expire(b"d", 50);
        h.set_expire(b"a", 200);
        assert_eq!(fields(&h), ["d", "c", "a", "b"]);
        // Equal expiry goes before the existing one.
        h.set_expire(b"b", 100);
        assert_eq!(fields(&h), ["d", "b", "c", "a"]);
        h.set_expire(b"d", 0);
        assert_eq!(fields(&h), ["b", "c", "a", "d"]);
        // Overwriting a volatile field drops the TTL and moves it last, KEEPTTL keeps it.
        h.set(b"b", b"new", false, LIM);
        assert_eq!(fields(&h), ["c", "a", "d", "b"]);
        h.set(b"c", b"kept", true, LIM);
        assert_eq!(h.expire_of(b"c"), Some(100));
        assert_eq!(h.min_expire(), 100);
        assert_eq!(h.live_len(150), 3);
        assert_eq!(h.expire_due(150), vec![Str::from_slice(b"c")]);
        assert_eq!(h.min_expire(), 200);
        assert_eq!(fields(&h), ["a", "d", "b"]);
    }

    #[test]
    fn table_ttls() {
        let mut h = Hash::new();
        h.prepare(10, [1], LIM);
        assert_eq!(h.encoding(), "hashtable");
        for (i, f) in [b"a", b"b", b"c"].iter().enumerate() {
            h.set(*f, b"v", false, LIM);
            h.set_expire(*f, 100 * (3 - i as i64));
        }
        assert_eq!(h.min_expire(), 100);
        assert_eq!(h.get_live(b"c", 101), None);
        assert_eq!(h.get_live(b"c", 100), Some(&b"v"[..]));
        let mem = h.mem();
        assert_eq!(h.expire_due(200), vec![Str::from_slice(b"c"), Str::from_slice(b"b")]);
        assert!(h.mem() < mem);
        assert_eq!(h.len(), 1);
        h.set(b"a", b"w", false, LIM);
        assert_eq!(h.min_expire(), 0);
        assert!(h.remove(b"a"));
        assert!(h.is_empty());
    }

    #[test]
    fn listpackex_converts_with_ttls() {
        let mut h = Hash::new();
        h.set(b"a", b"1", false, LIM);
        h.set_expire(b"a", 500);
        for f in [b"b", b"c", b"d", b"e"] {
            h.set(f, b"x", false, LIM);
        }
        assert_eq!(h.encoding(), "hashtable");
        assert_eq!(h.expire_of(b"a"), Some(500));
        assert_eq!(h.min_expire(), 500);
        let all: Vec<_> = h.iter().filter(|x| x.2 != 0).collect();
        assert_eq!(all.len(), 1);
    }
}
