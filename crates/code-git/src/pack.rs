//! Pack files (version 2): reading, with OFS_DELTA and REF_DELTA resolution, and writing.
//!
//! Reading never sizes an allocation from a count or length the peer sent. Buffers grow with
//! the bytes actually inflated, and every object, the pack and the total unpacked size are
//! capped by [`Limits`], so a small pack can't expand into a huge one (a "delta bomb").
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
}

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
#[derive(Debug)]
pub struct Pack {
    entries: Vec<Entry>,
    limits: Limits,
    inflated: u64,
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
    let count = u32::from_be_bytes(data[8..12].try_into().unwrap()) as usize;
    if data.len() < 32 {
        return Err(PackError::Truncated);
    }
    let end = data.len() - 20;
    if Sha1::digest(&data[..end])[..] != data[end..] {
        return Err(PackError::Checksum);
    }

    // Every entry takes at least a 1-byte header and a 2-byte zlib stream; don't trust `count`.
    let mut entries = Vec::with_capacity(count.min((end - 12) / 3));
    let mut by_offset: HashMap<usize, usize> = HashMap::new();
    let mut pos = 12;
    let mut inflated = 0u64;
    for _ in 0..count {
        if pos >= end {
            return Err(PackError::Truncated);
        }
        let offset = pos;
        let (ty, size, n) = entry_header(&data[pos..end]).ok_or(PackError::BadEntry(offset))?;
        pos += n;
        let kind = match ty {
            1..=4 => EntryKind::Base(Kind::from_pack_type(ty).unwrap()),
            6 => {
                let (back, n) = ofs_offset(&data[pos..end]).ok_or(PackError::BadEntry(offset))?;
                pos += n;
                let base = offset.checked_sub(back).filter(|_| back > 0).ok_or(PackError::BadEntry(offset))?;
                EntryKind::Ofs(*by_offset.get(&base).ok_or(PackError::BadEntry(offset))?)
            }
            7 => {
                let id = data.get(pos..pos + 20).filter(|_| pos + 20 <= end).ok_or(PackError::Truncated)?;
                pos += 20;
                EntryKind::Ref(ObjectId::from_bytes(id).unwrap())
            }
            _ => return Err(PackError::BadEntry(offset)),
        };
        // Deltas are bounded like objects: a delta never needs to be larger than its result.
        if size > limits.max_object_bytes {
            return Err(PackError::ObjectTooLarge(limits.max_object_bytes));
        }
        let (content, used) = inflate(&data[pos..end], size as usize).map_err(|e| match e {
            InflateError::Truncated => PackError::Truncated,
            InflateError::Corrupt => PackError::Zlib(offset),
        })?;
        pos += used;
        inflated += content.len() as u64;
        if inflated > limits.max_unpacked_bytes {
            return Err(PackError::UnpackedTooLarge(limits.max_unpacked_bytes));
        }
        by_offset.insert(offset, entries.len());
        entries.push(Entry { kind, data: content });
    }
    if pos != end {
        return Err(PackError::TrailingData);
    }
    Ok(Pack { entries, limits: *limits, inflated })
}

/// `(type, size, header length)` of an entry header.
fn entry_header(b: &[u8]) -> Option<(u8, u64, usize)> {
    let first = *b.first()?;
    let ty = (first >> 4) & 7;
    let mut size = (first & 15) as u64;
    let mut shift = 4;
    let mut i = 1;
    let mut c = first;
    while c & 0x80 != 0 {
        c = *b.get(i)?;
        if shift > 57 {
            return None;
        }
        size |= ((c & 0x7f) as u64) << shift;
        shift += 7;
        i += 1;
    }
    Some((ty, size, i))
}

/// The backwards offset of an OFS_DELTA entry.
fn ofs_offset(b: &[u8]) -> Option<(usize, usize)> {
    let mut c = *b.first()?;
    let mut off = (c & 0x7f) as u64;
    let mut i = 1;
    while c & 0x80 != 0 {
        c = *b.get(i)?;
        off = off.checked_add(1)?.checked_mul(128)? | (c & 0x7f) as u64;
        i += 1;
        if i > 9 {
            return None;
        }
    }
    Some((usize::try_from(off).ok()?, i))
}

enum InflateError {
    Truncated,
    Corrupt,
}

/// Inflates one zlib stream that must produce exactly `expected` bytes. Returns the bytes and
/// the compressed length consumed. Capacity grows with output actually produced.
fn inflate(input: &[u8], expected: usize) -> Result<(Vec<u8>, usize), InflateError> {
    let mut d = Decompress::new(true);
    let limit = expected + 1; // one spare byte detects streams longer than declared
    let mut out: Vec<u8> = Vec::with_capacity(limit.min(8192));
    loop {
        if out.len() == out.capacity() {
            let grow = out.capacity().max(8192).min(limit - out.len());
            if grow == 0 {
                return Err(InflateError::Corrupt);
            }
            out.reserve_exact(grow);
        }
        let (in_before, out_before) = (d.total_in(), d.total_out());
        let status = d
            .decompress_vec(&input[d.total_in() as usize..], &mut out, FlushDecompress::None)
            .map_err(|_| InflateError::Corrupt)?;
        if out.len() > expected {
            return Err(InflateError::Corrupt);
        }
        match status {
            flate2::Status::StreamEnd => break,
            _ if d.total_in() == in_before && d.total_out() == out_before => {
                return Err(if d.total_in() as usize >= input.len() { InflateError::Truncated } else { InflateError::Corrupt });
            }
            _ => {}
        }
    }
    if out.len() != expected {
        return Err(InflateError::Corrupt);
    }
    Ok((out, d.total_in() as usize))
}

/// Applies a git delta to `base`. The result is capped at `max` bytes.
pub fn apply_delta(base: &[u8], delta: &[u8], max: u64) -> Result<Vec<u8>, PackError> {
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
    // Copies repeat base data, so the result can outgrow both inputs; start from what's present.
    let mut out = Vec::with_capacity(dst_size.min(base.len() + delta.len()));
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
            if out.len() + src.len() > dst_size {
                return Err(PackError::BadDelta);
            }
            out.extend_from_slice(src);
        } else if op != 0 {
            let n = op as usize;
            let src = delta.get(i..i + n).ok_or(PackError::BadDelta)?;
            i += n;
            if out.len() + n > dst_size {
                return Err(PackError::BadDelta);
            }
            out.extend_from_slice(src);
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
    /// Delta entries waiting for a base, by base entry index / base id.
    wait_ofs: HashMap<usize, Vec<usize>>,
    wait_ref: HashMap<ObjectId, Vec<usize>>,
    queue: Vec<usize>,
    started: bool,
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
            wait_ofs: HashMap::new(),
            wait_ref: HashMap::new(),
            queue: Vec::new(),
            started: false,
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
                let out = apply_delta(&base_data, &delta, self.limits.max_object_bytes)?;
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

    pub fn add_external(&mut self, object: Object) {
        self.externals.insert(object.id, object);
    }

    /// Every object in the pack, in pack order. Externals are not included.
    pub fn finish(mut self) -> Result<Vec<Object>, PackError> {
        if let Some(id) = self.missing().first() {
            return Err(PackError::MissingBase(*id));
        }
        if !self.wait_ofs.is_empty() {
            return Err(PackError::BadDelta);
        }
        let mut out = Vec::with_capacity(self.resolved.len());
        for r in self.resolved.drain(..) {
            let (kind, data, id) = r.ok_or(PackError::BadDelta)?;
            out.push(Object { id, kind, data });
        }
        Ok(out)
    }
}

/// Parses and resolves a self-contained pack.
pub fn read(data: &[u8], limits: &Limits) -> Result<Vec<Object>, PackError> {
    let mut r = Resolver::new(parse(data, limits)?);
    r.run()?;
    r.finish()
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
                r.add_external(o);
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
        let mut out = Vec::with_capacity(16 + data.len() / 2);
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
