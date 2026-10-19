//! The keyspace of one database: keys and their entries, reached through views ([`KvRef`],
//! [`KvMut`]) rather than references to stored structs, so entries can be laid out compactly
//! (Redis 8's `kvobj`: key, metadata and a small string value in one allocation).
//!
//! Each key is one allocation: a 12-byte header (LRU, keysizes slot, Redis' object bits,
//! lengths), the expiry when there is one, the key's bytes, then a string value of up to
//! [`PACK_MAX`] bytes in place (else a [`Value`]). The allocation is sized to the allocator's
//! class and the slack becomes room for the string to grow in place. A 16-byte key holding a
//! short string is one 32-byte allocation.
//!
//! Two of Redis' dicts index the nodes ([`PtrDict`]): all keys, and the keys with a TTL (as
//! Redis 8's `expires` holds the same kvobjs). Neither copies keys; a node that has to move
//! (its value outgrew it, it gained an expiry) is re-pointed in both.

use crate::dict::DictStats;
use crate::ptrdict::{Keyed, PtrDict};
use crate::util::Rng;
use crate::value::{Entry, Str, Value, ValueRef};
use std::alloc::{Layout, alloc, dealloc};
use std::marker::PhantomData;
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
/// The key's length is a `u32` after the expiry (keys of 255 bytes and more).
const LONG_KEY: u8 = 4;

/// A node's fixed part. After it, in the same allocation: the expiry (8 bytes, with
/// `HAS_EXPIRE`), a long key's length (4, with `LONG_KEY`), the key, then the value: a packed
/// string's `vcap` bytes, or a [`Value`] at the next 8-byte boundary.
#[repr(C)]
pub struct Head {
    lru: u32,
    sized: u16,
    obj: u8,
    sds: u8,
    flags: u8,
    /// A packed string's length and the room for it.
    vlen: u8,
    vcap: u8,
    /// The key's length, unless `LONG_KEY`.
    klen: u8,
}

const HEAD: usize = std::mem::size_of::<Head>();
const SLOT: usize = std::mem::size_of::<Value>();
const _: () = assert!(HEAD == 12 && SLOT == 24 && std::mem::align_of::<Value>() <= 8);

/// An entry's metadata, carried over when its node is rebuilt.
#[derive(Clone, Copy)]
struct Meta {
    lru: u32,
    sized: u16,
    obj: u8,
    sds: u8,
}

/// Offsets of a node's parts.
#[derive(Clone, Copy)]
struct Shape {
    key: usize,
    val: usize,
    size: usize,
}

#[inline]
fn key_off(flags: u8) -> usize {
    HEAD + if flags & HAS_EXPIRE != 0 { 8 } else { 0 } + if flags & LONG_KEY != 0 { 4 } else { 0 }
}

#[inline]
fn shape(flags: u8, klen: usize, vcap: u8) -> Shape {
    let key = key_off(flags);
    let end = key + klen;
    if flags & PACKED != 0 {
        Shape { key, val: end, size: end + vcap as usize }
    } else {
        let val = end.next_multiple_of(8);
        Shape { key, val, size: val + SLOT }
    }
}

/// The allocator's size class for `n` bytes (mimalloc's and jemalloc's small bins: 8-byte steps
/// to 64, then four per doubling), so a packed string can use what would be slack.
#[inline]
fn size_class(n: usize) -> usize {
    if n <= 64 {
        return n.next_multiple_of(8);
    }
    let p = usize::BITS - 1 - (n - 1).leading_zeros();
    n.next_multiple_of((1usize << p) / 4)
}

#[inline]
fn layout(size: usize) -> Layout {
    Layout::from_size_align(size, 8).expect("node layout")
}

/// A node, read.
#[derive(Clone, Copy)]
pub struct NodeRef<'a> {
    p: NonNull<Head>,
    _l: PhantomData<&'a Head>,
}

impl<'a> NodeRef<'a> {
    #[inline]
    fn new(p: NonNull<Head>) -> Self {
        Self { p, _l: PhantomData }
    }

    #[inline]
    fn at(l: usize) -> Self {
        // SAFETY: slots hold live node pointers.
        Self::new(unsafe { NonNull::new_unchecked(l as *mut Head) })
    }

    #[inline]
    fn head(self) -> &'a Head {
        // SAFETY: a live node.
        unsafe { self.p.as_ref() }
    }

    #[inline]
    fn base(self) -> *mut u8 {
        self.p.as_ptr() as *mut u8
    }

    #[inline]
    fn packed(self) -> bool {
        self.head().flags & PACKED != 0
    }

    #[inline]
    fn klen(self) -> usize {
        let h = self.head();
        if h.flags & LONG_KEY != 0 {
            // SAFETY: the length precedes the key with `LONG_KEY`.
            unsafe { (self.base().add(key_off(h.flags) - 4) as *const u32).read_unaligned() as usize }
        } else {
            h.klen as usize
        }
    }

    #[inline]
    fn shape(self) -> Shape {
        let h = self.head();
        shape(h.flags, self.klen(), h.vcap)
    }

    #[inline]
    pub fn key(self) -> &'a [u8] {
        let off = key_off(self.head().flags);
        // SAFETY: the key's bytes are written at creation.
        unsafe { std::slice::from_raw_parts(self.base().add(off), self.klen()) }
    }

    #[inline]
    fn meta(self) -> Meta {
        let h = self.head();
        Meta { lru: h.lru, sized: h.sized, obj: h.obj, sds: h.sds }
    }

    #[inline]
    fn has_expire_field(self) -> bool {
        self.head().flags & HAS_EXPIRE != 0
    }

    #[inline]
    fn expire(self) -> i64 {
        if !self.has_expire_field() {
            return 0;
        }
        // SAFETY: the expiry follows the header with the flag.
        unsafe { (self.base().add(HEAD) as *const i64).read_unaligned() }
    }

    #[inline]
    fn value_ptr(self) -> *mut u8 {
        // SAFETY: within the node.
        unsafe { self.base().add(self.shape().val) }
    }

    #[inline]
    fn slot(self) -> &'a Value {
        debug_assert!(!self.packed());
        // SAFETY: an unpacked node holds an aligned `Value` there.
        unsafe { &*(self.value_ptr() as *const Value) }
    }

    #[inline]
    fn str_bytes(self) -> &'a [u8] {
        // SAFETY: a packed node's first `vlen` value bytes are written.
        unsafe { std::slice::from_raw_parts(self.value_ptr(), self.head().vlen as usize) }
    }

    #[inline]
    fn value(self) -> ValueRef<'a> {
        if self.packed() { ValueRef::Str(self.str_bytes()) } else { self.slot().view() }
    }

    /// What the value owns, counted as a boxed value would be (a string's bytes past 23).
    #[inline]
    fn value_mem(self) -> usize {
        if self.packed() {
            let n = self.head().vlen as usize;
            if n > 23 { n } else { 0 }
        } else {
            self.slot().mem()
        }
    }
}

// SAFETY: `NodeRef::key` reads a live node's key, fixed for the node's life.
unsafe impl Keyed for Head {
    #[inline]
    unsafe fn key<'a>(p: NonNull<Self>) -> &'a [u8] {
        NodeRef::new(p).key()
    }
}

/// A new node holding `value` (a string of up to [`PACK_MAX`] bytes in place).
fn node_new(key: &[u8], meta: Meta, expire: i64, value: Value) -> NonNull<Head> {
    let packed = matches!(&value, Value::Str(s) if s.len() <= PACK_MAX);
    let long = key.len() >= u8::MAX as usize;
    let flags = if expire != 0 { HAS_EXPIRE } else { 0 } | if packed { PACKED } else { 0 } | if long { LONG_KEY } else { 0 };
    let (vlen, vcap) = match &value {
        Value::Str(s) if packed => {
            let end = key_off(flags) + key.len();
            (s.len() as u8, (size_class(end + s.len()) - end).min(PACK_MAX) as u8)
        }
        _ => (0, 0),
    };
    let sh = shape(flags, key.len(), vcap);
    let layout = layout(sh.size);
    // SAFETY: the layout is non-zero sized; every part is written before the node is used.
    unsafe {
        let base = alloc(layout);
        let Some(p) = NonNull::new(base as *mut Head) else { std::alloc::handle_alloc_error(layout) };
        p.as_ptr().write(Head {
            lru: meta.lru,
            sized: meta.sized,
            obj: meta.obj,
            sds: meta.sds,
            flags,
            vlen,
            vcap,
            klen: if long { u8::MAX } else { key.len() as u8 },
        });
        if flags & HAS_EXPIRE != 0 {
            (base.add(HEAD) as *mut i64).write_unaligned(expire);
        }
        if long {
            let n = u32::try_from(key.len()).expect("key length");
            (base.add(sh.key - 4) as *mut u32).write_unaligned(n);
        }
        std::ptr::copy_nonoverlapping(key.as_ptr(), base.add(sh.key), key.len());
        let v = base.add(sh.val);
        match value {
            Value::Str(s) if packed => std::ptr::copy_nonoverlapping(s.as_ptr(), v, s.len()),
            value => (v as *mut Value).write(value),
        }
        p
    }
}

fn node_from_entry(key: &[u8], e: Entry) -> NonNull<Head> {
    node_new(key, Meta { lru: e.lru, sized: e.sized, obj: e.obj, sds: e.sds }, e.expire, e.value)
}

/// Free a node and its value.
///
/// # Safety
/// `p` is a live node no table points at any more.
unsafe fn node_free(p: NonNull<Head>) {
    let n = NodeRef::new(p);
    let size = n.shape().size;
    if !n.packed() {
        // SAFETY: an unpacked node's value is live until here.
        unsafe { std::ptr::drop_in_place(n.value_ptr() as *mut Value) };
    }
    // SAFETY: allocated with this layout.
    unsafe { dealloc(n.base(), layout(size)) };
}

/// Take a node apart into an owned key and entry.
///
/// # Safety
/// As for [`node_free`].
unsafe fn node_into_entry(p: NonNull<Head>) -> (Str, Entry) {
    let n = NodeRef::new(p);
    let key = Str::from_slice(n.key());
    let (m, expire, size) = (n.meta(), n.expire(), n.shape().size);
    let value = if n.packed() {
        Value::str(n.str_bytes())
    } else {
        // SAFETY: the value is moved out; the node is freed below without dropping it.
        unsafe { (n.value_ptr() as *const Value).read() }
    };
    // SAFETY: allocated with this layout; nothing in it is used again.
    unsafe { dealloc(n.base(), layout(size)) };
    (key, Entry { value, expire, lru: m.lru, sized: m.sized, obj: m.obj, sds: m.sds })
}

/// A stored entry, read (or an owned entry seen the same way).
#[derive(Clone, Copy)]
pub enum KvRef<'a> {
    Node(NodeRef<'a>),
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

/// A stored entry, to change. A change that doesn't fit the node moves it to a new one, put in
/// its place in both tables.
pub struct KvMut<'a> {
    /// The keyspace link holding the node.
    slot: &'a mut usize,
    /// The TTL index.
    x: &'a mut PtrDict<Head>,
}

impl<'a> KvMut<'a> {
    #[inline]
    fn node(&self) -> NodeRef<'_> {
        NodeRef::at(*self.slot)
    }

    #[inline]
    fn head_mut(&mut self) -> &mut Head {
        // SAFETY: a live node, uniquely borrowed through its slot.
        unsafe { &mut *(*self.slot as *mut Head) }
    }

    #[inline]
    pub fn as_ref(&self) -> KvRef<'_> {
        KvRef::Node(self.node())
    }

    #[inline]
    pub fn into_ref(self) -> KvRef<'a> {
        KvRef::Node(NodeRef::at(*self.slot))
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

    /// The aggregate value in an unpacked node (`None` for strings).
    #[inline]
    fn aggregate_ptr(&self) -> Option<*mut Value> {
        let n = self.node();
        if n.packed() || matches!(n.slot(), Value::Str(_)) { None } else { Some(n.value_ptr() as *mut Value) }
    }

    /// The value of an aggregate (any type but a string) to change in place; `None` for a
    /// string (see [`KvMut::take_value`]).
    #[inline]
    pub fn aggregate_mut(&mut self) -> Option<&mut Value> {
        // SAFETY: the node's value, uniquely borrowed through its slot.
        self.aggregate_ptr().map(|p| unsafe { &mut *p })
    }

    /// [`KvMut::aggregate_mut`] for the rest of the borrow.
    #[inline]
    pub fn into_aggregate_mut(self) -> Option<&'a mut Value> {
        // SAFETY: as above, for the borrow of the slot.
        self.aggregate_ptr().map(|p| unsafe { &mut *p })
    }

    /// Take the value out to change it (a string's bytes copied out of the node); give it back
    /// with [`KvMut::put_value`]. Meanwhile the entry holds an empty string.
    pub fn take_value(&mut self) -> Value {
        let n = self.node();
        if n.packed() {
            let v = Value::str(n.str_bytes());
            self.head_mut().vlen = 0;
            v
        } else {
            // SAFETY: the node's value, uniquely borrowed through its slot.
            let slot = unsafe { &mut *(n.value_ptr() as *mut Value) };
            std::mem::replace(slot, Value::Str(Str::new()))
        }
    }

    /// Store `v` as the value: a short string in place when there is room, else the node is
    /// rebuilt for it.
    pub fn put_value(&mut self, v: Value) {
        match v {
            Value::Str(s) if s.len() <= PACK_MAX => self.put_str(&s),
            v if !self.node().packed() => {
                // SAFETY: as in `take_value`.
                unsafe { *(self.node().value_ptr() as *mut Value) = v };
            }
            v => self.rebuild(v, None),
        }
    }

    /// Store the string `b` as the value: [`KvMut::put_value`] without building it first.
    pub fn put_str(&mut self, b: &[u8]) {
        let n = self.node();
        if n.packed() && b.len() <= n.head().vcap as usize {
            // SAFETY: the node has `vcap` bytes for the value.
            unsafe { std::ptr::copy_nonoverlapping(b.as_ptr(), n.value_ptr(), b.len()) };
            self.head_mut().vlen = b.len() as u8;
        } else {
            self.rebuild(Value::str(b), None);
        }
    }

    /// Move the entry to a new node holding `v` (and the expiry `expire`, else the current
    /// one), keeping both tables pointing at it.
    fn rebuild(&mut self, v: Value, expire: Option<i64>) {
        let old = NodeRef::at(*self.slot);
        let cur = old.expire();
        let at = expire.unwrap_or(cur);
        let new = node_new(old.key(), old.meta(), at, v);
        let key = NodeRef::new(new).key();
        match (cur != 0, at != 0) {
            (true, true) => self.x.repoint(key, old.p, new),
            (true, false) => self.x.remove_ptr(key, old.p),
            (false, true) => {
                self.x.insert_new(new);
            }
            (false, false) => {}
        }
        *self.slot = new.as_ptr() as usize;
        // SAFETY: nothing points at the old node any more.
        unsafe { node_free(old.p) };
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

    /// Set or clear (0) the expiry, keeping the TTL index ([`crate::db::Db::set_expire`] keeps
    /// the accounting).
    pub(crate) fn set_expire_raw(&mut self, at: i64) {
        let n = NodeRef::at(*self.slot);
        let cur = n.expire();
        if n.has_expire_field() {
            // SAFETY: the node has the field.
            unsafe { (n.base().add(HEAD) as *mut i64).write_unaligned(at) };
            match (cur != 0, at != 0) {
                (false, true) => {
                    self.x.insert_new(n.p);
                }
                (true, false) => self.x.remove_ptr(n.key(), n.p),
                _ => {}
            }
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
        self.head_mut().lru = v;
    }
    #[inline]
    fn set_sized(&mut self, v: u16) {
        self.head_mut().sized = v;
    }
    #[inline]
    fn set_obj(&mut self, v: u8) {
        self.head_mut().obj = v;
    }
    #[inline]
    fn set_sds(&mut self, v: u8) {
        self.head_mut().sds = v;
    }
}

/// Keys prefetched at a time ([`KeySpace::prefetch`]).
pub const PREFETCH_BATCH: usize = 16;

/// The keys of one database, and the index of those with a TTL.
#[derive(Default)]
pub struct KeySpace {
    /// Every key: owns the nodes.
    d: PtrDict<Head>,
    /// The keys whose expiry is set (Redis' `expires`): the same nodes.
    x: PtrDict<Head>,
}

#[inline]
fn kv<'a>(p: NonNull<Head>) -> (&'a [u8], KvRef<'a>) {
    let n = NodeRef::new(p);
    (n.key(), KvRef::Node(n))
}

impl KeySpace {
    /// A keyspace hashing with `hasher` (tests compare tables seeded alike).
    pub fn with_hasher(hasher: foldhash::fast::RandomState) -> Self {
        Self { d: PtrDict::with_hasher(hasher.clone()), x: PtrDict::with_hasher(hasher) }
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
        for p in self.d.iter() {
            // SAFETY: each node once; the tables are cleared next.
            unsafe { node_free(p) };
        }
        self.d.clear();
        self.x.clear();
    }

    #[inline]
    pub fn get(&self, key: &[u8]) -> Option<KvRef<'_>> {
        self.d.get(key).map(|p| KvRef::Node(NodeRef::new(p)))
    }

    #[inline]
    pub fn contains_key(&self, key: &[u8]) -> bool {
        self.d.get(key).is_some()
    }

    /// The entry to change (a rehash step, as Redis' lookups take).
    #[inline]
    pub fn get_mut(&mut self, key: &[u8]) -> Option<KvMut<'_>> {
        let KeySpace { d, x } = self;
        d.get_slot(key).map(|slot| KvMut { slot, x })
    }

    /// Add a key known to be absent.
    pub fn insert_new(&mut self, key: &[u8], e: Entry) -> KvMut<'_> {
        let ttl = e.expire != 0;
        let p = node_from_entry(key, e);
        let KeySpace { d, x } = self;
        let slot = d.insert_new(p);
        if ttl {
            x.insert_new(p);
        }
        KvMut { slot, x }
    }

    pub fn remove_entry(&mut self, key: &[u8]) -> Option<(Str, Entry)> {
        let p = self.d.remove(key)?;
        if NodeRef::new(p).expire() != 0 {
            self.x.remove_ptr(key, p);
        }
        // SAFETY: unlinked from both tables.
        Some(unsafe { node_into_entry(p) })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], KvRef<'_>)> {
        self.d.iter().map(kv)
    }

    pub fn scan(&self, cursor: u64, mut f: impl FnMut(&[u8], KvRef<'_>)) -> u64 {
        self.d.scan(cursor, |p| {
            let (k, e) = kv(p);
            f(k, e)
        })
    }

    pub fn random(&self, rng: &mut Rng) -> Option<(&[u8], KvRef<'_>)> {
        self.d.random(rng).map(kv)
    }

    pub fn sample(&self, rng: &mut Rng, count: usize, mut f: impl FnMut(&[u8], KvRef<'_>)) {
        self.d.sample(rng, count, |p| {
            let (k, e) = kv(p);
            f(k, e)
        })
    }

    /// Change every entry.
    pub fn for_each_mut(&mut self, mut f: impl FnMut(KvMut<'_>)) {
        let KeySpace { d, x } = self;
        d.for_each_slot(|slot| f(KvMut { slot, x: &mut *x }))
    }

    /// Start loading the buckets, then the nodes, of keys about to be looked up (a pipelined
    /// batch, like Valkey's command-queue prefetch): their cache misses overlap instead of
    /// following one another.
    pub fn prefetch<'k>(&self, keys: impl Iterator<Item = &'k [u8]>) {
        if self.d.is_empty() {
            return;
        }
        let mut hs = [0u64; PREFETCH_BATCH];
        let mut n = 0;
        for k in keys.take(PREFETCH_BATCH) {
            hs[n] = self.d.hash(k);
            self.d.prefetch_bucket(hs[n]);
            n += 1;
        }
        for &h in &hs[..n] {
            self.d.prefetch_entry(h);
        }
    }

    // --- the TTL index (Redis' `expires` dict) ---

    /// Keys with an expiry.
    #[inline]
    pub fn expires_len(&self) -> usize {
        self.x.len()
    }

    #[inline]
    pub fn expires_is_empty(&self) -> bool {
        self.x.is_empty()
    }

    #[inline]
    pub fn expires_is_rehashing(&self) -> bool {
        self.x.is_rehashing()
    }

    pub fn expires_stats(&self, full: bool) -> Vec<DictStats> {
        self.x.stats(full)
    }

    pub fn expires_expand(&mut self, n: usize) {
        self.x.expand(n)
    }

    pub fn expires_table_bytes(&self) -> usize {
        self.x.table_bytes()
    }

    pub fn expires_set_resize(&mut self, on: bool) {
        self.x.set_resize(on)
    }

    pub fn expires_rehash(&mut self, steps: usize) -> bool {
        self.x.rehash(steps)
    }

    pub fn expires_scan(&self, cursor: u64, mut f: impl FnMut(&[u8], KvRef<'_>)) -> u64 {
        self.x.scan(cursor, |p| {
            let (k, e) = kv(p);
            f(k, e)
        })
    }

    pub fn expires_random(&self, rng: &mut Rng) -> Option<(&[u8], KvRef<'_>)> {
        self.x.random(rng).map(kv)
    }

    pub fn expires_sample(&self, rng: &mut Rng, count: usize, mut f: impl FnMut(&[u8], KvRef<'_>)) {
        self.x.sample(rng, count, |p| {
            let (k, e) = kv(p);
            f(k, e)
        })
    }

    /// The TTL index's keys in its own order (tests).
    pub fn expires_iter(&self) -> impl Iterator<Item = &[u8]> {
        self.x.iter().map(|p| NodeRef::new(p).key())
    }
}

impl Drop for KeySpace {
    fn drop(&mut self) {
        self.clear();
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

    /// The boxed keyspace and TTL index the packed one replaced.
    struct Model {
        keys: Dict<Str, Entry>,
        vol: Dict<Str, ()>,
    }

    impl Model {
        fn set_expire(&mut self, k: &[u8], at: i64) {
            let Some(e) = self.keys.get_mut(k) else { return };
            let had = e.expire != 0;
            e.expire = at;
            match (had, at != 0) {
                (false, true) => {
                    self.vol.insert_new(Str::from_slice(k), ());
                }
                (true, false) => {
                    self.vol.remove(k);
                }
                _ => {}
            }
        }
    }

    /// The packed keyspace behaves exactly like the boxed `Dict<Str, Entry>` and TTL index it
    /// replaced: same entries, iteration order, SCAN, random picks and table shape, for both.
    #[test]
    fn matches_boxed_model() {
        let hasher = foldhash::fast::RandomState::default();
        let mut ks = KeySpace::with_hasher(hasher.clone());
        let mut m = Model { keys: Dict::with_hasher(hasher.clone()), vol: Dict::with_hasher(hasher) };
        let mut r = Rng::seeded(7);
        let key = |i: u64| -> Vec<u8> {
            match i % 5 {
                0 => format!("k{i}").into_bytes(),
                1 => format!("a-much-longer-key-name-{i}-{}", "x".repeat((i % 50) as usize)).into_bytes(),
                2 if i.is_multiple_of(7) => Vec::new(),
                3 => format!("{i}-{}", "L".repeat(250 + (i % 20) as usize)).into_bytes(),
                _ => i.to_le_bytes().to_vec(),
            }
        };
        for round in 0..60_000u64 {
            let k = key(r.next_u64() % 3000);
            match r.next_u64() % 9 {
                0..=2 => {
                    if !m.keys.contains_key(&k) {
                        let mut e = Entry::new(value(&mut r), r.next_u64() as u32);
                        if r.next_u64().is_multiple_of(3) {
                            e.expire = 1_000 + (r.next_u64() % 1_000_000) as i64;
                        }
                        (e.sized, e.obj, e.sds) = (r.next_u64() as u16, r.next_u64() as u8, r.next_u64() as u8);
                        ks.insert_new(&k, e.clone());
                        if e.expire != 0 {
                            m.vol.insert_new(Str::from_slice(&k), ());
                        }
                        m.keys.insert_new(Str::from_slice(&k), e);
                    }
                }
                3 => {
                    let a = ks.remove_entry(&k).map(|(k, e)| (k, snap(&e)));
                    let b = m.keys.remove_entry(&k).map(|(k, e)| {
                        if e.expire != 0 {
                            m.vol.remove(&k);
                        }
                        (k, snap(&e))
                    });
                    assert_eq!(a, b);
                }
                4 => {
                    let v = value(&mut r);
                    let old_m = m.keys.get_mut(&k).map(|e| format!("{:?}", std::mem::replace(&mut e.value, v.clone())));
                    let old_s = ks.get_mut(&k).map(|mut e| format!("{:?}", e.replace_value(v)));
                    assert_eq!(old_s, old_m);
                }
                5 => {
                    let at = if r.next_u64().is_multiple_of(4) { 0 } else { (r.next_u64() % 1_000_000) as i64 };
                    m.set_expire(&k, at);
                    if let Some(mut e) = ks.get_mut(&k) {
                        e.set_expire_raw(at);
                    }
                }
                6 => {
                    let lru = r.next_u64() as u32;
                    if let Some(e) = m.keys.get_mut(&k) {
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
                    let f = |v: &mut Value| {
                        if let Value::Str(s) = v {
                            let mut b = s.to_vec();
                            b.extend(std::iter::repeat_n(b'g', grow));
                            *s = Str::from_slice(&b);
                        }
                    };
                    if let Some(e) = m.keys.get_mut(&k) {
                        f(&mut e.value);
                    }
                    if let Some(mut e) = ks.get_mut(&k) {
                        e.with_value(f);
                    }
                }
                _ => {
                    let a = ks.get(&k).map(|e| snap(&e.to_entry()));
                    let b = m.keys.get(&k).map(snap);
                    assert_eq!(a, b);
                    assert_eq!(ks.get(&k).map(|e| e.value_mem()), m.keys.get(&k).map(|e| e.value.mem()));
                }
            }
            if round % 5_000 == 0 || round == 59_999 {
                check(&ks, &m, round);
            }
        }
        let mut n = 0;
        ks.for_each_mut(|mut e| {
            e.set_sized(1);
            n += 1;
        });
        assert_eq!(n, m.keys.len());
        assert!(ks.iter().all(|(_, e)| Fields::sized(&e) == 1));
        ks.clear();
        assert!(ks.is_empty() && ks.expires_is_empty());
    }

    fn check(ks: &KeySpace, m: &Model, round: u64) {
        assert_eq!((ks.len(), ks.expires_len()), (m.keys.len(), m.vol.len()));
        assert_eq!((ks.d.rehash_index(), ks.x.rehash_index()), (m.keys.rehash_index(), m.vol.rehash_index()), "rehash index at {round}");
        assert_eq!((ks.buckets(), ks.table_bytes(), ks.is_rehashing()), (m.keys.buckets(), m.keys.table_bytes(), m.keys.is_rehashing()));
        assert_eq!((ks.expires_table_bytes(), ks.expires_is_rehashing()), (m.vol.table_bytes(), m.vol.is_rehashing()));
        let a: Vec<_> = ks.iter().map(|(k, e)| (k.to_vec(), snap(&e.to_entry()))).collect();
        let b: Vec<_> = m.keys.iter().map(|(k, e)| (k.to_vec(), snap(e))).collect();
        assert_eq!(a, b);
        let a: Vec<_> = ks.expires_iter().map(|k| k.to_vec()).collect();
        let b: Vec<_> = m.vol.iter().map(|(k, _)| k.to_vec()).collect();
        assert_eq!(a, b);
        let (mut ca, mut cb) = (0, 0);
        loop {
            let (mut ka, mut kb) = (Vec::new(), Vec::new());
            ca = ks.scan(ca, |k, _| ka.push(k.to_vec()));
            cb = m.keys.scan(cb, |k, _| kb.push(k.to_vec()));
            assert_eq!((ca, &ka), (cb, &kb));
            if ca == 0 {
                break;
            }
        }
        let (mut ca, mut cb) = (0, 0);
        loop {
            let (mut ka, mut kb) = (Vec::new(), Vec::new());
            ca = ks.expires_scan(ca, |k, e| {
                assert_ne!(e.expire(), 0);
                ka.push(k.to_vec())
            });
            cb = m.vol.scan(cb, |k, _| kb.push(k.to_vec()));
            assert_eq!((ca, &ka), (cb, &kb));
            if ca == 0 {
                break;
            }
        }
        let (mut ra, mut rb) = (Rng::seeded(round), Rng::seeded(round));
        assert_eq!(ks.random(&mut ra).map(|(k, _)| k.to_vec()), m.keys.random(&mut rb).map(|(k, _)| k.to_vec()));
        assert_eq!(ks.expires_random(&mut ra).map(|(k, _)| k.to_vec()), m.vol.random(&mut rb).map(|(k, _)| k.to_vec()));
        assert_eq!(format!("{:?}", ks.expires_stats(true)), format!("{:?}", m.vol.stats(true)));
        let (mut sa, mut sb) = (Vec::new(), Vec::new());
        ks.sample(&mut ra, 20, |k, _| sa.push(k.to_vec()));
        m.keys.sample(&mut rb, 20, |k, _| sb.push(k.to_vec()));
        assert_eq!(sa, sb, "keys sample");
        ks.expires_sample(&mut ra, 20, |k, _| sa.push(k.to_vec()));
        m.vol.sample(&mut rb, 20, |k, _| sb.push(k.to_vec()));
        assert_eq!(sa.len(), sb.len(), "expires sample {round} {:?}", ks.expires_stats(false));
        assert_eq!(sa, sb);
        assert_eq!(format!("{:?}", ks.stats(true)), format!("{:?}", m.keys.stats(true)));
        assert_eq!(format!("{:?}", ks.expires_stats(true)), format!("{:?}", m.vol.stats(true)));
    }

    #[test]
    fn node_sizes() {
        assert_eq!(std::mem::size_of::<Head>(), 12);
        let meta = Meta { lru: 0, sized: 0, obj: 0, sds: 0 };
        let size = |key: &[u8], exp: i64, v: &[u8]| {
            let p = node_new(key, meta, exp, Value::str(v));
            let n = NodeRef::new(p).shape().size;
            unsafe { node_free(p) };
            n
        };
        // A 16-byte key with a 3-byte string: one 32-byte allocation (Valkey: 48); with a TTL, 40.
        assert_eq!(size(b"key:000000123456", 0, b"xxx"), 32);
        assert_eq!(size(b"key:000000123456", 1, b"xxx"), 40);
        assert_eq!((size_class(65), size_class(128), size_class(129), size_class(300)), (80, 128, 160, 320));
    }
}
