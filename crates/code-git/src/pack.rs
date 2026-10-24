//! Pack files (version 2): reading, with OFS_DELTA and REF_DELTA resolution, and writing.
//!
//! Reading never sizes an allocation from a count or length the peer sent. Buffers grow with
//! the bytes actually inflated, and every object, the pack and the total unpacked size are
//! capped by [`Limits`], so a small pack can't expand into a huge one (a "delta bomb").
//!
//! [`Parser`] reads a pack as it arrives, chunk by chunk, so the compressed pack is never held
//! whole: only its inflated entries are. A [`Meter`] is charged for every buffer before it
//! grows (inflated entries, delta results, thin-pack bases) and released as they're freed; the
//! git server charges its memory budget through it.
//!
//! Writing is undeltified, like Scion: the server never computes deltas, which keeps serving
//! O(bytes) and lets any cache answer with stored objects.

use crate::object::{Kind, Object, ObjectId};
use bytes::Bytes;
use flate2::{Compression, Decompress, FlushDecompress, write::ZlibEncoder};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io::Write;

/// Caps on what a peer can make us hold.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Largest pack accepted (the request or response body).
    pub max_pack_bytes: u64,
    /// Largest single object, after inflating and applying deltas.
    pub max_object_bytes: u64,
    /// Largest total of all objects in one pack, after inflating and applying deltas.
    pub max_unpacked_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_pack_bytes: 512 << 20, max_object_bytes: 100 << 20, max_unpacked_bytes: 2 << 30 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("not a pack file")]
    NotPack,
    #[error("unsupported pack version {0}")]
    Version(u32),
    #[error("pack is truncated")]
    Truncated,
    #[error("pack checksum mismatch")]
    Checksum,
    #[error("trailing data after the last pack entry")]
    TrailingData,
    #[error("pack is larger than {0} bytes")]
    PackTooLarge(u64),
    #[error("object is larger than {0} bytes")]
    ObjectTooLarge(u64),
    #[error("pack unpacks to more than {0} bytes")]
    UnpackedTooLarge(u64),
    #[error("invalid pack entry at offset {0}")]
    BadEntry(usize),
    #[error("corrupt compressed data at offset {0}")]
    Zlib(usize),
    #[error("invalid delta")]
    BadDelta,
    #[error("delta base {0} is missing")]
    MissingBase(ObjectId),
    /// The [`Meter`] refused memory.
    #[error(transparent)]
    Memory(#[from] crate::budget::BudgetError),
}

/// Accounts for the memory reading a pack holds. `charge` is called before a buffer grows and
/// may refuse; `release` when a buffer is freed. `()` accounts for nothing.
pub trait Meter: Send {
    fn charge(&mut self, bytes: u64) -> Result<(), PackError>;
    fn release(&mut self, bytes: u64);
}

impl Meter for () {
    fn charge(&mut self, _bytes: u64) -> Result<(), PackError> {
        Ok(())
    }
    fn release(&mut self, _bytes: u64) {}
}

/// Bookkeeping per entry on top of its data (the entry, its index slots and its resolved slot),
/// charged so a pack of many tiny objects is accounted for too.
const ENTRY_OVERHEAD: u64 = 160;
/// The trailing SHA-1.
const TRAILER: usize = 20;
/// Longest entry start: a 10-byte header and a 20-byte REF_DELTA base.
const MAX_ENTRY_START: usize = 30;

#[derive(Debug)]
enum EntryKind {
    Base(Kind),
    /// Index of the base entry in `entries`.
    Ofs(usize),
    Ref(ObjectId),
}

#[derive(Debug)]
struct Entry {
    kind: EntryKind,
    /// Inflated content (the delta instructions for delta entries).
    data: Vec<u8>,
}

/// A parsed pack whose deltas still need resolving. See [`Resolver`].
pub struct Pack {
    entries: Vec<Entry>,
    limits: Limits,
    inflated: u64,
    meter: Box<dyn Meter>,
}

impl std::fmt::Debug for Pack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pack").field("entries", &self.entries.len()).field("inflated", &self.inflated).finish()
    }
}

/// Counts for tests and logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackStats {
    pub objects: usize,
    pub ofs_deltas: usize,
    pub ref_deltas: usize,
}

impl Pack {
    pub fn stats(&self) -> PackStats {
        let mut s = PackStats { objects: self.entries.len(), ..Default::default() };
        for e in &self.entries {
            match e.kind {
                EntryKind::Ofs(_) => s.ofs_deltas += 1,
                EntryKind::Ref(_) => s.ref_deltas += 1,
                EntryKind::Base(_) => {}
            }
        }
        s
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Parses and inflates every entry of `data`, a complete pack including its trailing checksum.
pub fn parse(data: &[u8], limits: &Limits) -> Result<Pack, PackError> {
    if data.len() as u64 > limits.max_pack_bytes {
        return Err(PackError::PackTooLarge(limits.max_pack_bytes));
    }
    if data.len() < 12 {
        return Err(if data.starts_with(&b"PACK"[..data.len().min(4)]) { PackError::Truncated } else { PackError::NotPack });
    }
    if &data[..4] != b"PACK" {
        return Err(PackError::NotPack);
    }
    let version = u32::from_be_bytes(data[4..8].try_into().unwrap());
    if version != 2 && version != 3 {
        return Err(PackError::Version(version));
    }
    if data.len() < 32 {
        return Err(PackError::Truncated);
    }
    let end = data.len() - TRAILER;
    if Sha1::digest(&data[..end])[..] != data[end..] {
        return Err(PackError::Checksum);
    }
    let mut p = Parser::new(*limits, Box::new(()));
    p.verify = false; // just checked
    p.feed(data)?;
    p.finish()
}

enum State {
    Header,
    Entry,
    Inflate(Inflating),
    Done,
}

struct Inflating {
    /// Pack offset of the entry.
    offset: usize,
    kind: EntryKind,
    expected: usize,
    z: Decompress,
    out: Vec<u8>,
}

/// Reads a pack as it arrives: [`Parser::feed`] it chunks, then [`Parser::finish`]. Only a few
/// bytes of input are ever kept (a partial entry header and the last 20 bytes, which may be the
/// trailing checksum); entries are inflated as their bytes arrive.
pub struct Parser {
    limits: Limits,
    meter: Box<dyn Meter>,
    verify: bool,
    hasher: Sha1,
    /// Input not consumed yet.
    pending: Vec<u8>,
    /// Pack offset of the next input byte to consume.
    offset: usize,
    total: u64,
    count: u32,
    state: State,
    entries: Vec<Entry>,
    by_offset: HashMap<usize, usize>,
    inflated: u64,
}

enum Start {
    Entry(EntryKind, u64, usize),
    More,
}

impl Parser {
    pub fn new(limits: Limits, meter: Box<dyn Meter>) -> Self {
        Parser {
            limits,
            meter,
            verify: true,
            hasher: Sha1::new(),
            pending: Vec::new(),
            offset: 0,
            total: 0,
            count: 0,
            state: State::Header,
            entries: Vec::new(),
            by_offset: HashMap::new(),
            inflated: 0,
        }
    }

    /// Bytes fed so far.
    pub fn bytes(&self) -> u64 {
        self.total
    }

    pub fn feed(&mut self, data: &[u8]) -> Result<(), PackError> {
        self.total += data.len() as u64;
        if self.total > self.limits.max_pack_bytes {
            return Err(PackError::PackTooLarge(self.limits.max_pack_bytes));
        }
        if self.pending.is_empty() {
            let n = self.consume(data, false)?;
            self.pending.extend_from_slice(&data[n..]);
        } else {
            let mut buf = std::mem::take(&mut self.pending);
            buf.extend_from_slice(data);
            let n = self.consume(&buf, false)?;
            buf.drain(..n);
            self.pending = buf;
        }
        Ok(())
    }

    /// The end of the input: checks the trailer and returns the parsed pack.
    pub fn finish(mut self) -> Result<Pack, PackError> {
        let buf = std::mem::take(&mut self.pending);
        let n = self.consume(&buf, true)?;
        let rest = &buf[n..];
        if !matches!(self.state, State::Done) || rest.len() < TRAILER {
            return Err(PackError::Truncated);
        }
        if self.verify && self.hasher.clone().finalize()[..] != rest[..] {
            return Err(PackError::Checksum);
        }
        Ok(Pack { entries: self.entries, limits: self.limits, inflated: self.inflated, meter: self.meter })
    }

    /// Processes what it can of `input`, never the last 20 bytes unless the pack is complete;
    /// returns the bytes consumed (hashed and gone).
    fn consume(&mut self, input: &[u8], last: bool) -> Result<usize, PackError> {
        let n = self.step(input, last)?;
        if self.verify {
            self.hasher.update(&input[..n]);
        }
        self.offset += n;
        Ok(n)
    }

    fn step(&mut self, input: &[u8], last: bool) -> Result<usize, PackError> {
        let avail = input.len().saturating_sub(TRAILER);
        let mut pos = 0;
        loop {
            let rest = &input[pos..avail.max(pos)];
            match &mut self.state {
                State::Header => {
                    // Reject what isn't a pack as soon as it shows, before holding anything.
                    let magic = &input[..input.len().min(4)];
                    if !b"PACK".starts_with(magic) {
                        return Err(PackError::NotPack);
                    }
                    if input.len() >= 8 {
                        let version = u32::from_be_bytes(input[4..8].try_into().unwrap());
                        if version != 2 && version != 3 {
                            return Err(PackError::Version(version));
                        }
                    }
                    if rest.len() < 12 {
                        return if last { Err(PackError::Truncated) } else { Ok(pos) };
                    }
                    self.count = u32::from_be_bytes(rest[8..12].try_into().unwrap());
                    pos += 12;
                    self.state = State::Entry;
                }
                State::Entry => {
                    if self.entries.len() as u64 == u64::from(self.count) {
                        self.state = State::Done;
                        continue;
                    }
                    let offset = self.offset + pos;
                    let (kind, size, n) = match self.entry_start(rest, offset)? {
                        Start::Entry(k, s, n) => (k, s, n),
                        Start::More if last || rest.len() >= MAX_ENTRY_START => return Err(PackError::Truncated),
                        Start::More => return Ok(pos),
                    };
                    // Deltas are bounded like objects: a delta never needs to be larger than its result.
                    if size > self.limits.max_object_bytes {
                        return Err(PackError::ObjectTooLarge(self.limits.max_object_bytes));
                    }
                    self.meter.charge(ENTRY_OVERHEAD)?;
                    pos += n;
                    self.state = State::Inflate(Inflating {
                        offset,
                        kind,
                        expected: size as usize,
                        z: Decompress::new(true),
                        out: Vec::new(),
                    });
                }
                State::Inflate(inf) => {
                    let (used, done) = inf.step(rest, self.meter.as_mut())?;
                    pos += used;
                    if !done {
                        return if last { Err(PackError::Truncated) } else { Ok(pos) };
                    }
                    let State::Inflate(inf) = std::mem::replace(&mut self.state, State::Entry) else { unreachable!() };
                    if inf.out.len() != inf.expected {
                        return Err(PackError::Zlib(inf.offset));
                    }
                    self.inflated += inf.out.len() as u64;
                    if self.inflated > self.limits.max_unpacked_bytes {
                        return Err(PackError::UnpackedTooLarge(self.limits.max_unpacked_bytes));
                    }
                    self.by_offset.insert(inf.offset, self.entries.len());
                    self.entries.push(Entry { kind: inf.kind, data: inf.out });
                }
                State::Done => {
                    if avail > pos {
                        return Err(PackError::TrailingData);
                    }
                    return Ok(pos);
                }
            }
        }
    }

    /// An entry's header (and delta base), if all of it is in `b`.
    fn entry_start(&self, b: &[u8], offset: usize) -> Result<Start, PackError> {
        let bad = PackError::BadEntry(offset);
        let Some((ty, size, mut n)) = entry_header(b).map_err(|()| PackError::BadEntry(offset))? else { return Ok(Start::More) };
        let kind = match ty {
            1..=4 => EntryKind::Base(Kind::from_pack_type(ty).unwrap()),
            6 => {
                let Some((back, m)) = ofs_offset(&b[n..]).map_err(|()| PackError::BadEntry(offset))? else { return Ok(Start::More) };
                n += m;
                let base = offset.checked_sub(back).filter(|_| back > 0).ok_or(PackError::BadEntry(offset))?;
                EntryKind::Ofs(*self.by_offset.get(&base).ok_or(bad)?)
            }
            7 => {
                let Some(id) = b.get(n..n + 20) else { return Ok(Start::More) };
                n += 20;
                EntryKind::Ref(ObjectId::from_bytes(id).unwrap())
            }
            _ => return Err(bad),
        };
        Ok(Start::Entry(kind, size, n))
    }
}

impl Inflating {
    /// Inflates from `input`; returns the bytes consumed and whether the stream ended. Capacity
    /// grows with output actually produced, charged first.
    fn step(&mut self, input: &[u8], meter: &mut dyn Meter) -> Result<(usize, bool), PackError> {
        let limit = self.expected + 1; // one spare byte detects streams longer than declared
        let start = self.z.total_in();
        loop {
            if self.out.len() == self.out.capacity() {
                let grow = self.out.capacity().max(8192).min(limit - self.out.len());
                if grow == 0 {
                    return Err(PackError::Zlib(self.offset));
                }
                meter.charge(grow as u64)?;
                self.out.reserve_exact(grow);
            }
            let used = (self.z.total_in() - start) as usize;
            let (in_before, out_before) = (self.z.total_in(), self.z.total_out());
            let status = self
                .z
                .decompress_vec(&input[used..], &mut self.out, FlushDecompress::None)
                .map_err(|_| PackError::Zlib(self.offset))?;
            if self.out.len() > self.expected {
                return Err(PackError::Zlib(self.offset));
            }
            let used = (self.z.total_in() - start) as usize;
            match status {
                flate2::Status::StreamEnd => return Ok((used, true)),
                _ if self.z.total_in() == in_before && self.z.total_out() == out_before => {
                    return if used >= input.len() { Ok((used, false)) } else { Err(PackError::Zlib(self.offset)) };
                }
                _ => {}
            }
        }
    }
}

/// `(type, size, header length)` of an entry header; `Ok(None)` if `b` ends inside it.
fn entry_header(b: &[u8]) -> Result<Option<(u8, u64, usize)>, ()> {
    let Some(&first) = b.first() else { return Ok(None) };
    let ty = (first >> 4) & 7;
    let mut size = (first & 15) as u64;
    let mut shift = 4;
    let mut i = 1;
    let mut c = first;
    while c & 0x80 != 0 {
        let Some(&next) = b.get(i) else { return Ok(None) };
        c = next;
        if shift > 57 {
            return Err(());
        }
        size |= ((c & 0x7f) as u64) << shift;
        shift += 7;
        i += 1;
    }
    Ok(Some((ty, size, i)))
}

/// The backwards offset of an OFS_DELTA entry; `Ok(None)` if `b` ends inside it.
fn ofs_offset(b: &[u8]) -> Result<Option<(usize, usize)>, ()> {
    let Some(&first) = b.first() else { return Ok(None) };
    let mut c = first;
    let mut off = (c & 0x7f) as u64;
    let mut i = 1;
    while c & 0x80 != 0 {
        let Some(&next) = b.get(i) else { return Ok(None) };
        c = next;
        off = off.checked_add(1).and_then(|o| o.checked_mul(128)).ok_or(())? | (c & 0x7f) as u64;
        i += 1;
        if i > 9 {
            return Err(());
        }
    }
    Ok(Some((usize::try_from(off).map_err(|_| ())?, i)))
}

/// Applies a git delta to `base`. The result is capped at `max` bytes.
pub fn apply_delta(base: &[u8], delta: &[u8], max: u64) -> Result<Vec<u8>, PackError> {
    apply_delta_metered(base, delta, max, &mut ())
}

/// [`apply_delta`], charging the result's capacity to `meter` before it grows.
fn apply_delta_metered(base: &[u8], delta: &[u8], max: u64, meter: &mut dyn Meter) -> Result<Vec<u8>, PackError> {
    fn varint(d: &[u8], i: &mut usize) -> Result<u64, PackError> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let c = *d.get(*i).ok_or(PackError::BadDelta)?;
            *i += 1;
            if shift > 63 {
                return Err(PackError::BadDelta);
            }
            v |= ((c & 0x7f) as u64) << shift;
            shift += 7;
            if c & 0x80 == 0 {
                return Ok(v);
            }
        }
    }
    let mut i = 0;
    let src_size = varint(delta, &mut i)?;
    let dst_size = varint(delta, &mut i)?;
    if src_size != base.len() as u64 {
        return Err(PackError::BadDelta);
    }
    if dst_size > max {
        return Err(PackError::ObjectTooLarge(max));
    }
    let dst_size = dst_size as usize;
    // Copies repeat base data, so the result can outgrow both inputs; start from what's present
    // and grow (charged) as the copies need it.
    let mut out: Vec<u8> = Vec::new();
    let put = |out: &mut Vec<u8>, src: &[u8], meter: &mut dyn Meter| -> Result<(), PackError> {
        let need = out.len() + src.len();
        if need > dst_size {
            return Err(PackError::BadDelta);
        }
        if need > out.capacity() {
            let cap = need.max(out.capacity() * 2).max(dst_size.min(base.len() + delta.len())).min(dst_size);
            meter.charge((cap - out.capacity()) as u64)?;
            out.reserve_exact(cap - out.len());
        }
        out.extend_from_slice(src);
        Ok(())
    };
    while i < delta.len() {
        let op = delta[i];
        i += 1;
        if op & 0x80 != 0 {
            let mut off = 0usize;
            let mut len = 0usize;
            for bit in 0..4 {
                if op & (1 << bit) != 0 {
                    off |= (*delta.get(i).ok_or(PackError::BadDelta)? as usize) << (8 * bit);
                    i += 1;
                }
            }
            for bit in 0..3 {
                if op & (0x10 << bit) != 0 {
                    len |= (*delta.get(i).ok_or(PackError::BadDelta)? as usize) << (8 * bit);
                    i += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            let src = base.get(off..off.checked_add(len).ok_or(PackError::BadDelta)?).ok_or(PackError::BadDelta)?;
            put(&mut out, src, meter)?;
        } else if op != 0 {
            let n = op as usize;
            let src = delta.get(i..i + n).ok_or(PackError::BadDelta)?;
            i += n;
            put(&mut out, src, meter)?;
        } else {
            return Err(PackError::BadDelta);
        }
    }
    if out.len() != dst_size {
        return Err(PackError::BadDelta);
    }
    Ok(out)
}

/// Resolves deltas in a parsed pack. Bases outside the pack (thin packs) are requested through
/// [`Resolver::missing`] and supplied with [`Resolver::add_external`].
pub struct Resolver {
    entries: Vec<Entry>,
    limits: Limits,
    unpacked: u64,
    resolved: Vec<Option<(Kind, Bytes, ObjectId)>>,
    by_id: HashMap<ObjectId, usize>,
    externals: HashMap<ObjectId, Object>,
    external_bytes: u64,
    /// Delta entries waiting for a base, by base entry index / base id.
    wait_ofs: HashMap<usize, Vec<usize>>,
    wait_ref: HashMap<ObjectId, Vec<usize>>,
    queue: Vec<usize>,
    started: bool,
    meter: Box<dyn Meter>,
}

impl Resolver {
    pub fn new(pack: Pack) -> Self {
        let n = pack.entries.len();
        let mut r = Resolver {
            limits: pack.limits,
            unpacked: pack.inflated,
            resolved: (0..n).map(|_| None).collect(),
            entries: pack.entries,
            by_id: HashMap::new(),
            externals: HashMap::new(),
            external_bytes: 0,
            wait_ofs: HashMap::new(),
            wait_ref: HashMap::new(),
            queue: Vec::new(),
            started: false,
            meter: pack.meter,
        };
        for (i, e) in r.entries.iter().enumerate() {
            match e.kind {
                EntryKind::Base(_) => r.queue.push(i),
                EntryKind::Ofs(b) => r.wait_ofs.entry(b).or_default().push(i),
                EntryKind::Ref(id) => r.wait_ref.entry(id).or_default().push(i),
            }
        }
        r
    }

    /// Resolves everything resolvable with the bases at hand.
    pub fn run(&mut self) -> Result<(), PackError> {
        if !self.started {
            self.started = true;
            for i in std::mem::take(&mut self.queue) {
                let e = &mut self.entries[i];
                let EntryKind::Base(kind) = e.kind else { unreachable!() };
                let data = Bytes::from(std::mem::take(&mut e.data));
                let id = ObjectId::hash(kind, &data);
                self.by_id.entry(id).or_insert(i);
                self.resolved[i] = Some((kind, data, id));
                self.queue.push(i);
            }
        }
        // Children of a resolved object: OFS deltas on its entry, REF deltas on its id.
        let mut external_ready: Vec<ObjectId> = self.externals.keys().filter(|id| self.wait_ref.contains_key(id)).copied().collect();
        loop {
            let (base_kind, base_data, children) = if let Some(i) = self.queue.pop() {
                let (kind, data, id) = self.resolved[i].clone().unwrap();
                let mut children = self.wait_ofs.remove(&i).unwrap_or_default();
                children.extend(self.wait_ref.remove(&id).unwrap_or_default());
                (kind, data, children)
            } else if let Some(id) = external_ready.pop() {
                let o = &self.externals[&id];
                (o.kind, o.data.clone(), self.wait_ref.remove(&id).unwrap_or_default())
            } else {
                break;
            };
            for c in children {
                let delta = std::mem::take(&mut self.entries[c].data);
                let out = apply_delta_metered(&base_data, &delta, self.limits.max_object_bytes, self.meter.as_mut())?;
                self.meter.release(delta.capacity() as u64);
                drop(delta);
                self.unpacked += out.len() as u64;
                if self.unpacked > self.limits.max_unpacked_bytes {
                    return Err(PackError::UnpackedTooLarge(self.limits.max_unpacked_bytes));
                }
                let id = ObjectId::hash(base_kind, &out);
                self.by_id.entry(id).or_insert(c);
                self.resolved[c] = Some((base_kind, Bytes::from(out), id));
                self.queue.push(c);
            }
        }
        Ok(())
    }

    /// Base ids that neither the pack nor the externals supplied.
    pub fn missing(&self) -> Vec<ObjectId> {
        self.wait_ref.keys().filter(|id| !self.externals.contains_key(id)).copied().collect()
    }

    /// A base from outside the pack, charged to the meter until [`Resolver::finish`].
    pub fn add_external(&mut self, object: Object) -> Result<(), PackError> {
        let bytes = object.data.len() as u64 + ENTRY_OVERHEAD;
        self.meter.charge(bytes)?;
        self.external_bytes += bytes;
        if let Some(old) = self.externals.insert(object.id, object) {
            let b = old.data.len() as u64 + ENTRY_OVERHEAD;
            self.external_bytes -= b;
            self.meter.release(b);
        }
        Ok(())
    }

    /// Every object in the pack, in pack order. Externals are not included.
    pub fn finish(self) -> Result<Vec<Object>, PackError> {
        self.finish_metered().map(|(objects, _)| objects)
    }

    /// [`Resolver::finish`], and the meter, which still accounts for the objects returned.
    pub fn finish_metered(mut self) -> Result<(Vec<Object>, Box<dyn Meter>), PackError> {
        if let Some(id) = self.missing().first() {
            return Err(PackError::MissingBase(*id));
        }
        if !self.wait_ofs.is_empty() {
            return Err(PackError::BadDelta);
        }
        self.externals.clear();
        self.meter.release(std::mem::take(&mut self.external_bytes));
        let mut out = Vec::with_capacity(self.resolved.len());
        for r in self.resolved.drain(..) {
            let (kind, data, id) = r.ok_or(PackError::BadDelta)?;
            out.push(Object { id, kind, data });
        }
        Ok((out, self.meter))
    }
}

/// Parses and resolves a self-contained pack.
pub fn read(data: &[u8], limits: &Limits) -> Result<Vec<Object>, PackError> {
    let mut r = Resolver::new(parse(data, limits)?);
    r.run()?;
    r.finish()
}

/// Resolves a parsed pack on the current (blocking) thread, fetching bases outside it (thin
/// packs) with `lookup`. Returns the objects and the meter that accounts for them.
pub fn resolve_with<F>(pack: Pack, mut lookup: F) -> anyhow::Result<(Vec<Object>, Box<dyn Meter>)>
where
    F: FnMut(ObjectId) -> anyhow::Result<Option<Object>>,
{
    let mut r = Resolver::new(pack);
    r.run()?;
    loop {
        let missing = r.missing();
        if missing.is_empty() {
            break;
        }
        let mut found = false;
        for id in missing {
            if let Some(o) = lookup(id)? {
                found = true;
                r.add_external(o)?;
            }
        }
        if !found {
            break;
        }
        r.run()?;
    }
    Ok(r.finish_metered()?)
}

/// Parses and resolves a pack, fetching bases outside it (thin packs) with `lookup`. CPU work
/// runs on the blocking pool.
pub async fn read_with<F, Fut>(data: Bytes, limits: Limits, mut lookup: F) -> Result<Vec<Object>, anyhow::Error>
where
    F: FnMut(ObjectId) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Option<Object>>>,
{
    let mut r = tokio::task::spawn_blocking(move || -> Result<Resolver, PackError> {
        let mut r = Resolver::new(parse(&data, &limits)?);
        r.run()?;
        Ok(r)
    })
    .await??;
    loop {
        let missing = r.missing();
        if missing.is_empty() {
            break;
        }
        let mut found = false;
        for id in missing {
            if let Some(o) = lookup(id).await? {
                found = true;
                r.add_external(o)?;
            }
        }
        if !found {
            break;
        }
        r = tokio::task::spawn_blocking(move || -> Result<Resolver, PackError> {
            r.run()?;
            Ok(r)
        })
        .await??;
    }
    Ok(tokio::task::spawn_blocking(move || r.finish()).await??)
}

/// The most bytes [`Writer::entry`] produces for `len` bytes of data (a header and zlib's
/// worst case), so callers can account for an entry before writing it.
pub fn entry_bound(len: usize) -> usize {
    16 + len + (len >> 12) + (len >> 14) + (len >> 25) + 13
}

/// Writes a pack entry by entry, so it can be streamed.
pub struct Writer {
    hasher: Sha1,
    level: Compression,
}

impl Writer {
    /// Starts a pack of `count` objects; returns the writer and the 12-byte header.
    pub fn new(count: u32) -> (Self, Vec<u8>) {
        let mut header = Vec::with_capacity(12);
        header.extend_from_slice(b"PACK");
        header.extend_from_slice(&2u32.to_be_bytes());
        header.extend_from_slice(&count.to_be_bytes());
        let mut hasher = Sha1::new();
        hasher.update(&header);
        (Writer { hasher, level: Compression::fast() }, header)
    }

    /// One undeltified entry.
    pub fn entry(&mut self, kind: Kind, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(entry_bound(data.len()));
        let mut size = data.len() as u64;
        let mut c = (kind.pack_type() << 4) | (size & 15) as u8;
        size >>= 4;
        while size > 0 {
            out.push(c | 0x80);
            c = (size & 0x7f) as u8;
            size >>= 7;
        }
        out.push(c);
        let mut z = ZlibEncoder::new(out, self.level);
        z.write_all(data).expect("write to Vec");
        let out = z.finish().expect("write to Vec");
        self.hasher.update(&out);
        out
    }

    /// The trailing checksum.
    pub fn finish(self) -> [u8; 20] {
        self.hasher.finalize().into()
    }
}

/// A whole pack in memory.
pub fn write(objects: &[Object]) -> Vec<u8> {
    let (mut w, mut out) = Writer::new(objects.len() as u32);
    for o in objects {
        out.extend_from_slice(&w.entry(o.kind, &o.data));
    }
    out.extend_from_slice(&w.finish());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read() {
        let objs = vec![Object::new(Kind::Blob, "hello\n"), Object::new(Kind::Blob, vec![1u8; 100_000]), Object::new(Kind::Tree, "")];
        let pack = write(&objs);
        assert_eq!(read(&pack, &Limits::default()).unwrap(), objs);
    }

    #[test]
    fn delta_ops() {
        let base = b"hello world";
        // src 11, dst 16: copy 0..5, insert " big", copy 5..11 ... too short; build by hand
        let delta = [11u8, 15, 0x91, 0, 5, 4, b' ', b'b', b'i', b'g', 0x91, 5, 6];
        assert_eq!(apply_delta(base, &delta, 1 << 20).unwrap(), b"hello big world");
        assert!(apply_delta(base, &delta, 10).is_err());
        assert!(apply_delta(b"short", &delta, 1 << 20).is_err());
        assert!(apply_delta(base, &[11, 15, 0], 1 << 20).is_err());
    }
}
