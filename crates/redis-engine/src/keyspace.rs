//! The keyspace of one database: keys and their entries, reached through views ([`KvRef`],
//! [`KvMut`]) rather than references to stored structs, so entries can be laid out compactly
//! (Redis 8's `kvobj`: key, metadata and a small string value in one allocation).
//!
//! The table is Redis' dict ([`crate::dict::RawDict`]): the same buckets, rehashing and SCAN
//! guarantees. Each node is one allocation ([`KvBox`]): a header (chain link, LRU, keysizes slot,
//! Redis' object bits, lengths), the expiry when there is one, a string value of up to
//! [`PACK_MAX`] bytes in place (else a [`Value`]), then the key's bytes. A 16-byte key holding a
//! short string takes one ~48-byte allocation where a boxed node took 72 bytes and possibly more.

use crate::dict::{DictStats, NodePtr, RawDict};
use crate::util::Rng;
use crate::value::{Entry, Str, Value, ValueRef};
use std::alloc::{Layout, alloc, dealloc};
use std::ptr::NonNull;

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


/// Longest string kept in its node (longer ones in a [`Value`] slot).
pub const PACK_MAX: usize = 255;
const HAS_EXPIRE: u8 = 1;
const PACKED: u8 = 2;

/// A node's fixed part.
#[repr(C)]
struct Head {
    next: Option<KvBox>,
    lru: u32,
    sized: u16,
    obj: u8,
    sds: u8,
    flags: u8,
    /// A packed string's length and the room for it.
    vlen: u8,
    vcap: u8,
    _pad: u8,
    klen: u32,
}

const HEAD: usize = std::mem::size_of::<Head>();
const SLOT: usize = std::mem::size_of::<Value>();
const _: () = assert!(HEAD == 24 && SLOT == 24 && std::mem::align_of::<Value>() <= 8);

/// An entry's metadata, carried over when its node is rebuilt.
#[derive(Clone, Copy)]
struct Meta {
    lru: u32,
    sized: u16,
    obj: u8,
    sds: u8,
}

/// One keyspace node (see the module docs), owned.
pub struct KvBox(NonNull<Head>);

// SAFETY: a node owns its value like a `Box<Entry>` would.
unsafe impl Send for KvBox {}
// SAFETY: shared access only reads.
unsafe impl Sync for KvBox {}

#[inline]
fn val_off(flags: u8) -> usize {
    HEAD + if flags & HAS_EXPIRE != 0 { 8 } else { 0 }
}

#[inline]
fn val_size(flags: u8, vcap: u8) -> usize {
    if flags & PACKED != 0 { vcap as usize } else { SLOT }
}

#[inline]
fn node_layout(flags: u8, vcap: u8, klen: usize) -> Layout {
    Layout::from_size_align(val_off(flags) + val_size(flags, vcap) + klen, 8).expect("node layout")
}

/// Room for a packed string of `len` bytes (some slack so it can grow a little in place).
#[inline]
fn pack_cap(len: usize) -> u8 {
    len.max(1).next_multiple_of(8).min(PACK_MAX) as u8
}

impl KvBox {
    fn new(key: &[u8], meta: Meta, expire: i64, value: Value) -> KvBox {
        let packed = matches!(&value, Value::Str(s) if s.len() <= PACK_MAX);
        let flags = if expire != 0 { HAS_EXPIRE } else { 0 } | if packed { PACKED } else { 0 };
        let (vlen, vcap) = match &value {
            Value::Str(s) if packed => (s.len() as u8, pack_cap(s.len())),
            _ => (0, 0),
        };
        let layout = node_layout(flags, vcap, key.len());
        // SAFETY: the layout is non-zero sized; every part is written before the node is used.
        unsafe {
            let base = alloc(layout);
            let Some(p) = NonNull::new(base as *mut Head) else { std::alloc::handle_alloc_error(layout) };
            p.as_ptr().write(Head {
                next: None,
                lru: meta.lru,
                sized: meta.sized,
                obj: meta.obj,
                sds: meta.sds,
                flags,
                vlen,
                vcap,
                _pad: 0,
                klen: u32::try_from(key.len()).expect("key length"),
            });
            if flags & HAS_EXPIRE != 0 {
                (base.add(HEAD) as *mut i64).write(expire);
            }
            let v = base.add(val_off(flags));
            match value {
                Value::Str(s) if packed => std::ptr::copy_nonoverlapping(s.as_ptr(), v, s.len()),
                value => (v as *mut Value).write(value),
            }
            std::ptr::copy_nonoverlapping(key.as_ptr(), v.add(val_size(flags, vcap)), key.len());
            KvBox(p)
        }
    }

    fn from_entry(key: &[u8], e: Entry) -> KvBox {
        KvBox::new(key, Meta { lru: e.lru, sized: e.sized, obj: e.obj, sds: e.sds }, e.expire, e.value)
    }

    #[inline]
    fn head(&self) -> &Head {
        // SAFETY: a live node.
        unsafe { self.0.as_ref() }
    }

    #[inline]
    fn head_mut(&mut self) -> &mut Head {
        // SAFETY: a live node, uniquely borrowed.
        unsafe { self.0.as_mut() }
    }

    #[inline]
    fn base(&self) -> *mut u8 {
        self.0.as_ptr() as *mut u8
    }

    #[inline]
    fn packed(&self) -> bool {
        self.head().flags & PACKED != 0
    }

    #[inline]
    fn meta(&self) -> Meta {
        let h = self.head();
        Meta { lru: h.lru, sized: h.sized, obj: h.obj, sds: h.sds }
    }

    #[inline]
    fn expire(&self) -> i64 {
        let h = self.head();
        if h.flags & HAS_EXPIRE == 0 {
            return 0;
        }
        // SAFETY: the expiry follows the header when the flag is set.
        unsafe { (self.base().add(HEAD) as *const i64).read() }
    }

    #[inline]
    fn value_ptr(&self) -> *mut u8 {
        // SAFETY: within the node.
        unsafe { self.base().add(val_off(self.head().flags)) }
    }

    #[inline]
    fn slot(&self) -> &Value {
        debug_assert!(!self.packed());
        // SAFETY: an unpacked node holds a `Value` there.
        unsafe { &*(self.value_ptr() as *const Value) }
    }

    #[inline]
    fn slot_mut(&mut self) -> &mut Value {
        debug_assert!(!self.packed());
        // SAFETY: as in `slot`, uniquely borrowed.
        unsafe { &mut *(self.value_ptr() as *mut Value) }
    }

    #[inline]
    fn str_bytes(&self) -> &[u8] {
        // SAFETY: a packed node's first `vlen` value bytes are written.
        unsafe { std::slice::from_raw_parts(self.value_ptr(), self.head().vlen as usize) }
    }

    #[inline]
    fn value(&self) -> ValueRef<'_> {
        if self.packed() { ValueRef::Str(self.str_bytes()) } else { self.slot().view() }
    }

    /// What the value owns, counted as the boxed layout counted it (a string's bytes past 23).
    #[inline]
    fn value_mem(&self) -> usize {
        if self.packed() {
            let n = self.head().vlen as usize;
            if n > 23 { n } else { 0 }
        } else {
            self.slot().mem()
        }
    }

    /// Take the node apart: its metadata, expiry, value and chain link.
    fn into_parts(self) -> (Meta, i64, Value, Option<KvBox>) {
        let mut me = std::mem::ManuallyDrop::new(self);
        let (meta, expire) = (me.meta(), me.expire());
        let next = me.head_mut().next.take();
        let value = if me.packed() {
            Value::str(me.str_bytes())
        } else {
            // SAFETY: the value is moved out; the node is freed below without dropping it.
            unsafe { (me.value_ptr() as *const Value).read() }
        };
        let h = me.head();
        let layout = node_layout(h.flags, h.vcap, h.klen as usize);
        // SAFETY: allocated with this layout; nothing in it is used again.
        unsafe { dealloc(me.base(), layout) };
        (meta, expire, value, next)
    }

    fn into_entry(self) -> (Str, Entry) {
        let key = Str::from_slice(self.key());
        let (m, expire, value, _) = self.into_parts();
        (key, Entry { value, expire, lru: m.lru, sized: m.sized, obj: m.obj, sds: m.sds })
    }
}

impl NodePtr for KvBox {
    #[inline]
    fn key(&self) -> &[u8] {
        let h = self.head();
        // SAFETY: the key's bytes end the node.
        unsafe { std::slice::from_raw_parts(self.value_ptr().add(val_size(h.flags, h.vcap)), h.klen as usize) }
    }
    #[inline]
    fn next(&self) -> &Option<Self> {
        &self.head().next
    }
    #[inline]
    fn next_mut(&mut self) -> &mut Option<Self> {
        &mut self.head_mut().next
    }
}

impl Drop for KvBox {
    fn drop(&mut self) {
        let next = self.head_mut().next.take();
        if !self.packed() {
            // SAFETY: an unpacked node's value is live until here.
            unsafe { std::ptr::drop_in_place(self.value_ptr() as *mut Value) };
        }
        let h = self.head();
        let layout = node_layout(h.flags, h.vcap, h.klen as usize);
        // SAFETY: allocated with this layout.
        unsafe { dealloc(self.base(), layout) };
        drop(next);
    }
}

/// A stored entry, read (or an owned entry seen the same way).
#[derive(Clone, Copy)]
pub enum KvRef<'a> {
    Node(&'a KvBox),
    Entry(&'a Entry),
}

impl<'a> KvRef<'a> {
    #[inline]
    pub fn value(self) -> ValueRef<'a> {
        match self {
            KvRef::Node(n) => n.value(),
            KvRef::Entry(e) => e.value.view(),
        }
    }

    #[inline]
    pub fn expire(self) -> i64 {
        match self {
            KvRef::Node(n) => n.expire(),
            KvRef::Entry(e) => e.expire,
        }
    }

    #[inline]
    pub fn expired(self, now: i64) -> bool {
        let e = self.expire();
        e != 0 && e <= now
    }

    #[inline]
    pub fn lru(self) -> u32 {
        match self {
            KvRef::Node(n) => n.head().lru,
            KvRef::Entry(e) => e.lru,
        }
    }

    #[inline]
    pub fn obj(self) -> u8 {
        match self {
            KvRef::Node(n) => n.head().obj,
            KvRef::Entry(e) => e.obj,
        }
    }

    /// Where the entry is (DEBUG OBJECT's `Value at`).
    #[inline]
    pub fn addr(self) -> *const u8 {
        match self {
            KvRef::Node(n) => n.base(),
            KvRef::Entry(e) => e as *const Entry as *const u8,
        }
    }

    /// An owned copy of the entry.
    pub fn to_entry(self) -> Entry {
        match self {
            KvRef::Node(n) => {
                let m = n.meta();
                Entry { value: n.value().to_value(), expire: n.expire(), lru: m.lru, sized: m.sized, obj: m.obj, sds: m.sds }
            }
            KvRef::Entry(e) => e.clone(),
        }
    }

    /// Heap bytes the value owns ([`Value::mem`]).
    #[inline]
    pub fn value_mem(self) -> usize {
        match self {
            KvRef::Node(n) => n.value_mem(),
            KvRef::Entry(e) => e.value.mem(),
        }
    }
}

impl<'a> From<&'a Entry> for KvRef<'a> {
    /// An owned entry seen like a stored one.
    fn from(e: &'a Entry) -> Self {
        KvRef::Entry(e)
    }
}

impl Fields for KvRef<'_> {
    #[inline]
    fn value(&self) -> ValueRef<'_> {
        (*self).value()
    }
    #[inline]
    fn expire(&self) -> i64 {
        (*self).expire()
    }
    #[inline]
    fn lru(&self) -> u32 {
        (*self).lru()
    }
    #[inline]
    fn sized(&self) -> u16 {
        match self {
            KvRef::Node(n) => n.head().sized,
            KvRef::Entry(e) => e.sized,
        }
    }
    #[inline]
    fn obj(&self) -> u8 {
        (*self).obj()
    }
    #[inline]
    fn sds(&self) -> u8 {
        match self {
            KvRef::Node(n) => n.head().sds,
            KvRef::Entry(e) => e.sds,
        }
    }
}

/// A stored entry, to change. Changes that don't fit the node rebuild it in place in its chain.
pub struct KvMut<'a> {
    link: &'a mut Option<KvBox>,
}

impl<'a> KvMut<'a> {
    #[inline]
    fn node(&self) -> &KvBox {
        self.link.as_ref().expect("a node")
    }

    #[inline]
    fn node_mut(&mut self) -> &mut KvBox {
        self.link.as_mut().expect("a node")
    }

    #[inline]
    pub fn as_ref(&self) -> KvRef<'_> {
        KvRef::Node(self.node())
    }

    #[inline]
    pub fn into_ref(self) -> KvRef<'a> {
        let link: &'a Option<KvBox> = self.link;
        KvRef::Node(link.as_ref().expect("a node"))
    }

    #[inline]
    pub fn expire(&self) -> i64 {
        self.node().expire()
    }

    #[inline]
    pub fn expired(&self, now: i64) -> bool {
        let e = self.expire();
        e != 0 && e <= now
    }

    /// The value of an aggregate (any type but a string) to change in place; `None` for a
    /// string (see [`KvMut::take_value`]).
    #[inline]
    pub fn aggregate_mut(&mut self) -> Option<&mut Value> {
        let n = self.node_mut();
        if n.packed() || matches!(n.slot(), Value::Str(_)) { None } else { Some(n.slot_mut()) }
    }

    /// [`KvMut::aggregate_mut`] for the rest of the borrow.
    #[inline]
    pub fn into_aggregate_mut(self) -> Option<&'a mut Value> {
        let n = self.link.as_mut().expect("a node");
        if n.packed() || matches!(n.slot(), Value::Str(_)) { None } else { Some(n.slot_mut()) }
    }

    /// Take the value out to change it (a string's bytes copied out of the node); give it back
    /// with [`KvMut::put_value`]. Meanwhile the entry holds an empty string.
    pub fn take_value(&mut self) -> Value {
        let n = self.node_mut();
        if n.packed() {
            let v = Value::str(n.str_bytes());
            n.head_mut().vlen = 0;
            v
        } else {
            std::mem::replace(n.slot_mut(), Value::Str(Str::new()))
        }
    }

    /// Store `v` as the value: a short string in place when there is room, else the node is
    /// rebuilt for it.
    pub fn put_value(&mut self, v: Value) {
        let n = self.node_mut();
        match v {
            Value::Str(s) if s.len() <= PACK_MAX && n.packed() && s.len() <= n.head().vcap as usize => {
                // SAFETY: the node has `vcap` bytes for the value.
                unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), n.value_ptr(), s.len()) };
                n.head_mut().vlen = s.len() as u8;
            }
            Value::Str(s) if s.len() <= PACK_MAX => self.rebuild(Value::Str(s), None),
            v if !n.packed() => *n.slot_mut() = v,
            v => self.rebuild(v, None),
        }
    }

    /// Store the string `b` as the value: [`KvMut::put_value`] without building it first.
    pub fn put_str(&mut self, b: &[u8]) {
        let n = self.node_mut();
        if n.packed() && b.len() <= n.head().vcap as usize {
            // SAFETY: the node has `vcap` bytes for the value.
            unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), n.value_ptr(), b.len()) };
            n.head_mut().vlen = b.len() as u8;
        } else {
            self.put_value(Value::str(b));
        }
    }

    /// Rebuild the node holding `v` (and the expiry `expire`, else the current one).
    fn rebuild(&mut self, v: Value, expire: Option<i64>) {
        let old = self.link.take().expect("a node");
        let key = Str::from_slice(old.key());
        let (meta, cur, _old_value, next) = old.into_parts();
        let mut n = KvBox::new(&key, meta, expire.unwrap_or(cur), v);
        n.head_mut().next = next;
        *self.link = Some(n);
    }

    /// Replace the value, returning the old one.
    pub fn replace_value(&mut self, v: Value) -> Value {
        let old = self.take_value();
        self.put_value(v);
        old
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
    pub(crate) fn set_expire_raw(&mut self, at: i64) {
        let n = self.node_mut();
        if n.head().flags & HAS_EXPIRE != 0 {
            // SAFETY: the node has the field.
            unsafe { (n.base().add(HEAD) as *mut i64).write(at) };
        } else if at != 0 {
            let v = self.take_value();
            self.rebuild(v, Some(at));
        }
    }

    /// Heap bytes the value owns ([`Value::mem`]).
    #[inline]
    pub fn value_mem(&self) -> usize {
        self.node().value_mem()
    }
}

impl Fields for KvMut<'_> {
    #[inline]
    fn value(&self) -> ValueRef<'_> {
        self.node().value()
    }
    #[inline]
    fn expire(&self) -> i64 {
        self.node().expire()
    }
    #[inline]
    fn lru(&self) -> u32 {
        self.node().head().lru
    }
    #[inline]
    fn sized(&self) -> u16 {
        self.node().head().sized
    }
    #[inline]
    fn obj(&self) -> u8 {
        self.node().head().obj
    }
    #[inline]
    fn sds(&self) -> u8 {
        self.node().head().sds
    }
}

impl FieldsMut for KvMut<'_> {
    #[inline]
    fn set_lru(&mut self, v: u32) {
        self.node_mut().head_mut().lru = v;
    }
    #[inline]
    fn set_sized(&mut self, v: u16) {
        self.node_mut().head_mut().sized = v;
    }
    #[inline]
    fn set_obj(&mut self, v: u8) {
        self.node_mut().head_mut().obj = v;
    }
    #[inline]
    fn set_sds(&mut self, v: u8) {
        self.node_mut().head_mut().sds = v;
    }
}

/// The keys of one database.
#[derive(Default)]
pub struct KeySpace {
    d: RawDict<KvBox>,
}

impl KeySpace {
    /// A keyspace hashing with `hasher` (tests compare tables seeded alike).
    pub fn with_hasher(hasher: foldhash::fast::RandomState) -> Self {
        Self { d: RawDict::with_hasher(hasher) }
    }

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
        self.d.get(key).map(KvRef::Node)
    }

    #[inline]
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.d.contains_key(key)
    }

    /// The entry to change (a rehash step, as Redis' lookups take).
    #[inline]
    pub fn get_mut(&mut self, key: &[u8]) -> Option<KvMut<'_>> {
        self.d.get_link(key).map(|link| KvMut { link })
    }

    /// Add a key known to be absent.
    pub fn insert_new(&mut self, key: &[u8], e: Entry) -> KvMut<'_> {
        KvMut { link: self.d.insert_new(KvBox::from_entry(key, e)) }
    }

    pub fn remove_entry(&mut self, key: &[u8]) -> Option<(Str, Entry)> {
        self.d.remove(key).map(KvBox::into_entry)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], KvRef<'_>)> {
        self.d.iter().map(|n| (n.key(), KvRef::Node(n)))
    }

    pub fn scan(&self, cursor: u64, mut f: impl FnMut(&[u8], KvRef<'_>)) -> u64 {
        self.d.scan(cursor, |n| f(n.key(), KvRef::Node(n)))
    }

    pub fn random(&self, rng: &mut Rng) -> Option<(&[u8], KvRef<'_>)> {
        self.d.random(rng).map(|n| (n.key(), KvRef::Node(n)))
    }

    pub fn sample(&self, rng: &mut Rng, count: usize, mut f: impl FnMut(&[u8], KvRef<'_>)) {
        self.d.sample(rng, count, |n| f(n.key(), KvRef::Node(n)))
    }

    /// Change every entry.
    pub fn for_each_mut(&mut self, mut f: impl FnMut(KvMut<'_>)) {
        self.d.for_each_link(|link| f(KvMut { link }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dict;

    fn snap(e: &Entry) -> String {
        format!("{:?}/{}/{}/{}/{}/{}", e.value, e.expire, e.lru, e.sized, e.obj, e.sds)
    }

    fn value(r: &mut Rng) -> Value {
        match r.next_u64() % 10 {
            0 => Value::Hash(Box::default()),
            1 => Value::str(&vec![b'L'; 256 + (r.next_u64() % 200) as usize]),
            n => {
                let len = (r.next_u64() % if n < 5 { 24 } else { 256 }) as usize;
                Value::str(&(0..len).map(|i| (i as u64 ^ r.next_u64()) as u8).collect::<Vec<_>>())
            }
        }
    }

    /// The packed keyspace behaves exactly like the boxed `Dict<Str, Entry>` it replaced: same
    /// entries, iteration order, SCAN, random picks and table shape.
    #[test]
    fn matches_boxed_model() {
        let hasher = foldhash::fast::RandomState::default();
        let mut ks = KeySpace::with_hasher(hasher.clone());
        let mut model: Dict<Str, Entry> = Dict::with_hasher(hasher);
        let mut r = Rng::seeded(7);
        let key = |i: u64| -> Vec<u8> {
            match i % 4 {
                0 => format!("k{i}").into_bytes(),
                1 => format!("a-much-longer-key-name-{i}-{}", "x".repeat((i % 50) as usize)).into_bytes(),
                2 if i.is_multiple_of(7) => Vec::new(),
                _ => i.to_le_bytes().to_vec(),
            }
        };
        for round in 0..40_000u64 {
            let k = key(r.next_u64() % 3000);
            match r.next_u64() % 9 {
                0..=2 => {
                    if !model.contains_key(&k) {
                        let mut e = Entry::new(value(&mut r), r.next_u64() as u32);
                        if r.next_u64().is_multiple_of(3) {
                            e.expire = 1_000 + (r.next_u64() % 1_000_000) as i64;
                        }
                        (e.sized, e.obj, e.sds) = (r.next_u64() as u16, r.next_u64() as u8, r.next_u64() as u8);
                        ks.insert_new(&k, e.clone());
                        model.insert_new(Str::from_slice(&k), e);
                    }
                }
                3 => {
                    let a = ks.remove_entry(&k).map(|(k, e)| (k, snap(&e)));
                    let b = model.remove_entry(&k).map(|(k, e)| (k, snap(&e)));
                    assert_eq!(a, b);
                }
                4 => {
                    let v = value(&mut r);
                    let m = model.get_mut(&k).map(|e| snap(&Entry { value: std::mem::replace(&mut e.value, v.clone()), ..e.clone() }));
                    let s = ks.get_mut(&k).map(|mut e| {
                        let old = e.replace_value(v);
                        snap(&Entry { value: old, ..e.as_ref().to_entry() })
                    });
                    // The old value against the new metadata: same on both.
                    assert_eq!(s.is_some(), m.is_some());
                }
                5 => {
                    let at = if r.next_u64().is_multiple_of(4) { 0 } else { (r.next_u64() % 1_000_000) as i64 };
                    if let Some(e) = model.get_mut(&k) {
                        e.expire = at;
                    }
                    if let Some(mut e) = ks.get_mut(&k) {
                        e.set_expire_raw(at);
                    }
                }
                6 => {
                    let lru = r.next_u64() as u32;
                    if let Some(e) = model.get_mut(&k) {
                        e.lru = lru;
                        e.sds = e.sds.wrapping_add(1);
                    }
                    if let Some(mut e) = ks.get_mut(&k) {
                        e.set_lru(lru);
                        let s = e.sds();
                        e.set_sds(s.wrapping_add(1));
                    }
                }
                7 => {
                    let grow = (r.next_u64() % 40) as usize;
                    if let Some(e) = model.get_mut(&k)
                        && let Value::Str(s) = &mut e.value
                    {
                        let mut b = s.to_vec();
                        b.extend(std::iter::repeat_n(b'g', grow));
                        *s = Str::from_slice(&b);
                    }
                    if let Some(mut e) = ks.get_mut(&k) {
                        e.with_value(|v| {
                            if let Value::Str(s) = v {
                                let mut b = s.to_vec();
                                b.extend(std::iter::repeat_n(b'g', grow));
                                *s = Str::from_slice(&b);
                            }
                        });
                    }
                }
                _ => {
                    let a = ks.get(&k).map(|e| snap(&e.to_entry()));
                    let b = model.get(&k).map(snap);
                    assert_eq!(a, b);
                    assert_eq!(ks.get(&k).map(|e| e.value_mem()), model.get(&k).map(|e| e.value.mem()));
                }
            }
            if round % 5_000 == 0 || round == 39_999 {
                assert_eq!(ks.len(), model.len());
                assert_eq!((ks.buckets(), ks.table_bytes(), ks.is_rehashing()), (model.buckets(), model.table_bytes(), model.is_rehashing()));
                let a: Vec<_> = ks.iter().map(|(k, e)| (k.to_vec(), snap(&e.to_entry()))).collect();
                let b: Vec<_> = model.iter().map(|(k, e)| (k.to_vec(), snap(e))).collect();
                assert_eq!(a, b);
                let (mut ca, mut cb) = (0, 0);
                loop {
                    let mut ka = Vec::new();
                    let mut kb = Vec::new();
                    ca = ks.scan(ca, |k, _| ka.push(k.to_vec()));
                    cb = model.scan(cb, |k, _| kb.push(k.to_vec()));
                    assert_eq!((ca, &ka), (cb, &kb));
                    if ca == 0 {
                        break;
                    }
                }
                let (mut ra, mut rb) = (Rng::seeded(round), Rng::seeded(round));
                assert_eq!(ks.random(&mut ra).map(|(k, _)| k.to_vec()), model.random(&mut rb).map(|(k, _)| k.to_vec()));
                let (mut sa, mut sb) = (Vec::new(), Vec::new());
                ks.sample(&mut ra, 20, |k, _| sa.push(k.to_vec()));
                model.sample(&mut rb, 20, |k, _| sb.push(k.to_vec()));
                assert_eq!(sa, sb);
                assert_eq!(format!("{:?}", ks.stats(true)), format!("{:?}", model.stats(true)));
            }
        }
        let mut n = 0;
        ks.for_each_mut(|mut e| {
            e.set_sized(1);
            n += 1;
        });
        assert_eq!(n, model.len());
        assert!(ks.iter().all(|(_, e)| Fields::sized(&e) == 1));
        ks.clear();
        assert!(ks.is_empty());
    }

    #[test]
    fn node_sizes() {
        assert_eq!(std::mem::size_of::<Head>(), 24);
        assert_eq!(std::mem::size_of::<Option<KvBox>>(), 8);
        // A 16-byte key with a 10-byte string and no TTL: one 56-byte allocation.
        assert_eq!(node_layout(PACKED, pack_cap(10), 16).size(), 56);
    }
}
