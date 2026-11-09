//! The `set` type, with Redis' three encodings and its exact conversion rules:
//!
//! - `intset`: every member is an integer and there are at most `set-max-intset-entries`. A
//!   sorted array whose element width grows 16 → 32 → 64 bits as needed (like Redis' intset), so
//!   members iterate in ascending order.
//! - `listpack`: at most `set-max-listpack-entries` members of at most `set-max-listpack-value`
//!   bytes, in a [`Pack`] (insertion order).
//! - `hashtable`: a [`Dict`] (hash order, Redis-style SCAN cursors).
//!
//! Like Redis, a set only converts "upwards" (intset → listpack → hashtable), except where Redis
//! itself rebuilds a set (SPOP with a large count, SINTERSTORE results).

use crate::config::Config;
use crate::dict::Dict;
use crate::pack::Pack;
use crate::util::{Rng, parse_i64};
use crate::value::Str;
use std::borrow::Cow;

/// Encoding thresholds (`set-max-*` parameters).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub intset_entries: usize,
    pub listpack_entries: usize,
    pub listpack_value: usize,
}

impl Limits {
    pub fn from_cfg(c: &Config) -> Self {
        Self {
            // intsetMaxEntries(): capped at 2^30.
            intset_entries: c.set_max_intset_entries.min(1 << 30),
            listpack_entries: c.set_max_listpack_entries,
            listpack_value: c.set_max_listpack_value,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self { intset_entries: 512, listpack_entries: 128, listpack_value: 64 }
    }
}

/// A sorted integer array with a growable element width (Redis' `intset`).
#[derive(Clone, Debug)]
enum Ints {
    W16(Vec<i16>),
    W32(Vec<i32>),
    W64(Vec<i64>),
}

macro_rules! ints_dispatch {
    ($self:expr, $v:ident => $body:expr) => {
        match $self {
            Ints::W16($v) => $body,
            Ints::W32($v) => $body,
            Ints::W64($v) => $body,
        }
    };
}

impl Ints {
    fn len(&self) -> usize {
        ints_dispatch!(self, v => v.len())
    }

    /// Bytes per member.
    fn width(&self) -> usize {
        match self {
            Ints::W16(_) => 2,
            Ints::W32(_) => 4,
            Ints::W64(_) => 8,
        }
    }

    fn get(&self, i: usize) -> i64 {
        match self {
            Ints::W16(v) => v[i].into(),
            Ints::W32(v) => v[i].into(),
            Ints::W64(v) => v[i],
        }
    }

    fn mem(&self) -> usize {
        match self {
            Ints::W16(v) => v.capacity() * 2,
            Ints::W32(v) => v.capacity() * 4,
            Ints::W64(v) => v.capacity() * 8,
        }
    }

    fn fits(&self, x: i64) -> bool {
        match self {
            Ints::W16(_) => i16::try_from(x).is_ok(),
            Ints::W32(_) => i32::try_from(x).is_ok(),
            Ints::W64(_) => true,
        }
    }

    fn find(&self, x: i64) -> Option<usize> {
        if !self.fits(x) {
            return None;
        }
        match self {
            Ints::W16(v) => v.binary_search(&(x as i16)).ok(),
            Ints::W32(v) => v.binary_search(&(x as i32)).ok(),
            Ints::W64(v) => v.binary_search(&x).ok(),
        }
    }

    fn upgrade_for(&mut self, x: i64) {
        if self.fits(x) {
            return;
        }
        let wide32 = i32::try_from(x).is_ok();
        *self = match std::mem::replace(self, Ints::W16(Vec::new())) {
            Ints::W16(v) if wide32 => Ints::W32(v.into_iter().map(i32::from).collect()),
            Ints::W16(v) => Ints::W64(v.into_iter().map(i64::from).collect()),
            Ints::W32(v) => Ints::W64(v.into_iter().map(i64::from).collect()),
            w @ Ints::W64(_) => w,
        };
    }

    /// Insert keeping the order; false if already present.
    fn insert(&mut self, x: i64) -> bool {
        self.upgrade_for(x);
        match self {
            Ints::W16(v) => match v.binary_search(&(x as i16)) {
                Ok(_) => false,
                Err(i) => {
                    v.insert(i, x as i16);
                    true
                }
            },
            Ints::W32(v) => match v.binary_search(&(x as i32)) {
                Ok(_) => false,
                Err(i) => {
                    v.insert(i, x as i32);
                    true
                }
            },
            Ints::W64(v) => match v.binary_search(&x) {
                Ok(_) => false,
                Err(i) => {
                    v.insert(i, x);
                    true
                }
            },
        }
    }

    fn remove_at(&mut self, i: usize) {
        ints_dispatch!(self, v => { v.remove(i); })
    }

    fn remove(&mut self, x: i64) -> bool {
        match self.find(x) {
            Some(i) => {
                self.remove_at(i);
                true
            }
            None => false,
        }
    }

    fn iter(&self) -> impl Iterator<Item = i64> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }
}

/// Bytes of the hashtable encoding: member heap bytes are tracked as they change.
#[derive(Clone, Debug, Default)]
struct Table {
    d: Dict<Str, ()>,
    heap: usize,
    /// Redis' `sdsAllocSize` of every member (the dict's metadata).
    sds: usize,
}

#[inline]
fn str_heap(s: &Str) -> usize {
    if s.spilled() { s.capacity() } else { 0 }
}

impl Table {
    fn with_capacity(n: usize) -> Self {
        Self { d: Dict::with_capacity(n), heap: 0, sds: 0 }
    }

    fn insert(&mut self, m: &[u8]) -> bool {
        if self.d.contains_key(m) {
            return false;
        }
        let k = Str::from_slice(m);
        self.heap += str_heap(&k);
        self.sds += crate::object::sds_alloc(k.len());
        self.d.insert_new(k, ());
        true
    }

    fn remove(&mut self, m: &[u8]) -> bool {
        match self.d.remove_entry(m) {
            Some((k, ())) => {
                self.heap -= str_heap(&k);
                self.sds -= crate::object::sds_alloc(k.len());
                true
            }
            None => false,
        }
    }

    fn mem(&self) -> usize {
        self.heap + self.d.len() * Dict::<Str, ()>::NODE_OVERHEAD + self.d.table_bytes()
    }
}

#[derive(Clone, Debug)]
enum Enc {
    Ints(Ints),
    Pack(Pack),
    Hash(Table),
}

#[derive(Clone, Debug)]
pub struct Set {
    enc: Enc,
}

impl Default for Set {
    /// An empty intset (`createIntsetObject`); [`Set::add`] converts as members arrive.
    fn default() -> Self {
        Self::new_intset()
    }
}

fn fmt_int(i: i64) -> Vec<u8> {
    itoa::Buffer::new().format(i).as_bytes().to_vec()
}

/// Iterator over a set's members (integers are formatted on the fly).
pub struct Members<'a> {
    inner: MembersInner<'a>,
}

enum MembersInner<'a> {
    Ints(&'a Ints, usize),
    Pack(crate::pack::PackIter<'a>),
    Hash(crate::dict::Iter<'a, Str, ()>),
}

impl<'a> Iterator for Members<'a> {
    type Item = Cow<'a, [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            MembersInner::Ints(v, i) => {
                if *i >= v.len() {
                    return None;
                }
                let x = v.get(*i);
                *i += 1;
                Some(Cow::Owned(fmt_int(x)))
            }
            MembersInner::Pack(it) => it.next().map(Cow::Borrowed),
            MembersInner::Hash(it) => it.next().map(|(k, _)| Cow::Borrowed(&k[..])),
        }
    }
}

impl Set {
    pub fn new_intset() -> Self {
        Self { enc: Enc::Ints(Ints::W16(Vec::new())) }
    }

    pub fn new_listpack() -> Self {
        Self { enc: Enc::Pack(Pack::new()) }
    }

    pub fn new_hashtable(capacity: usize) -> Self {
        Self { enc: Enc::Hash(Table::with_capacity(capacity)) }
    }

    /// `setTypeCreate`: the encoding a new set starts with, given its first member and how many
    /// members are about to be added.
    pub fn create(first: &[u8], size_hint: usize, lim: &Limits) -> Self {
        if parse_i64(first).is_some() && size_hint <= lim.intset_entries {
            Self::new_intset()
        } else if size_hint <= lim.listpack_entries {
            Self::new_listpack()
        } else {
            Self::new_hashtable(size_hint)
        }
    }

    /// RDB load: an intset of `width`-byte integers (2, 4 or 8) from strictly ascending members.
    pub fn from_intset(width: usize, members: impl Iterator<Item = i64>) -> Self {
        let ints = match width {
            2 => Ints::W16(members.map(|x| x as i16).collect()),
            4 => Ints::W32(members.map(|x| x as i32).collect()),
            _ => Ints::W64(members.collect()),
        };
        Self { enc: Enc::Ints(ints) }
    }

    /// RDB save: the element width in bytes and the members (ascending) of an intset.
    pub fn intset(&self) -> Option<(usize, impl Iterator<Item = i64> + '_)> {
        match &self.enc {
            Enc::Ints(v) => {
                let w = match v {
                    Ints::W16(_) => 2,
                    Ints::W32(_) => 4,
                    Ints::W64(_) => 8,
                };
                Some((w, v.iter()))
            }
            _ => None,
        }
    }

    /// RDB load: append `m` to a listpack set as stored, whatever its length. False if it is
    /// already a member (or the set is not a listpack).
    pub fn load_listpack_member(&mut self, m: &[u8]) -> bool {
        match &mut self.enc {
            Enc::Pack(p) if p.position(m).is_none() => {
                p.push_back(m);
                true
            }
            _ => false,
        }
    }

    pub fn len(&self) -> usize {
        match &self.enc {
            Enc::Ints(v) => v.len(),
            Enc::Pack(p) => p.len(),
            Enc::Hash(t) => t.d.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Heap bytes owned, maintained incrementally so this is O(1).
    pub fn mem(&self) -> usize {
        match &self.enc {
            Enc::Ints(v) => v.mem(),
            Enc::Pack(p) => p.mem(),
            Enc::Hash(t) => t.mem(),
        }
    }

    /// `setTypeAllocSize`: the intset, the listpack, or the table with every member's sds.
    pub fn alloc_size(&self) -> usize {
        match &self.enc {
            // `sizeof(intset)` and the members at the set's width.
            Enc::Ints(v) => 8 + v.len() * v.width(),
            Enc::Pack(p) => crate::object::lp_bytes(p.iter()),
            Enc::Hash(t) => crate::object::dict_usage(t.d.len(), t.d.buckets()) + t.sds,
        }
    }

    /// `OBJECT ENCODING`.
    /// DEBUG HTSTATS-KEY: the hashtable's statistics (`None` unless hashtable-encoded).
    pub fn dict_stats(&self, full: bool) -> Option<Vec<crate::dict::DictStats>> {
        match &self.enc {
            Enc::Hash(t) => Some(t.d.stats(full)),
            _ => None,
        }
    }

    pub fn encoding(&self) -> &'static str {
        match &self.enc {
            Enc::Ints(_) => "intset",
            Enc::Pack(_) => "listpack",
            Enc::Hash(_) => "hashtable",
        }
    }

    pub fn is_intset(&self) -> bool {
        matches!(self.enc, Enc::Ints(_))
    }

    pub fn is_listpack(&self) -> bool {
        matches!(self.enc, Enc::Pack(_))
    }

    pub fn is_hashtable(&self) -> bool {
        matches!(self.enc, Enc::Hash(_))
    }

    pub fn contains(&self, member: &[u8]) -> bool {
        match &self.enc {
            Enc::Ints(v) => parse_i64(member).is_some_and(|x| v.find(x).is_some()),
            Enc::Pack(p) => p.position(member).is_some(),
            Enc::Hash(t) => t.d.contains_key(member),
        }
    }

    /// Members in the encoding's order: ascending for intsets, insertion order for listpacks,
    /// hash order for hashtables.
    pub fn iter_members(&self) -> Members<'_> {
        Members {
            inner: match &self.enc {
                Enc::Ints(v) => MembersInner::Ints(v, 0),
                Enc::Pack(p) => MembersInner::Pack(p.iter()),
                Enc::Hash(t) => MembersInner::Hash(t.d.iter()),
            },
        }
    }

    /// The `i`-th member in iteration order (O(1) for intsets, O(i) for listpacks). Not for
    /// hashtables.
    fn nth_small(&self, i: usize) -> Vec<u8> {
        match &self.enc {
            Enc::Ints(v) => fmt_int(v.get(i)),
            Enc::Pack(p) => p.get(i).unwrap().to_vec(),
            Enc::Hash(_) => unreachable!("nth_small on a hashtable"),
        }
    }

    fn convert_to_hash(&mut self, capacity: usize) {
        if self.is_hashtable() {
            return;
        }
        let mut t = Table::with_capacity(capacity.max(self.len()));
        for m in self.iter_members() {
            t.insert(&m);
        }
        self.enc = Enc::Hash(t);
    }

    /// `setTypeAdd`: add a member, converting the encoding when a threshold is crossed. Returns
    /// true if it was added.
    pub fn add(&mut self, m: &[u8], lim: &Limits) -> bool {
        match &mut self.enc {
            Enc::Hash(t) => t.insert(m),
            Enc::Pack(p) => {
                if p.position(m).is_some() {
                    return false;
                }
                if p.len() < lim.listpack_entries && m.len() <= lim.listpack_value {
                    p.push_back(m);
                } else {
                    let n = p.len() + 1;
                    self.convert_to_hash(n);
                    let Enc::Hash(t) = &mut self.enc else { unreachable!() };
                    t.insert(m);
                }
                true
            }
            Enc::Ints(v) => {
                if let Some(x) = parse_i64(m) {
                    if !v.insert(x) {
                        return false;
                    }
                    if v.len() > lim.intset_entries {
                        self.convert_to_hash(0);
                    }
                    return true;
                }
                // A string joins an intset: listpack if everything fits, else hashtable.
                let n = v.len();
                let maxelelen = if n > 0 {
                    let d = |x: i64| itoa::Buffer::new().format(x).len();
                    d(v.get(0)).max(d(v.get(n - 1)))
                } else {
                    0
                };
                if n < lim.listpack_entries && m.len() <= lim.listpack_value && maxelelen <= lim.listpack_value {
                    let mut p = Pack::new();
                    for x in v.iter() {
                        p.push_back(itoa::Buffer::new().format(x).as_bytes());
                    }
                    p.push_back(m);
                    p.shrink_to_fit();
                    self.enc = Enc::Pack(p);
                } else {
                    self.convert_to_hash(n + 1);
                    let Enc::Hash(t) = &mut self.enc else { unreachable!() };
                    t.insert(m);
                }
                true
            }
        }
    }

    /// Remove a member; true if it was present.
    pub fn remove(&mut self, m: &[u8]) -> bool {
        match &mut self.enc {
            Enc::Ints(v) => parse_i64(m).is_some_and(|x| v.remove(x)),
            Enc::Pack(p) => match p.position(m) {
                Some(i) => {
                    p.remove(i);
                    true
                }
                None => false,
            },
            Enc::Hash(t) => t.remove(m),
        }
    }

    /// `setTypeMaybeConvert`: go straight to a hashtable when `size_hint` new members would cross
    /// the current encoding's entry limit.
    pub fn maybe_convert(&mut self, size_hint: usize, lim: &Limits) {
        let convert = match &self.enc {
            Enc::Pack(_) => size_hint > lim.listpack_entries,
            Enc::Ints(_) => size_hint > lim.intset_entries,
            Enc::Hash(_) => false,
        };
        if convert {
            self.convert_to_hash(size_hint);
        }
    }

    /// `maybeConvertToIntset`: become an intset if every member is an integer and there are not
    /// too many (SINTERSTORE results).
    pub fn to_intset_if_possible(&mut self, lim: &Limits) {
        if self.is_intset() || self.len() > lim.intset_entries {
            return;
        }
        let mut v = Ints::W16(Vec::new());
        for m in self.iter_members() {
            match parse_i64(&m) {
                Some(x) => {
                    v.insert(x);
                }
                None => return,
            }
        }
        self.enc = Enc::Ints(v);
    }

    /// A random member (`setTypeRandomElement`). The set must not be empty.
    pub fn random_member(&self, rng: &mut Rng) -> Vec<u8> {
        match &self.enc {
            Enc::Hash(t) => t.d.random(rng).map(|(k, _)| k.to_vec()).unwrap_or_default(),
            _ => self.nth_small(rng.below(self.len())),
        }
    }

    /// Remove and return a random member.
    pub fn pop_random(&mut self, rng: &mut Rng) -> Option<Vec<u8>> {
        if self.is_empty() {
            return None;
        }
        match &mut self.enc {
            Enc::Ints(v) => {
                let i = rng.below(v.len());
                let x = v.get(i);
                v.remove_at(i);
                Some(fmt_int(x))
            }
            Enc::Pack(p) => p.remove(rng.below(p.len())),
            Enc::Hash(t) => {
                let m = t.d.random(rng).map(|(k, _)| k.to_vec())?;
                t.remove(&m);
                Some(m)
            }
        }
    }

    /// One SSCAN step over a hashtable (`f` gets each member); small encodings return everything
    /// and cursor 0, like Redis.
    pub fn scan(&self, cursor: u64, mut f: impl FnMut(&[u8])) -> u64 {
        match &self.enc {
            Enc::Hash(t) => t.d.scan(cursor, |k, _| f(k)),
            _ => {
                for m in self.iter_members() {
                    f(&m);
                }
                0
            }
        }
    }

    /// Release spare capacity (after bulk removals).
    pub fn shrink_to_fit(&mut self) {
        match &mut self.enc {
            Enc::Ints(v) => ints_dispatch!(v, x => x.shrink_to_fit()),
            Enc::Pack(p) => p.shrink_to_fit(),
            Enc::Hash(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn members(s: &Set) -> Vec<String> {
        s.iter_members().map(|m| String::from_utf8_lossy(&m).into_owned()).collect()
    }

    #[test]
    fn intset_sorted_and_widening() {
        let lim = Limits::default();
        let mut s = Set::create(b"5", 3, &lim);
        assert!(s.add(b"5", &lim));
        assert!(s.add(b"-3", &lim));
        assert!(!s.add(b"5", &lim));
        assert!(s.add(b"70000", &lim));
        assert!(s.add(b"-9223372036854775808", &lim));
        assert_eq!(s.encoding(), "intset");
        assert_eq!(members(&s), ["-9223372036854775808", "-3", "5", "70000"]);
        assert!(s.contains(b"70000") && !s.contains(b"70001") && !s.contains(b"x"));
        assert!(s.remove(b"-3") && !s.remove(b"-3"));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn conversions() {
        let lim = Limits::default();
        let mut s = Set::create(b"1", 2, &lim);
        s.add(b"2", &lim);
        s.add(b"1", &lim);
        s.add(b"a", &lim);
        assert_eq!(s.encoding(), "listpack");
        assert_eq!(members(&s), ["1", "2", "a"]);
        s.add(&[b'x'; 65], &lim);
        assert_eq!(s.encoding(), "hashtable");

        let mut s = Set::create(b"1", 1, &lim);
        for i in 0..512 {
            s.add(i.to_string().as_bytes(), &lim);
        }
        assert_eq!(s.encoding(), "intset");
        s.add(b"9999", &lim);
        assert_eq!(s.encoding(), "hashtable");

        let mut s = Set::create(b"a", 1, &lim);
        for i in 0..128 {
            s.add(format!("v{i}").as_bytes(), &lim);
        }
        assert_eq!(s.encoding(), "listpack");
        s.add(b"z", &lim);
        assert_eq!(s.encoding(), "hashtable");
        assert_eq!(s.len(), 129);
        assert!(s.contains(b"v5"));
    }

    #[test]
    fn to_intset() {
        let lim = Limits::default();
        let mut s = Set::new_listpack();
        s.add(b"3", &lim);
        s.add(b"1", &lim);
        s.to_intset_if_possible(&lim);
        assert_eq!(s.encoding(), "intset");
        assert_eq!(members(&s), ["1", "3"]);
    }

    #[test]
    fn mem_tracks() {
        let lim = Limits::default();
        let mut s = Set::new_hashtable(0);
        let long = vec![b'q'; 100];
        s.add(&long, &lim);
        let m1 = s.mem();
        assert!(m1 >= 100);
        s.remove(&long);
        assert!(s.mem() < m1);
    }
}
