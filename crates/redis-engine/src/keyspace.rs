//! The keyspace of one database: keys and their entries, reached through views ([`KvRef`],
//! [`KvMut`]) rather than references to stored structs, so entries can be laid out compactly
//! (Redis 8's `kvobj`: key, metadata and a small string value in one allocation).
//!
//! The table is Redis' dict ([`crate::dict`]): the same buckets, rehashing and SCAN guarantees.

use crate::dict::{Dict, DictStats};
use crate::util::Rng;
use crate::value::{Entry, Str, Value, ValueRef};

/// An entry's parts, however it is stored.
pub trait Fields {
    fn value(&self) -> ValueRef<'_>;
    /// Absolute expiry in Unix ms; 0 means no TTL.
    fn expire(&self) -> i64;
    fn lru(&self) -> u32;
    /// The keysizes slot it is counted in.
    fn sized(&self) -> u16;
    /// How Redis would hold the object ([`crate::object`]).
    fn obj(&self) -> u8;
    /// A grown raw string's sds size class.
    fn sds(&self) -> u8;

    #[inline]
    fn expired(&self, now: i64) -> bool {
        let e = self.expire();
        e != 0 && e <= now
    }
}

/// Changing an entry's metadata.
pub trait FieldsMut: Fields {
    fn set_lru(&mut self, v: u32);
    fn set_sized(&mut self, v: u16);
    fn set_obj(&mut self, v: u8);
    fn set_sds(&mut self, v: u8);
}

impl Fields for Entry {
    #[inline]
    fn value(&self) -> ValueRef<'_> {
        self.value.view()
    }
    #[inline]
    fn expire(&self) -> i64 {
        self.expire
    }
    #[inline]
    fn lru(&self) -> u32 {
        self.lru
    }
    #[inline]
    fn sized(&self) -> u16 {
        self.sized
    }
    #[inline]
    fn obj(&self) -> u8 {
        self.obj
    }
    #[inline]
    fn sds(&self) -> u8 {
        self.sds
    }
}

impl FieldsMut for Entry {
    #[inline]
    fn set_lru(&mut self, v: u32) {
        self.lru = v;
    }
    #[inline]
    fn set_sized(&mut self, v: u16) {
        self.sized = v;
    }
    #[inline]
    fn set_obj(&mut self, v: u8) {
        self.obj = v;
    }
    #[inline]
    fn set_sds(&mut self, v: u8) {
        self.sds = v;
    }
}

/// A stored entry, read.
#[derive(Clone, Copy)]
pub struct KvRef<'a> {
    e: &'a Entry,
}

impl<'a> KvRef<'a> {
    #[inline]
    pub fn value(self) -> ValueRef<'a> {
        self.e.value.view()
    }

    #[inline]
    pub fn expire(self) -> i64 {
        self.e.expire
    }

    #[inline]
    pub fn expired(self, now: i64) -> bool {
        self.e.expired(now)
    }

    #[inline]
    pub fn lru(self) -> u32 {
        self.e.lru
    }

    #[inline]
    pub fn obj(self) -> u8 {
        self.e.obj
    }

    /// Where the entry is (DEBUG OBJECT's `Value at`).
    #[inline]
    pub fn addr(self) -> *const u8 {
        self.e as *const Entry as *const u8
    }

    /// An owned copy of the entry.
    pub fn to_entry(self) -> Entry {
        self.e.clone()
    }

    /// Heap bytes the value owns ([`Value::mem`]).
    #[inline]
    pub fn value_mem(self) -> usize {
        self.e.value.mem()
    }
}

impl<'a> From<&'a Entry> for KvRef<'a> {
    /// An owned entry seen like a stored one.
    fn from(e: &'a Entry) -> Self {
        KvRef { e }
    }
}

impl Fields for KvRef<'_> {
    #[inline]
    fn value(&self) -> ValueRef<'_> {
        self.e.value.view()
    }
    #[inline]
    fn expire(&self) -> i64 {
        self.e.expire
    }
    #[inline]
    fn lru(&self) -> u32 {
        self.e.lru
    }
    #[inline]
    fn sized(&self) -> u16 {
        self.e.sized
    }
    #[inline]
    fn obj(&self) -> u8 {
        self.e.obj
    }
    #[inline]
    fn sds(&self) -> u8 {
        self.e.sds
    }
}

/// A stored entry, to change.
pub struct KvMut<'a> {
    e: &'a mut Entry,
}

impl<'a> KvMut<'a> {
    #[inline]
    pub fn as_ref(&self) -> KvRef<'_> {
        KvRef { e: self.e }
    }

    #[inline]
    pub fn expire(&self) -> i64 {
        self.e.expire
    }

    #[inline]
    pub fn expired(&self, now: i64) -> bool {
        self.e.expired(now)
    }

    #[inline]
    pub fn into_ref(self) -> KvRef<'a> {
        KvRef { e: self.e }
    }

    /// The value of an aggregate (any type but a string) to change in place; `None` for a
    /// string (see [`KvMut::take_value`]).
    #[inline]
    pub fn aggregate_mut(&mut self) -> Option<&mut Value> {
        (!matches!(self.e.value, Value::Str(_))).then_some(&mut self.e.value)
    }

    /// [`KvMut::aggregate_mut`] for the rest of the borrow.
    #[inline]
    pub fn into_aggregate_mut(self) -> Option<&'a mut Value> {
        (!matches!(self.e.value, Value::Str(_))).then_some(&mut self.e.value)
    }

    /// Take the value out to change it (a string's bytes copied out of the entry); give it
    /// back with [`KvMut::put_value`]. Meanwhile the entry holds an empty string.
    #[inline]
    pub fn take_value(&mut self) -> Value {
        std::mem::replace(&mut self.e.value, Value::Str(Str::new()))
    }

    /// Store `v` as the value.
    #[inline]
    pub fn put_value(&mut self, v: Value) {
        self.e.value = v;
    }

    /// Replace the value, returning the old one.
    #[inline]
    pub fn replace_value(&mut self, v: Value) -> Value {
        std::mem::replace(&mut self.e.value, v)
    }

    /// Change the value through `f`.
    pub fn with_value<R>(&mut self, f: impl FnOnce(&mut Value) -> R) -> R {
        match self.aggregate_mut() {
            Some(v) => f(v),
            None => {
                let mut v = self.take_value();
                let r = f(&mut v);
                self.put_value(v);
                r
            }
        }
    }

    /// Set the raw expiry ([`crate::db::Db::set_expire`] keeps the volatile index).
    #[inline]
    pub(crate) fn set_expire_raw(&mut self, at: i64) {
        self.e.expire = at;
    }

    /// Heap bytes the value owns ([`Value::mem`]).
    #[inline]
    pub fn value_mem(&self) -> usize {
        self.e.value.mem()
    }
}

impl Fields for KvMut<'_> {
    #[inline]
    fn value(&self) -> ValueRef<'_> {
        self.e.value.view()
    }
    #[inline]
    fn expire(&self) -> i64 {
        self.e.expire
    }
    #[inline]
    fn lru(&self) -> u32 {
        self.e.lru
    }
    #[inline]
    fn sized(&self) -> u16 {
        self.e.sized
    }
    #[inline]
    fn obj(&self) -> u8 {
        self.e.obj
    }
    #[inline]
    fn sds(&self) -> u8 {
        self.e.sds
    }
}

impl FieldsMut for KvMut<'_> {
    #[inline]
    fn set_lru(&mut self, v: u32) {
        self.e.lru = v;
    }
    #[inline]
    fn set_sized(&mut self, v: u16) {
        self.e.sized = v;
    }
    #[inline]
    fn set_obj(&mut self, v: u8) {
        self.e.obj = v;
    }
    #[inline]
    fn set_sds(&mut self, v: u8) {
        self.e.sds = v;
    }
}

/// The keys of one database.
#[derive(Default)]
pub struct KeySpace {
    d: Dict<Str, Entry>,
}

impl KeySpace {
    #[inline]
    pub fn len(&self) -> usize {
        self.d.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.d.is_empty()
    }

    #[inline]
    pub fn is_rehashing(&self) -> bool {
        self.d.is_rehashing()
    }

    pub fn stats(&self, full: bool) -> Vec<DictStats> {
        self.d.stats(full)
    }

    pub fn expand(&mut self, n: usize) {
        self.d.expand(n)
    }

    pub fn table_bytes(&self) -> usize {
        self.d.table_bytes()
    }

    pub fn buckets(&self) -> usize {
        self.d.buckets()
    }

    pub fn set_resize(&mut self, on: bool) {
        self.d.set_resize(on)
    }

    pub fn rehash(&mut self, steps: usize) -> bool {
        self.d.rehash(steps)
    }

    pub fn clear(&mut self) {
        self.d.clear()
    }

    #[inline]
    pub fn get(&self, key: &[u8]) -> Option<KvRef<'_>> {
        self.d.get(key).map(|e| KvRef { e })
    }

    #[inline]
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.d.contains_key(key)
    }

    /// The entry to change (a rehash step, as Redis' lookups take).
    #[inline]
    pub fn get_mut(&mut self, key: &[u8]) -> Option<KvMut<'_>> {
        self.d.get_mut(key).map(|e| KvMut { e })
    }

    /// Add a key known to be absent.
    pub fn insert_new(&mut self, key: &[u8], e: Entry) -> KvMut<'_> {
        KvMut { e: self.d.insert_new(Str::from_slice(key), e) }
    }

    pub fn remove_entry(&mut self, key: &[u8]) -> Option<(Str, Entry)> {
        self.d.remove_entry(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], KvRef<'_>)> {
        self.d.iter().map(|(k, e)| (k.as_slice(), KvRef { e }))
    }

    pub fn scan(&self, cursor: u64, mut f: impl FnMut(&[u8], KvRef<'_>)) -> u64 {
        self.d.scan(cursor, |k, e| f(k, KvRef { e }))
    }

    pub fn random(&self, rng: &mut Rng) -> Option<(&[u8], KvRef<'_>)> {
        self.d.random(rng).map(|(k, e)| (k.as_slice(), KvRef { e }))
    }

    pub fn sample(&self, rng: &mut Rng, count: usize, mut f: impl FnMut(&[u8], KvRef<'_>)) {
        self.d.sample(rng, count, |k, e| f(k, KvRef { e }))
    }

    pub fn for_each_mut(&mut self, mut f: impl FnMut(&[u8], KvMut<'_>)) {
        self.d.for_each_mut(|k, e| f(k, KvMut { e }))
    }
}
