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
//! - `template-listpack` / `template-array` (Redis 8.10): the field names live in a shared
//!   template (sorted, see [`super::template`]) and the hash keeps only its values, in
//!   template order: a [`Pack`] while they fit the listpack limits, else an array.
//!
//! A hash starts as `listpack`, becomes `listpackex` the first time a field-expiration command
//! touches it, and becomes `hashtable` once it outgrows `hash-max-listpack-entries` /
//! `hash-max-listpack-value`. It never converts back. Template hashes are created by
//! `HIMPORT SET`, `RESTORE` and the `hash-*-template-entries` conversions; adding or removing a
//! field moves them to another template, and a field TTL turns them into plain hashes.
//!
//! Expiry times are absolute Unix milliseconds; 0 means "no TTL". The type itself never looks at
//! the clock: readers that care pass `now` ([`Hash::get_live`], [`Hash::iter_live`]) and the
//! command layer decides when expired fields are deleted (lazily, or by active expiry through
//! [`Hash::expire_due`]).

use super::template::{KeyRef, LP_SAFETY, Registry, field_cmp};
use crate::config::Config;
use crate::dict::Dict;
use crate::pack::Pack;
use crate::util::Rng;
use crate::value::Str;
use std::collections::BTreeSet;
use std::sync::Arc;

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
    Tmpl(Box<Tmpl>),
}

/// A template hash: the values of `t`'s fields, in its order.
#[derive(Clone, Debug)]
struct Tmpl {
    t: KeyRef,
    vals: Vals,
}

#[derive(Clone, Debug)]
enum Vals {
    /// `template-listpack`.
    Pack(Pack),
    /// `template-array`.
    Array(Vec<Str>),
}

impl Vals {
    fn get(&self, i: usize) -> Option<&[u8]> {
        match self {
            Vals::Pack(p) => p.get(i),
            Vals::Array(a) => a.get(i).map(|v| v.as_slice()),
        }
    }

    fn set(&mut self, i: usize, v: &[u8]) {
        match self {
            Vals::Pack(p) => {
                p.set(i, v);
            }
            Vals::Array(a) => a[i] = Str::from_slice(v),
        }
    }

    fn insert(&mut self, i: usize, v: &[u8]) {
        match self {
            Vals::Pack(p) => p.insert(i, v),
            Vals::Array(a) => a.insert(i, Str::from_slice(v)),
        }
    }

    fn remove(&mut self, i: usize) {
        match self {
            Vals::Pack(p) => {
                p.remove(i);
            }
            Vals::Array(a) => {
                a.remove(i);
            }
        }
    }

    fn make_array(&mut self) {
        if let Vals::Pack(p) = self {
            *self = Vals::Array(p.iter().map(Str::from_slice).collect());
        }
    }

    fn mem(&self) -> usize {
        match self {
            Vals::Pack(p) => p.mem(),
            Vals::Array(a) => a.capacity() * std::mem::size_of::<Str>() + a.iter().map(spill).sum::<usize>(),
        }
    }
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

    /// RDB load: an empty `listpack` (`listpackex` when `ex`) to fill with
    /// [`Hash::load_pack_entry`].
    pub fn new_pack(ex: bool) -> Self {
        let enc = if ex { Enc::PackEx(PackEx::default()) } else { Enc::Pack(Pack::new()) };
        Hash { enc, reg: 0 }
    }

    /// RDB load: append a pair as stored, whatever its size. In `listpackex` the fields with a
    /// TTL come first, in ascending order (the caller checks); `at` is ignored for `listpack`.
    pub fn load_pack_entry(&mut self, field: &[u8], value: &[u8], at: i64) {
        match &mut self.enc {
            Enc::Pack(p) => {
                p.push_back(field);
                p.push_back(value);
            }
            Enc::PackEx(x) => {
                x.pack.push_back(field);
                x.pack.push_back(value);
                if at != 0 {
                    x.ttl.push(at);
                }
            }
            Enc::Table(_) | Enc::Tmpl(_) => {}
        }
    }

    /// A template hash with `vals` (in `t`'s order), `template-listpack` when the values fit
    /// the listpack limits (`createHashObjectFromTemplate`).
    pub fn from_template<V: AsRef<[u8]>>(t: KeyRef, vals: &[V], lim: Limits) -> Self {
        let lp = super::template::LpLimits { entries: lim.entries, value: lim.value };
        let vals = if lp.fits(vals) {
            Vals::Pack(Pack::from_items(vals))
        } else {
            Vals::Array(vals.iter().map(|v| Str::from_slice(v.as_ref())).collect())
        };
        Hash { enc: Enc::Tmpl(Box::new(Tmpl { t, vals })), reg: 0 }
    }

    /// A `template-listpack` read from RDB: kept packed whatever the value sizes, a
    /// `template-array` past `hash-max-listpack-entries`.
    pub fn from_template_pack<V: AsRef<[u8]>>(t: KeyRef, vals: &[V], lim: Limits) -> Self {
        let mut vals = Vals::Pack(Pack::from_items(vals));
        if t.len() > lim.entries {
            vals.make_array();
        }
        Hash { enc: Enc::Tmpl(Box::new(Tmpl { t, vals })), reg: 0 }
    }

    /// The template of a template hash.
    pub fn template(&self) -> Option<&KeyRef> {
        match &self.enc {
            Enc::Tmpl(x) => Some(&x.t),
            _ => None,
        }
    }

    pub fn is_template(&self) -> bool {
        matches!(self.enc, Enc::Tmpl(_))
    }

    /// `template-listpack` (as opposed to `template-array`).
    pub fn is_template_listpack(&self) -> bool {
        matches!(&self.enc, Enc::Tmpl(x) if matches!(x.vals, Vals::Pack(_)))
    }

    pub fn is_listpack_ex(&self) -> bool {
        matches!(self.enc, Enc::PackEx(_))
    }

    /// Number of stored fields, including expired ones not deleted yet (what HLEN reports).
    pub fn len(&self) -> usize {
        match &self.enc {
            Enc::Pack(p) => p.pairs(),
            Enc::PackEx(x) => x.pack.pairs(),
            Enc::Table(t) => t.map.len(),
            Enc::Tmpl(x) => x.t.len(),
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
            Enc::Tmpl(x) => std::mem::size_of::<Tmpl>() + x.vals.mem(),
        }
    }

    /// DEBUG HTSTATS-KEY: the hashtable's statistics (`None` unless hashtable-encoded).
    pub fn dict_stats(&self, full: bool) -> Option<Vec<crate::dict::DictStats>> {
        match &self.enc {
            Enc::Table(t) => Some(t.map.stats(full)),
            _ => None,
        }
    }

    pub fn encoding(&self) -> &'static str {
        match &self.enc {
            Enc::Pack(_) => "listpack",
            Enc::PackEx(_) => "listpackex",
            Enc::Table(_) => "hashtable",
            Enc::Tmpl(x) => match x.vals {
                Vals::Pack(_) => "template-listpack",
                Vals::Array(_) => "template-array",
            },
        }
    }

    /// `listpack` or `listpackex`.
    pub fn is_listpack(&self) -> bool {
        matches!(self.enc, Enc::Pack(_) | Enc::PackEx(_))
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
            Enc::Tmpl(x) => x.t.index(field).ok().and_then(|i| x.vals.get(i)).map(|v| (v, 0)),
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
            Enc::Pack(_) | Enc::Tmpl(_) => 0,
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
    /// `hashtable`, template order for templates (what HGETALL returns).
    pub fn iter(&self) -> Iter<'_> {
        match &self.enc {
            Enc::Pack(p) => Iter(Inner::Pack(p.iter())),
            Enc::PackEx(x) => Iter(Inner::PackEx(x.pack.iter(), &x.ttl, 0)),
            Enc::Table(t) => Iter(Inner::Table(t.map.iter(), t.ttl.as_deref())),
            Enc::Tmpl(x) => match &x.vals {
                Vals::Pack(p) => Iter(Inner::TmplPack(x.t.fields().iter(), p.iter())),
                Vals::Array(a) => Iter(Inner::TmplArray(x.t.fields().iter(), a.iter())),
            },
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
            Enc::Tmpl(x) => x.t.len(),
            Enc::PackEx(x) => x.pack.pairs() - x.ttl.iter().take_while(|&&t| t < now).count(),
            Enc::Table(t) => {
                let gone = t.ttl.as_ref().map_or(0, |t| t.order.iter().take_while(|(at, _)| *at < now).count());
                t.map.len() - gone
            }
        }
    }

    /// The pair at position `i` of a listpack or template encoding (`None` for a hashtable or
    /// out of range).
    pub fn pack_pair(&self, i: usize) -> Option<(&[u8], &[u8])> {
        let p = match &self.enc {
            Enc::Pack(p) => p,
            Enc::PackEx(x) => &x.pack,
            Enc::Table(_) => return None,
            Enc::Tmpl(x) => return Some((x.t.fields().get(i)?, x.vals.get(i)?)),
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
    /// A `template-listpack` only checks the values, and becomes a `template-array`.
    pub fn prepare(&mut self, pairs: usize, lens: impl IntoIterator<Item = usize>, lim: Limits) {
        if let Enc::Tmpl(x) = &mut self.enc {
            if let Vals::Pack(p) = &x.vals {
                let mut sum = 0usize;
                for l in lens.into_iter().skip(1).step_by(2) {
                    if l > lim.value {
                        x.vals.make_array();
                        return;
                    }
                    sum += l;
                }
                if p.bytes() + sum > LP_SAFETY {
                    x.vals.make_array();
                }
            }
            return;
        }
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

    /// Start using field TTLs (`hashTypeSetExInit`): `listpack` becomes `listpackex`, a template
    /// hash becomes `listpackex` if it fits the listpack limits, else `hashtable`, and a
    /// `hashtable` gets its (empty) TTL index.
    pub fn enable_ttl(&mut self, lim: Limits) {
        match &mut self.enc {
            Enc::Pack(p) => {
                let pack = std::mem::take(p);
                self.enc = Enc::PackEx(PackEx { pack, ttl: Vec::new() });
            }
            Enc::Tmpl(_) => self.untemplate(true, lim),
            Enc::Table(t) => {
                t.ttl.get_or_insert_with(Box::default);
            }
            Enc::PackEx(_) => {}
        }
    }

    /// `hashTypeCanConvertTmplToListpack`.
    fn template_fits_listpack(x: &Tmpl, lim: Limits) -> bool {
        if x.t.len() > lim.entries {
            return false;
        }
        let mut sum = 0usize;
        for f in x.t.fields() {
            if f.len() > lim.value {
                return false;
            }
            sum += f.len();
        }
        match &x.vals {
            Vals::Array(a) => {
                for v in a {
                    if v.len() > lim.value {
                        return false;
                    }
                    sum += v.len();
                }
                sum <= LP_SAFETY
            }
            Vals::Pack(p) => p.bytes() + sum <= LP_SAFETY,
        }
    }

    /// `hashTypeConvertTmplToListpackOrHT`: a template hash becomes a plain `listpack` (or
    /// `listpackex` when `ex`) in template order, or a `hashtable` if it doesn't fit.
    pub fn untemplate(&mut self, ex: bool, lim: Limits) {
        let Enc::Tmpl(x) = &self.enc else { return };
        let enc = if Self::template_fits_listpack(x, lim) {
            let mut pack = Pack::new();
            for (f, v, _) in self.iter() {
                pack.push_back(f);
                pack.push_back(v);
            }
            if ex { Enc::PackEx(PackEx { pack, ttl: Vec::new() }) } else { Enc::Pack(pack) }
        } else {
            let mut t = Table::with_capacity(self.len());
            for (f, v, _) in self.iter() {
                t.insert_new(Str::from_slice(f), Str::from_slice(v));
            }
            if ex {
                t.ttl = Some(Box::default());
            }
            Enc::Table(Box::new(t))
        };
        self.enc = enc;
    }

    /// Does this hash use field expiration (a `listpackex`, or a `hashtable` with a TTL index)?
    fn uses_ttl(&self) -> bool {
        match &self.enc {
            Enc::PackEx(_) => true,
            Enc::Table(t) => t.ttl.is_some(),
            _ => false,
        }
    }

    /// `hashTypeTryConvertToTemplate`: with `min` to `max` fields (`min` 0: never, `max` 0: no
    /// upper bound), a `listpack` becomes a `template-listpack` and a `hashtable` without field
    /// expiration a `template-array`. Returns whether it converted.
    pub fn try_template(&mut self, reg: &Arc<Registry>, min: usize, max: usize) -> bool {
        self.try_template_with(min, max, |fields| Some(reg.key_ref(fields)))
    }

    /// [`Hash::try_template`] taking the template from `get` (which may refuse).
    pub fn try_template_with(&mut self, min: usize, max: usize, get: impl FnOnce(&[Str]) -> Option<KeyRef>) -> bool {
        if min == 0 || self.is_template() || self.uses_ttl() {
            return false;
        }
        let n = self.len();
        if n < min || (max > 0 && n > max) {
            return false;
        }
        let mut pairs: Vec<(&[u8], &[u8])> = self.iter().map(|(f, v, _)| (f, v)).collect();
        pairs.sort_by(|a, b| field_cmp(a.0, b.0));
        let fields: Vec<Str> = pairs.iter().map(|(f, _)| Str::from_slice(f)).collect();
        let Some(t) = get(&fields) else { return false };
        let vals = match &self.enc {
            Enc::Pack(_) => Vals::Pack(Pack::from_items(pairs.iter().map(|(_, v)| v))),
            _ => Vals::Array(pairs.iter().map(|(_, v)| Str::from_slice(v)).collect()),
        };
        drop(pairs);
        self.enc = Enc::Tmpl(Box::new(Tmpl { t, vals }));
        true
    }

    /// Move a template hash to the template of `fields` (`hashTemplateForInsertedField` /
    /// `ForDeletedField`), or make it plain if the registry is gone.
    fn retemplate(x: &mut Tmpl, fields: &[Str]) -> bool {
        match x.t.registry() {
            Some(r) => {
                // The new reference is taken before the old one is dropped, like Redis.
                x.t = r.key_ref(fields);
                true
            }
            None => false,
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
            Enc::Tmpl(x) => {
                for (f, v) in x.t.fields().iter().zip((0..).map_while(|i| x.vals.get(i))) {
                    t.insert_new(f.clone(), Str::from_slice(v));
                }
            }
        }
        self.enc = Enc::Table(Box::new(t));
    }

    /// HSET semantics (`hashTypeSet`): set `field` to `value`, returning true when the field is
    /// new. An existing TTL is removed unless `keep_ttl` (in `listpackex` the field then moves to
    /// the end). Converts to `hashtable` when the entry count exceeds `lim.entries`; value
    /// lengths are checked by [`Hash::prepare`].
    pub fn set(&mut self, field: &[u8], value: &[u8], keep_ttl: bool, lim: Limits) -> bool {
        if let Enc::Tmpl(x) = &mut self.enc {
            let idx = x.t.index(field);
            if let Vals::Pack(p) = &x.vals
                && (value.len() > lim.value
                    || p.bytes() + value.len() > LP_SAFETY
                    || (idx.is_err() && x.t.len() + 1 > lim.entries))
            {
                x.vals.make_array();
            }
            match idx {
                Ok(i) => {
                    x.vals.set(i, value);
                    return false;
                }
                Err(pos) => {
                    let mut fields = x.t.fields().to_vec();
                    fields.insert(pos, Str::from_slice(field));
                    if Self::retemplate(x, &fields) {
                        x.vals.insert(pos, value);
                        return true;
                    }
                    self.untemplate(false, lim);
                }
            }
        }
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
            Enc::Tmpl(_) => unreachable!("handled above"),
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
            Enc::Tmpl(x) => {
                let Ok(i) = x.t.index(field) else { return false };
                if x.t.len() == 1 {
                    // The last field: an empty listpack (the key is deleted by the caller).
                    self.enc = Enc::Pack(Pack::new());
                    return true;
                }
                let mut fields = x.t.fields().to_vec();
                fields.remove(i);
                if !Self::retemplate(x, &fields) {
                    let lim = Limits { entries: usize::MAX, value: usize::MAX };
                    self.untemplate(false, lim);
                    return self.remove(field);
                }
                x.vals.remove(i);
                true
            }
        }
    }

    /// Set (or clear, with 0) the expiry of an existing `field`. Turns a `listpack` into
    /// `listpackex`. In `listpackex` the field is re-inserted at its place by expiry time.
    /// Returns false if the field doesn't exist.
    pub fn set_expire(&mut self, field: &[u8], at: i64) -> bool {
        if !self.uses_ttl() {
            let lim = self.template().and_then(|t| t.registry()).map_or(
                Limits { entries: usize::MAX, value: usize::MAX },
                |r| Limits { entries: r.limits().entries, value: r.limits().value },
            );
            self.enable_ttl(lim);
        }
        match &mut self.enc {
            Enc::Pack(_) | Enc::Tmpl(_) => unreachable!("converted above"),
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
            Enc::Pack(_) | Enc::Tmpl(_) => {}
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
            Enc::Tmpl(x) => {
                if let Vals::Pack(p) = &mut x.vals {
                    p.shrink_to_fit();
                }
            }
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
    TmplPack(std::slice::Iter<'a, Str>, crate::pack::PackIter<'a>),
    TmplArray(std::slice::Iter<'a, Str>, std::slice::Iter<'a, Str>),
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
            Inner::TmplPack(f, v) => Some((f.next()?.as_slice(), v.next()?, 0)),
            Inner::TmplArray(f, v) => Some((f.next()?.as_slice(), v.next()?.as_slice(), 0)),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = match &self.0 {
            Inner::Pack(it) | Inner::PackEx(it, _, _) => it.len() / 2,
            Inner::Table(it, _) => it.size_hint().0,
            Inner::TmplPack(it, _) | Inner::TmplArray(it, _) => it.len(),
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
    fn templates_move_between_field_sets() {
        let reg = Registry::new();
        let lim = Limits { entries: 4, value: 8 };
        let mut h = Hash::new();
        for (f, v) in [("b", "1"), ("a", "2"), ("ccc", "3")] {
            h.set(f.as_bytes(), v.as_bytes(), false, lim);
        }
        assert!(!h.try_template(&reg, 4, 0));
        assert!(h.try_template(&reg, 3, 3));
        assert_eq!((h.encoding(), fields(&h)), ("template-listpack", vec!["a".into(), "b".into(), "ccc".into()]));
        let mut h2 = h.clone();
        assert_eq!((reg.stats().templates, reg.stats().keys), (1, 2));
        // A new field: another template (the old one stays for the clone).
        assert!(h.set(b"dd", b"4", false, lim));
        assert_eq!(fields(&h), ["a", "b", "dd", "ccc"]);
        assert_eq!((reg.stats().templates, reg.stats().keys), (2, 2));
        // Past the entry limit, or a long value: template-array.
        assert!(h.set(b"e", b"5", false, lim));
        assert_eq!(h.encoding(), "template-array");
        assert!(!h2.set(b"a", b"123456789", false, lim));
        assert_eq!((h2.encoding(), h2.get(b"a")), ("template-array", Some(&b"123456789"[..])));
        // Removing fields; the last one leaves an empty listpack.
        assert!(h2.remove(b"b") && !h2.remove(b"zz"));
        assert_eq!(fields(&h2), ["a", "ccc"]);
        assert!(h2.remove(b"a") && h2.remove(b"ccc"));
        assert_eq!((h2.encoding(), h2.len()), ("listpack", 0));
        // A field TTL makes it plain: listpackex if it fits, else hashtable.
        let mut small = Hash::from_template(reg.key_ref(&[Str::from_slice(b"x")]), &[b"1"], lim);
        small.enable_ttl(lim);
        assert_eq!((small.encoding(), small.get(b"x")), ("listpackex", Some(&b"1"[..])));
        h.enable_ttl(lim);
        assert_eq!((h.encoding(), h.len()), ("hashtable", 5));
        drop((h, h2, small));
        assert_eq!((reg.stats().templates, reg.stats().keys), (0, 0));
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
