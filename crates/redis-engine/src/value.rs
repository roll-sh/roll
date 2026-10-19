//! Values stored in the keyspace.

use crate::bloom::{Sketch, SketchKind};
use crate::json::Json;
use crate::types::{Array, Hash, List, Set, Stream, ZSet};
use crate::ts::Series;
use crate::vset::VSet;

/// A byte string with up to 23 bytes stored inline (no allocation): keys, small values, members.
pub type Str = crate::bytestr::Str;

#[inline]
pub fn s(b: &[u8]) -> Str {
    Str::from_slice(b)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    String,
    List,
    Hash,
    Set,
    ZSet,
    Stream,
    Array,
    /// RedisJSON documents (a module type).
    Json,
    /// RedisBloom's module types.
    Sketch(SketchKind),
    /// Vector sets (a module type).
    VSet,
    Ts,
    /// RediSearch's suggestion dictionaries (FT.SUGADD), a module type.
    Sug,
}

impl Kind {
    /// Whether Redis' object types (`o->type`) differ: `type_changed` fires on that, and every
    /// module type is `OBJ_MODULE` (a TS key replaced by a JSON document doesn't fire it).
    pub fn obj_type_changed(self, other: Kind) -> bool {
        let module = |k: Kind| matches!(k, Kind::Json | Kind::Sketch(_) | Kind::VSet | Kind::Ts | Kind::Sug);
        if module(self) && module(other) { false } else { self != other }
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::String => "string",
            Kind::List => "list",
            Kind::Hash => "hash",
            Kind::Set => "set",
            Kind::ZSet => "zset",
            Kind::Stream => "stream",
            Kind::Array => "array",
            Kind::Json => "ReJSON-RL",
            Kind::Sketch(k) => k.name(),
            Kind::VSet => crate::vset::TYPE_NAME,
            Kind::Ts => crate::ts::TYPE_NAME,
            Kind::Sug => crate::search::trie::TYPE_NAME,
        }
    }

    pub fn parse(s: &[u8]) -> Option<Kind> {
        let lower = s.to_ascii_lowercase();
        if let Some(k) = SketchKind::parse(&lower) {
            return Some(Kind::Sketch(k));
        }
        Some(match lower.as_slice() {
            b"string" => Kind::String,
            b"list" => Kind::List,
            b"hash" => Kind::Hash,
            b"set" => Kind::Set,
            b"zset" => Kind::ZSet,
            b"stream" => Kind::Stream,
            b"array" => Kind::Array,
            b"rejson-rl" => Kind::Json,
            b"vectorset" => Kind::VSet,
            b"tsdb-type" => Kind::Ts,
            b"trietype0" => Kind::Sug,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub enum Value {
    Str(Str),
    List(Box<List>),
    Hash(Box<Hash>),
    Set(Box<Set>),
    ZSet(Box<ZSet>),
    Stream(Box<Stream>),
    Array(Box<Array>),
    Json(Box<Json>),
    Sketch(Box<Sketch>),
    VSet(Box<VSet>),
    Ts(Box<Series>),
    Sug(Box<crate::search::trie::Trie>),
}

impl Value {
    /// What a module's `rdb_save` does to the value it serializes: a t-digest compresses its
    /// buffered samples (DUMP, DEBUG OBJECT, MIGRATE, SAVE; not forked saves).
    pub fn prepare_save(&mut self) {
        if let Value::Sketch(x) = self
            && let crate::bloom::Sketch::TDigest(t) = &mut **x
        {
            let _ = t.compress();
        }
    }

    pub fn str(b: &[u8]) -> Self {
        Value::Str(Str::from_slice(b))
    }

    #[inline]
    pub fn kind(&self) -> Kind {
        self.view().kind()
    }

    /// What Redis allocates for an aggregate's encoding ([`ValueRef::alloc_size`]).
    pub fn alloc_size(&self) -> usize {
        self.view().alloc_size()
    }

    /// Aggregates are deleted when they become empty. Strings and streams never are.
    #[inline]
    pub fn is_empty_aggregate(&self) -> bool {
        self.view().is_empty_aggregate()
    }

    /// `OBJECT ENCODING`.
    pub fn encoding(&self) -> &'static str {
        self.view().encoding()
    }

    /// The keysizes histogram slot ([`ValueRef::size_slot`]).
    pub fn size_slot(&self) -> u16 {
        self.view().size_slot()
    }

    /// The value seen in place.
    #[inline]
    pub fn view(&self) -> ValueRef<'_> {
        match self {
            Value::Str(s) => ValueRef::Str(s),
            Value::List(x) => ValueRef::List(x),
            Value::Hash(x) => ValueRef::Hash(x),
            Value::Set(x) => ValueRef::Set(x),
            Value::ZSet(x) => ValueRef::ZSet(x),
            Value::Stream(x) => ValueRef::Stream(x),
            Value::Array(x) => ValueRef::Array(x),
            Value::Json(x) => ValueRef::Json(x),
            Value::Sketch(x) => ValueRef::Sketch(x),
            Value::VSet(x) => ValueRef::VSet(x),
            Value::Ts(x) => ValueRef::Ts(x),
            Value::Sug(x) => ValueRef::Sug(x),
        }
    }

    pub fn new_of(kind: Kind) -> Self {
        match kind {
            Kind::String => Value::Str(Str::new()),
            Kind::List => Value::List(Box::default()),
            Kind::Hash => Value::Hash(Box::default()),
            Kind::Set => Value::Set(Box::default()),
            Kind::ZSet => Value::ZSet(Box::default()),
            Kind::Stream => Value::Stream(Box::default()),
            Kind::Array => Value::Array(Box::default()),
            Kind::Json => Value::Json(Box::default()),
            Kind::Sketch(_) | Kind::VSet | Kind::Ts | Kind::Sug => unreachable!("module values are created by their commands"),
        }
    }

    /// Approximate heap bytes owned by the value (excluding the `Value` itself). O(1).
    pub fn mem(&self) -> usize {
        match self {
            Value::Str(s) => {
                if s.spilled() {
                    s.capacity()
                } else {
                    0
                }
            }
            Value::List(l) => std::mem::size_of::<List>() + l.mem(),
            Value::Hash(h) => std::mem::size_of::<Hash>() + h.mem(),
            Value::Set(x) => std::mem::size_of::<Set>() + x.mem(),
            Value::ZSet(z) => std::mem::size_of::<ZSet>() + z.mem(),
            Value::Stream(x) => std::mem::size_of::<Stream>() + x.mem(),
            Value::Array(x) => std::mem::size_of::<Array>() + x.mem(),
            Value::Json(j) => std::mem::size_of::<Json>() + j.mem(),
            Value::Sketch(s) => std::mem::size_of::<Sketch>() + s.mem(),
            Value::VSet(v) => v.mem_usage() as usize,
            Value::Ts(t) => t.approx_mem(),
            Value::Sug(t) => t.mem_usage(),
        }
    }

}

/// A stored value seen in place: strings as their bytes (the keyspace may hold them packed with
/// their key), the other types as themselves.
#[derive(Clone, Copy, Debug)]
pub enum ValueRef<'a> {
    Str(&'a [u8]),
    List(&'a List),
    Hash(&'a Hash),
    Set(&'a Set),
    ZSet(&'a ZSet),
    Stream(&'a Stream),
    Array(&'a Array),
    Json(&'a Json),
    Sketch(&'a Sketch),
    VSet(&'a VSet),
    Ts(&'a Series),
    Sug(&'a crate::search::trie::Trie),
}

impl<'a> ValueRef<'a> {
    #[inline]
    pub fn kind(self) -> Kind {
        match self {
            ValueRef::Str(_) => Kind::String,
            ValueRef::List(_) => Kind::List,
            ValueRef::Hash(_) => Kind::Hash,
            ValueRef::Set(_) => Kind::Set,
            ValueRef::ZSet(_) => Kind::ZSet,
            ValueRef::Stream(_) => Kind::Stream,
            ValueRef::Array(_) => Kind::Array,
            ValueRef::Json(_) => Kind::Json,
            ValueRef::Sketch(s) => Kind::Sketch(s.kind()),
            ValueRef::VSet(_) => Kind::VSet,
            ValueRef::Ts(_) => Kind::Ts,
            ValueRef::Sug(_) => Kind::Sug,
        }
    }

    /// What Redis allocates for an aggregate's encoding (`kvobjAllocSize` past the object and its
    /// key; strings are measured with their object, [`crate::object::usage`]).
    pub fn alloc_size(self) -> usize {
        match self {
            ValueRef::Str(_) => 0,
            ValueRef::List(l) => l.alloc_size(),
            ValueRef::Hash(h) => h.alloc_size(),
            ValueRef::Set(x) => x.alloc_size(),
            ValueRef::ZSet(z) => z.alloc_size(),
            ValueRef::Stream(x) => x.alloc_size(),
            ValueRef::Array(x) => x.mem(),
            ValueRef::Json(j) => j.usage(),
            ValueRef::Sketch(s) => s.mem_usage() as usize,
            ValueRef::VSet(v) => v.mem_usage() as usize,
            ValueRef::Ts(t) => t.mem_usage(0),
            ValueRef::Sug(t) => t.mem_usage(),
        }
    }

    /// Aggregates are deleted when they become empty. Strings and streams never are.
    #[inline]
    pub fn is_empty_aggregate(self) -> bool {
        match self {
            ValueRef::Str(_) | ValueRef::Stream(_) | ValueRef::Json(_) | ValueRef::Sketch(_) | ValueRef::VSet(_) | ValueRef::Ts(_) | ValueRef::Sug(_) => false,
            ValueRef::List(l) => l.is_empty(),
            ValueRef::Hash(h) => h.is_empty(),
            ValueRef::Set(x) => x.is_empty(),
            ValueRef::ZSet(z) => z.is_empty(),
            ValueRef::Array(a) => a.is_empty(),
        }
    }

    /// `OBJECT ENCODING`.
    pub fn encoding(self) -> &'static str {
        match self {
            ValueRef::Str(s) => {
                if s.len() <= 20 && crate::util::is_int(s) {
                    "int"
                } else if s.len() <= 44 {
                    "embstr"
                } else {
                    "raw"
                }
            }
            ValueRef::List(l) => l.encoding(),
            ValueRef::Hash(h) => h.encoding(),
            ValueRef::Set(x) => x.encoding(),
            ValueRef::ZSet(z) => z.encoding(),
            ValueRef::Stream(_) => "stream",
            ValueRef::Array(_) => "sliced-array",
            ValueRef::Json(_) | ValueRef::Sketch(_) | ValueRef::VSet(_) | ValueRef::Ts(_) | ValueRef::Sug(_) => "raw",
        }
    }

    /// The keysizes histogram slot (`type * SIZE_BINS + bin`): bin 0 for an empty value, else
    /// `floor(log2(len)) + 1` of the string length or element count (Redis' `getObjectLength`).
    pub fn size_slot(self) -> u16 {
        let (t, n) = match self {
            ValueRef::Str(s) => (0, s.len()),
            ValueRef::List(l) => (1, l.len()),
            ValueRef::Set(s) => (2, s.len()),
            ValueRef::ZSet(z) => (3, z.len()),
            ValueRef::Hash(h) => (4, h.len()),
            ValueRef::Stream(_) | ValueRef::Array(_) | ValueRef::Json(_) | ValueRef::Sketch(_) | ValueRef::VSet(_) | ValueRef::Ts(_) | ValueRef::Sug(_) => return NO_SIZE,
        };
        let bin = (usize::BITS - n.leading_zeros()) as usize;
        (t * SIZE_BINS + bin.min(SIZE_BINS - 1)) as u16
    }

    /// An owned copy.
    pub fn to_value(self) -> Value {
        match self {
            ValueRef::Str(s) => Value::str(s),
            ValueRef::List(x) => Value::List(Box::new(x.clone())),
            ValueRef::Hash(x) => Value::Hash(Box::new(x.clone())),
            ValueRef::Set(x) => Value::Set(Box::new(x.clone())),
            ValueRef::ZSet(x) => Value::ZSet(Box::new(x.clone())),
            ValueRef::Stream(x) => Value::Stream(Box::new(x.clone())),
            ValueRef::Array(x) => Value::Array(Box::new(x.clone())),
            ValueRef::Json(x) => Value::Json(Box::new(x.clone())),
            ValueRef::Sketch(x) => Value::Sketch(Box::new(x.clone())),
            ValueRef::VSet(x) => Value::VSet(Box::new(x.clone())),
            ValueRef::Ts(x) => Value::Ts(Box::new(x.clone())),
            ValueRef::Sug(x) => Value::Sug(Box::new(x.clone())),
        }
    }
}

/// A key's slot in the keyspace.
#[derive(Clone, Debug)]
pub struct Entry {
    pub value: Value,
    /// Absolute expiry in Unix milliseconds; 0 means no TTL.
    pub expire: i64,
    /// LRU clock (seconds, 24 bits) or LFU state (16-bit minutes + 8-bit counter), per policy.
    pub lru: u32,
    /// The INFO keysizes histogram slot this key is counted in ([`Value::size_slot`]), kept by
    /// [`crate::db::Db`].
    pub sized: u16,
    /// How Redis would hold the object: string encoding, expire slot ([`crate::object`]).
    pub obj: u8,
    /// A grown raw string's sds size class ([`crate::object::GROWN`]).
    pub sds: u8,
}

impl Entry {
    pub fn new(value: Value, lru: u32) -> Self {
        Self { value, expire: 0, lru, sized: NO_SIZE, obj: 0, sds: 0 }
    }

    /// A key with a TTL (RESTORE, RDB load).
    pub fn with_expire(value: Value, expire: i64, lru: u32) -> Self {
        Self { expire, ..Self::new(value, lru) }
    }

    #[inline]
    pub fn expired(&self, now: i64) -> bool {
        self.expire != 0 && self.expire <= now
    }
}

/// Not counted in the keysizes histogram (streams, arrays).
pub const NO_SIZE: u16 = u16::MAX;
/// Keysizes histogram dimensions: strings, lists, sets, zsets, hashes × power-of-two bins.
pub const SIZE_TYPES: usize = 5;
pub const SIZE_BINS: usize = 60;

/// Fixed bytes per key: the dict node plus the entry, and allocator rounding.
pub const KEY_OVERHEAD: usize = std::mem::size_of::<Entry>() + std::mem::size_of::<Str>() + 24;

/// Heap bytes of a key beyond the fixed overhead.
#[inline]
pub fn key_mem(k: &Str) -> usize {
    if k.spilled() { k.capacity() } else { 0 }
}
