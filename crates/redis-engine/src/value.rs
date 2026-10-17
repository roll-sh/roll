//! Values stored in the keyspace.

use crate::bloom::{Sketch, SketchKind};
use crate::json::Json;
use crate::types::{Array, Hash, List, Set, Stream, ZSet};
use crate::ts::Series;
use crate::vset::VSet;
use smallvec::SmallVec;

/// A byte string with up to 22 bytes stored inline (no allocation): keys, small values, members.
pub type Str = SmallVec<[u8; 22]>;

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
        match self {
            Value::Str(_) => Kind::String,
            Value::List(_) => Kind::List,
            Value::Hash(_) => Kind::Hash,
            Value::Set(_) => Kind::Set,
            Value::ZSet(_) => Kind::ZSet,
            Value::Stream(_) => Kind::Stream,
            Value::Array(_) => Kind::Array,
            Value::Json(_) => Kind::Json,
            Value::Sketch(s) => Kind::Sketch(s.kind()),
            Value::VSet(_) => Kind::VSet,
            Value::Ts(_) => Kind::Ts,
            Value::Sug(_) => Kind::Sug,
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

    /// What Redis allocates for an aggregate's encoding (`kvobjAllocSize` past the object and its
    /// key; strings are measured with their object, [`crate::object::usage`]).
    pub fn alloc_size(&self) -> usize {
        match self {
            Value::Str(_) => 0,
            Value::List(l) => l.alloc_size(),
            Value::Hash(h) => h.alloc_size(),
            Value::Set(x) => x.alloc_size(),
            Value::ZSet(z) => z.alloc_size(),
            Value::Stream(x) => x.alloc_size(),
            Value::Array(x) => x.mem(),
            Value::Json(j) => j.usage(),
            Value::Sketch(s) => s.mem_usage() as usize,
            Value::VSet(v) => v.mem_usage() as usize,
            Value::Ts(t) => t.mem_usage(0),
            Value::Sug(t) => t.mem_usage(),
        }
    }

    /// Aggregates are deleted when they become empty. Strings and streams never are.
    #[inline]
    pub fn is_empty_aggregate(&self) -> bool {
        match self {
            Value::Str(_) | Value::Stream(_) | Value::Json(_) | Value::Sketch(_) | Value::VSet(_) | Value::Ts(_) | Value::Sug(_) => false,
            Value::List(l) => l.is_empty(),
            Value::Hash(h) => h.is_empty(),
            Value::Set(x) => x.is_empty(),
            Value::ZSet(z) => z.is_empty(),
            Value::Array(a) => a.is_empty(),
        }
    }

    /// `OBJECT ENCODING`.
    pub fn encoding(&self) -> &'static str {
        match self {
            Value::Str(s) => {
                if s.len() <= 20 && crate::util::is_int(s) {
                    "int"
                } else if s.len() <= 44 {
                    "embstr"
                } else {
                    "raw"
                }
            }
            Value::List(l) => l.encoding(),
            Value::Hash(h) => h.encoding(),
            Value::Set(x) => x.encoding(),
            Value::ZSet(z) => z.encoding(),
            Value::Stream(_) => "stream",
            Value::Array(_) => "sliced-array",
            Value::Json(_) | Value::Sketch(_) | Value::VSet(_) | Value::Ts(_) | Value::Sug(_) => "raw",
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

impl Value {
    /// The keysizes histogram slot (`type * SIZE_BINS + bin`): bin 0 for an empty value, else
    /// `floor(log2(len)) + 1` of the string length or element count (Redis' `getObjectLength`).
    pub fn size_slot(&self) -> u16 {
        let (t, n) = match self {
            Value::Str(s) => (0, s.len()),
            Value::List(l) => (1, l.len()),
            Value::Set(s) => (2, s.len()),
            Value::ZSet(z) => (3, z.len()),
            Value::Hash(h) => (4, h.len()),
            Value::Stream(_) | Value::Array(_) | Value::Json(_) | Value::Sketch(_) | Value::VSet(_) | Value::Ts(_) | Value::Sug(_) => return NO_SIZE,
        };
        let bin = (usize::BITS - n.leading_zeros()) as usize;
        (t * SIZE_BINS + bin.min(SIZE_BINS - 1)) as u16
    }
}

/// Fixed bytes per key: the dict node plus the entry, and allocator rounding.
pub const KEY_OVERHEAD: usize = std::mem::size_of::<Entry>() + std::mem::size_of::<Str>() + 24;

/// Heap bytes of a key beyond the fixed overhead.
#[inline]
pub fn key_mem(k: &Str) -> usize {
    if k.spilled() { k.capacity() } else { 0 }
}
