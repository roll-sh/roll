//! Redis 8's key objects (`kvobj`): a 16 byte header, an expire slot when the key may expire, the
//! key as an embedded sds string, then the value. Strings are `int` (in the header), `embstr`
//! (embedded too while key and value fit a 64 byte cache line) or `raw` (an sds string of their
//! own, sized by the allocator and grown greedily in place). Entries record what their history
//! decided ([`Entry::obj`](crate::value::Entry)) so OBJECT ENCODING, MEMORY USAGE and DEBUG
//! OBJECT/SDSLEN report what Redis would.

use crate::client::jemalloc_size;
use crate::keyspace::{Fields, FieldsMut};
use crate::value::ValueRef;

/// A string kept as SET keeps it: `int` when it reads as one, `embstr` while it fits with its key,
/// otherwise `raw`.
pub const ENC_AUTO: u8 = 0;
/// Never `int` (INCRBYFLOAT's result): `embstr` or `raw` by size.
pub const ENC_STR: u8 = 1;
/// `raw`: changed in place (APPEND, SETRANGE, SETBIT…), created raw (BITOP) or moved next to a
/// key it doesn't fit with.
pub const ENC_RAW: u8 = 2;
const ENC_MASK: u8 = 3;
/// The object has an expire slot (8 bytes): from the first TTL until the object is recreated,
/// and always for keys of 128 bytes or more.
pub const SLOT: u8 = 4;
/// A raw string grown in place: its sds allocation's size class is in `Entry::sds` (otherwise it
/// is exactly sized for its length).
pub const GROWN: u8 = 8;

/// Keys from this length always get an expire slot (`KEY_SIZE_TO_INCLUDE_EXPIRE_THRESHOLD`).
pub const SLOT_KEY_LEN: usize = 128;
/// `sizeof(robj)`.
const ROBJ: usize = 16;
/// The longest string created embedded (`OBJ_ENCODING_EMBSTR_SIZE_LIMIT`).
const EMBSTR_MAX: usize = 44;
const CACHE_LINE: usize = 64;
/// sds grows by doubling up to this, then by this much (`SDS_MAX_PREALLOC`).
const SDS_MAX_PREALLOC: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StrEnc {
    Int,
    Embstr,
    Raw,
}

impl StrEnc {
    pub fn name(self) -> &'static str {
        match self {
            StrEnc::Int => "int",
            StrEnc::Embstr => "embstr",
            StrEnc::Raw => "raw",
        }
    }
}

/// Whether a value reads as a `long` the way `string2l` does (no sign, spaces or leading zeros).
pub fn int_encodable(s: &[u8]) -> bool {
    s.len() <= 20 && crate::util::is_int(s)
}

/// The encoding a string value has before it is stored (`tryObjectEncoding`): what decides
/// between swapping into the old object and replacing it.
pub fn created(s: &[u8], enc: u8) -> StrEnc {
    match enc & ENC_MASK {
        ENC_AUTO if int_encodable(s) => StrEnc::Int,
        ENC_RAW => StrEnc::Raw,
        _ if s.len() <= EMBSTR_MAX => StrEnc::Embstr,
        _ => StrEnc::Raw,
    }
}

/// Whether a value is stored as one of Redis' shared integers 0..9999
/// (`createStringObjectFromLongLongForValue`); `allowed` is [`crate::config::Config::shared_integers`].
/// A shared object is never swapped into the old one by `dbSetValue`.
pub fn shared_int(s: &[u8], enc: u8, allowed: bool) -> bool {
    allowed && enc & ENC_MASK == ENC_AUTO && s.len() <= 4 && s.first() != Some(&b'-') && int_encodable(s)
}

/// Whether an embedded string stays embedded next to a key of `key_len` (`kvobjSet`).
pub fn fits(key_len: usize, val_len: usize) -> bool {
    ROBJ + key_len + 3 + 4 + val_len <= CACHE_LINE
}

/// The stored encoding of a string under a key of `key_len`.
pub fn str_encoding(obj: u8, key_len: usize, s: &[u8]) -> StrEnc {
    match created(s, obj) {
        StrEnc::Embstr if !fits(key_len, s.len()) => StrEnc::Raw,
        e => e,
    }
}

// --- sds strings ---

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum SdsType {
    T5,
    T8,
    T16,
    T32,
    T64,
}

fn req_type(len: usize) -> SdsType {
    if len < 32 {
        SdsType::T5
    } else if len < 256 - 3 {
        SdsType::T8
    } else if len < 65536 - 5 {
        SdsType::T16
    } else if (len as u64) < (1u64 << 32) - 9 {
        SdsType::T32
    } else {
        SdsType::T64
    }
}

fn hdr(t: SdsType) -> usize {
    match t {
        SdsType::T5 => 1,
        SdsType::T8 => 3,
        SdsType::T16 => 5,
        SdsType::T32 => 9,
        SdsType::T64 => 17,
    }
}

fn type_max(t: SdsType) -> u64 {
    match t {
        SdsType::T5 => 31,
        SdsType::T8 => 255,
        SdsType::T16 => 65535,
        SdsType::T32 => (1u64 << 32) - 1,
        SdsType::T64 => u64::MAX,
    }
}

/// An sds allocation: its header type and buffer size (`sdsAllocSize`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sds {
    t: SdsType,
    size: usize,
}

impl Sds {
    /// `sdsnewlen`: exactly sized, type 5 for short strings (which can't record free space).
    pub fn new(len: usize) -> Self {
        let mut t = req_type(len);
        if t == SdsType::T5 && len == 0 {
            t = SdsType::T8;
        }
        if t == SdsType::T5 {
            // Type 5 reports its requested size.
            return Sds { t, size: 1 + len + 1 };
        }
        Self::sized(t, jemalloc_size(hdr(t) + len + 1))
    }

    /// The type a buffer of `size` ends up with (`adjustTypeIfNeeded`).
    fn sized(mut t: SdsType, size: usize) -> Self {
        if (size - hdr(t) - 1) as u64 > type_max(t) {
            t = req_type(size - hdr(t) - 1);
        }
        Sds { t, size }
    }

    /// Room for content past the header.
    pub fn alloc(&self) -> usize {
        match self.t {
            SdsType::T5 => self.size - 2,
            t => self.size - hdr(t) - 1,
        }
    }

    /// `sdsAllocSize`.
    pub fn size(&self) -> usize {
        self.size
    }

    /// `sdsMakeRoomFor` (greedy) for `len` bytes growing by `add`: None when it fits already.
    pub fn make_room(&self, len: usize, add: usize) -> Option<Sds> {
        let avail = if self.t == SdsType::T5 { 0 } else { self.alloc() - len };
        if avail >= add {
            return None;
        }
        let mut new = len + add;
        new = if new < SDS_MAX_PREALLOC { new * 2 } else { new + SDS_MAX_PREALLOC };
        let t = match req_type(new) {
            SdsType::T5 => SdsType::T8,
            t => t,
        };
        Some(Self::sized(t, jemalloc_size(hdr(t) + new + 1)))
    }

    /// Free space past `len` bytes (none for type 5, which doesn't record it).
    pub fn avail(&self, len: usize) -> usize {
        if self.t == SdsType::T5 { 0 } else { self.alloc().saturating_sub(len) }
    }

    /// `sdsResize(s, size, 1)`: the buffer reallocated for `size` bytes of content.
    pub fn resize(&self, size: usize) -> Sds {
        if self.alloc() == size {
            return *self;
        }
        let t = match req_type(size) {
            SdsType::T5 => SdsType::T8,
            t => t,
        };
        let same = self.t == t || (t < self.t && t > SdsType::T8);
        let keep = if same { self.t } else { t };
        Self::sized(keep, jemalloc_size(hdr(keep) + size + 1))
    }

    /// A grown buffer from its size class (types 8 and up).
    fn of_class(class: u8) -> Self {
        let size = class_size(class);
        let t = [SdsType::T8, SdsType::T16, SdsType::T32].into_iter().find(|&t| (size - hdr(t) - 1) as u64 <= type_max(t)).unwrap_or(SdsType::T64);
        Sds { t, size }
    }
}

/// jemalloc size classes by index: 8 … 64 by 8, then four per doubling.
fn class_size(i: u8) -> usize {
    match i {
        0..=7 => (i as usize + 1) * 8,
        _ => {
            let j = (i - 8) as usize;
            let lg = 6 + j / 4;
            (5 + j % 4) << (lg - 2)
        }
    }
}

fn class_of(size: usize) -> u8 {
    if size <= 64 {
        return (size.max(1).div_ceil(8) - 1) as u8;
    }
    let lg = (usize::BITS - (size - 1).leading_zeros() - 1) as usize;
    let step = 1usize << (lg - 2);
    (8 + (lg - 6) * 4 + (size.div_ceil(step) - 5)) as u8
}

/// The sds string behind a raw value of `len` bytes.
pub fn raw_sds(e: &(impl Fields + ?Sized), len: usize) -> Sds {
    if e.obj() & GROWN != 0 { Sds::of_class(e.sds()) } else { Sds::new(len) }
}

// --- whole objects ---

/// Bytes of the key's embedded sds string (exactly sized).
fn key_sds(key_len: usize) -> usize {
    hdr(req_type(key_len)) + key_len + 1
}

/// The object header with its expire slot and embedded key (`kvobjCreate`).
fn header(e: &(impl Fields + ?Sized), key_len: usize) -> usize {
    ROBJ + if e.obj() & SLOT != 0 { 8 } else { 0 } + 1 + key_sds(key_len)
}

/// An embedded string's object: header, key and value in one allocation, the value taking what
/// the allocator gives past them.
fn embstr_alloc(e: &(impl Fields + ?Sized), key_len: usize, val_len: usize) -> usize {
    jemalloc_size(header(e, key_len) + 3 + val_len + 1)
}

/// MEMORY USAGE for a string (`kvobjAllocSize`).
pub fn string_usage(e: &(impl Fields + ?Sized), key_len: usize, s: &[u8]) -> usize {
    match str_encoding(e.obj(), key_len, s) {
        StrEnc::Int => header(e, key_len),
        StrEnc::Embstr => embstr_alloc(e, key_len, s.len()),
        StrEnc::Raw => header(e, key_len) + raw_sds(e, s.len()).size(),
    }
}

// --- aggregates (`kvobjAllocSize` by type) ---

/// `sizeof(dict)`.
const DICT: usize = 56;
/// `sizeof(dictEntryNoValue)`: what `dictMemUsage` charges per element of a set, sorted set or
/// hash table.
const DICT_ENTRY: usize = 16;
/// An empty listpack: its 6 byte header and the end byte.
const LP_EMPTY: usize = 7;
/// `sizeof(ExpireMeta)`.
pub const EXPIRE_META: usize = 16;
/// `EMBED_VALUE_MAX_ALLOC_SIZE`: a hash entry embeds its value while the whole entry fits.
const EMBED_VALUE_MAX: usize = 128;

/// MEMORY USAGE (`kvobjAllocSize`): the object with its key, and what the value allocates.
pub fn usage(e: &(impl Fields + ?Sized), key_len: usize) -> usize {
    match e.value() {
        ValueRef::Str(s) => string_usage(e, key_len, s),
        // kvobjComputeSize takes zmalloc_size of a module value's object.
        ValueRef::Json(j) => jemalloc_size(header(e, key_len)) + j.usage(),
        ValueRef::Sketch(x) => jemalloc_size(header(e, key_len)) + x.mem_usage() as usize,
        ValueRef::VSet(x) => jemalloc_size(header(e, key_len)) + x.mem_usage() as usize,
        // The series' share of the label index is added by the caller, which has the index.
        ValueRef::Ts(x) => jemalloc_size(header(e, key_len)) + x.mem_usage(0),
        ValueRef::Sug(x) => jemalloc_size(header(e, key_len)) + x.mem_usage(),
        v => header(e, key_len) + v.alloc_size(),
    }
}

/// `sizeof(dict) + dictMemUsage(d)`: the struct, a bucket pointer per slot of both tables and an
/// entry per element.
pub fn dict_usage(len: usize, buckets: usize) -> usize {
    DICT + len * DICT_ENTRY + buckets * 8
}

/// `sdsAllocSize` of an `sdsnewlen` copy of `len` bytes.
pub fn sds_alloc(len: usize) -> usize {
    Sds::new(len).size()
}

/// `sdsReqSize` with the smallest type for `len` bytes: an sds placed inside another allocation.
pub fn sds_req(len: usize) -> usize {
    hdr(req_type(len)) + len + 1
}

/// `lpBytes` of a listpack holding `items`.
pub fn lp_bytes<'a>(items: impl IntoIterator<Item = &'a [u8]>) -> usize {
    LP_EMPTY + items.into_iter().map(crate::types::list::lp_entry_size).sum::<usize>()
}

/// `lpBytes` of an empty listpack plus `entries` bytes of entries.
pub fn lp_with(entries: usize) -> usize {
    LP_EMPTY + entries
}

/// How a hash table entry (`entry.c`) holds its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryValue {
    /// Embedded after the field as a type 8 sds of this many bytes.
    Embedded(usize),
    /// Behind a pointer, in an sds of its own.
    Pointer,
}

/// The layout `entryCreate` picks for a field and value: the value embedded while the entry
/// (field, TTL metadata, value) fits [`EMBED_VALUE_MAX`] bytes.
pub fn entry_layout(field_len: usize, value_len: usize, meta: bool) -> EntryValue {
    let embedded = 3 + value_len + 1;
    if entry_field(field_len, meta, false) + if meta { EXPIRE_META } else { 0 } + embedded <= EMBED_VALUE_MAX {
        EntryValue::Embedded(embedded)
    } else {
        EntryValue::Pointer
    }
}

/// The field's sds in an entry: type 8 at least when its aux bits are needed (TTL metadata or a
/// value pointer).
fn entry_field(field_len: usize, meta: bool, pointer: bool) -> usize {
    let mut t = req_type(field_len);
    if t == SdsType::T5 && (meta || pointer) {
        t = SdsType::T8;
    }
    hdr(t) + field_len + 1
}

/// `entryMemUsage`: the field, the value (embedded, or a pointer and its sds) and the TTL metadata.
pub fn entry_usage(field_len: usize, value_len: usize, meta: bool, value: EntryValue) -> usize {
    let exp = if meta { EXPIRE_META } else { 0 };
    match value {
        EntryValue::Embedded(slot) => entry_field(field_len, meta, false) + exp + slot,
        EntryValue::Pointer => entry_field(field_len, meta, true) + exp + 8 + sds_alloc(value_len),
    }
}

/// DEBUG SDSLEN for a string that isn't `int`: the key's sds and object allocation, the value's
/// length, free space and allocation (0 when embedded).
pub fn sdslen(e: &(impl Fields + ?Sized), key_len: usize, s: &[u8]) -> Option<String> {
    let (obj, avail, val_alloc) = match str_encoding(e.obj(), key_len, s) {
        StrEnc::Int => return None,
        StrEnc::Embstr => {
            let obj = embstr_alloc(e, key_len, s.len());
            (obj, obj - header(e, key_len) - 3 - 1 - s.len(), 0)
        }
        StrEnc::Raw => {
            let sds = raw_sds(e, s.len());
            (jemalloc_size(header(e, key_len)), sds.alloc() - s.len(), sds.size())
        }
    };
    Some(format!(
        "key_sds_len:{key_len}, key_sds_avail:0, key_zmalloc: {obj}, val_sds_len:{}, val_sds_avail:{avail}, val_zmalloc: {val_alloc}",
        s.len()
    ))
}

/// Redis' view of a stored object: its encoding and the sds behind a raw string.
pub trait Object: Fields {
    /// The encoding OBJECT ENCODING reports, given the entry's key length.
    fn encoding(&self, key_len: usize) -> &'static str {
        match self.value() {
            ValueRef::Str(s) => str_encoding(self.obj(), key_len, s).name(),
            v => v.encoding(),
        }
    }

    /// The sds string behind this raw string.
    fn sds_now(&self) -> Sds {
        match self.value() {
            ValueRef::Str(s) => raw_sds(self, s.len()),
            _ => Sds::new(0),
        }
    }

    /// Whether the value is an embedded string.
    fn embedded(&self, key_len: usize) -> bool {
        matches!(self.value(), ValueRef::Str(s) if str_encoding(self.obj(), key_len, s) == StrEnc::Embstr)
    }
}

impl<T: Fields + ?Sized> Object for T {}

/// Keeping Redis' view of an object up to date as it changes.
pub trait ObjectMut: FieldsMut {
    /// `dbUnshareStringValue`: a string about to change in place becomes raw, copied exactly
    /// sized unless it was raw already.
    fn unshare(&mut self, key_len: usize) {
        if let ValueRef::Str(s) = self.value()
            && str_encoding(self.obj(), key_len, s) != StrEnc::Raw
        {
            let obj = (self.obj() & SLOT) | ENC_RAW;
            self.set_obj(obj);
        }
    }

    /// Record a raw string's sds after it was reallocated (HyperLogLog updates).
    fn set_sds_of(&mut self, sds: Sds) {
        let ValueRef::Str(s) = self.value() else { return };
        if sds == Sds::new(s.len()) {
            let obj = self.obj() & !GROWN;
            self.set_obj(obj);
            self.set_sds(0);
        } else {
            let obj = self.obj() | GROWN;
            self.set_obj(obj);
            self.set_sds(class_of(sds.size()));
        }
    }

    /// `sdsMakeRoomFor` on a raw string about to grow by `add` bytes.
    fn sds_make_room(&mut self, add: usize) {
        let ValueRef::Str(s) = self.value() else { return };
        let len = s.len();
        let cur = raw_sds(self, len);
        // Either a new buffer, or the current one which no longer matches an exact copy of the
        // longer content (`sdsnewlen(0)` holding three bytes).
        let sds = cur.make_room(len, add).unwrap_or(cur);
        if sds != Sds::new(len + add) {
            let obj = self.obj() | GROWN;
            self.set_obj(obj);
            self.set_sds(class_of(sds.size()));
        }
    }

    /// After the value was replaced (`dbSetValue`; `old_embedded` from before): Redis keeps the
    /// old object, expire slot included, unless either value is embedded; otherwise it makes a
    /// new one with a slot only for a TTL that stays. `enc` is how the new value was created;
    /// `shared_ints` whether it may be a shared integer ([`shared_int`]).
    fn revalue(&mut self, old_embedded: bool, key_len: usize, enc: u8, keep_ttl: bool, shared_ints: bool) {
        let new_own = match self.value() {
            ValueRef::Str(s) => created(s, enc) != StrEnc::Embstr && !shared_int(s, enc, shared_ints),
            _ => true,
        };
        let slot = if !old_embedded && new_own {
            self.obj() & SLOT != 0
        } else {
            (keep_ttl && self.expire() != 0) || key_len >= SLOT_KEY_LEN
        };
        self.set_obj(enc | if slot { SLOT } else { 0 });
        self.set_sds(0);
    }

    /// The object rebuilt for another key (RENAME, MOVE, COPY): an embedded string that doesn't
    /// fit next to the new key becomes raw and stays raw; a copy is sized exactly. The expire
    /// slot follows the TTL ([`crate::db::Db::insert_new`]).
    fn rekey(&mut self, old_len: usize, new_len: usize, copy: bool) {
        let cur = self.obj();
        let mut obj = cur & ENC_MASK;
        if let ValueRef::Str(s) = self.value() {
            match str_encoding(cur, old_len, s) {
                StrEnc::Embstr if !fits(new_len, s.len()) => obj = ENC_RAW,
                StrEnc::Raw => {
                    obj = ENC_RAW;
                    if !copy {
                        obj |= cur & GROWN;
                    }
                }
                _ => {}
            }
        }
        if obj & GROWN == 0 {
            self.set_sds(0);
        }
        self.set_obj(obj);
    }
}

impl<T: FieldsMut + ?Sized> ObjectMut for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_classes_round_trip() {
        for i in 0..=120u8 {
            assert_eq!(class_of(class_size(i)), i, "{i}");
            assert_eq!(jemalloc_size(class_size(i)), class_size(i), "{i}");
        }
        assert_eq!((class_size(0), class_size(6), class_size(7), class_size(8), class_size(11), class_size(12)), (8, 56, 64, 80, 128, 160));
    }

    #[test]
    fn sds_sizes_like_redis() {
        // sdsnewlen: type 5 reports what it asked for, others the allocator's size.
        assert_eq!((Sds::new(1).size(), Sds::new(5).size(), Sds::new(41).size(), Sds::new(45).size(), Sds::new(200).size()), (3, 7, 48, 56, 224));
        assert_eq!((Sds::new(45).alloc(), Sds::new(200).alloc()), (52, 220));
        // "hello" + "x": doubled to 12 as type 8, a 16 byte buffer.
        let g = Sds::new(5).make_room(5, 1).unwrap();
        assert_eq!((g.size(), g.alloc()), (16, 12));
        assert_eq!(g.make_room(6, 1), None);
        assert_eq!(Sds::of_class(class_of(16)), g);
    }
}
