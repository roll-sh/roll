//! The garbage collector's memory of manifests, persisted (`search/gc/marks.bin`).
//!
//! A pass needs what every retained manifest in the bucket references. Manifests are immutable
//! and their keys are never reused, so what a pass learned from a key stays true for as long as
//! the key exists; the collector keeps it in memory and only reads manifests it hasn't seen. A
//! restarted collector would otherwise read every manifest again (millions of GETs in a bucket
//! with many tenants), so after a pass that learned or forgot enough it writes its memory here,
//! and the first pass of a new process starts from it. Entries for keys no longer listed are
//! dropped by the listing, exactly as for the in-memory copy; a missing, damaged or foreign
//! snapshot only costs the reads it would have saved.
//!
//! Format: `"RSGM"`, a format version byte, the CRC32C of the rest, then a zstd frame of
//! `count` entries, each `key, written_ms, head?, segments[(id, live?)], wal sources`, with
//! unsigned LEB128 varints for counts and numbers and length-prefixed strings. Decoding never
//! sizes an allocation from a count it read beyond what the bytes present can hold.

use super::Marks;
use crate::keys::{LiveGen, SegmentId};
use crate::manifest::WalHead;
use crate::wal::WalSource;
use std::sync::Arc;

const MAGIC: &[u8; 4] = b"RSGM";
const VERSION: u8 = 1;
const HEADER: usize = 4 + 1 + 4;

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

fn put_opt(out: &mut Vec<u8>, v: Option<u64>) {
    match v {
        // 0 means none, so a present value is stored plus one (u64::MAX can't be: file seqs
        // and times are far below it, and `to` is never MAX in a stored source).
        Some(v) => put_varint(out, v.saturating_add(1)),
        None => put_varint(out, 0),
    }
}

/// Serialize the collector's memory.
pub(crate) fn encode(entries: &[(String, Arc<Marks>)], level: i32) -> anyhow::Result<Vec<u8>> {
    let mut body = Vec::with_capacity(entries.len() * 128);
    put_varint(&mut body, entries.len() as u64);
    for (key, m) in entries {
        put_str(&mut body, key);
        put_varint(&mut body, m.written_ms);
        match &m.head {
            Some(h) => {
                body.push(1);
                put_str(&mut body, &h.node);
                put_varint(&mut body, h.file);
            }
            None => body.push(0),
        }
        put_varint(&mut body, m.segments.len() as u64);
        for (id, live) in m.segments.iter() {
            body.extend_from_slice(&id.0);
            match live {
                Some(g) => {
                    body.push(1);
                    body.extend_from_slice(&g.0.to_le_bytes());
                }
                None => body.push(0),
            }
        }
        put_varint(&mut body, m.wal.len() as u64);
        for s in m.wal.iter() {
            put_str(&mut body, &s.node);
            put_str(&mut body, &s.cluster);
            put_varint(&mut body, s.from);
            put_opt(&mut body, s.to);
            put_opt(&mut body, s.until_ms);
            // seq_nos are >= -1 (NO_OPS_PERFORMED); shift so they fit an unsigned varint.
            put_opt(&mut body, s.max_seq_no.map(|x| x.saturating_add(1).max(0) as u64));
            put_varint(&mut body, s.skip.len() as u64);
            for &[a, b] in &s.skip {
                put_varint(&mut body, a);
                put_varint(&mut body, b);
            }
        }
    }
    let packed = zstd::bulk::compress(&body, level)?;
    let mut out = Vec::with_capacity(HEADER + packed.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&crc32c::crc32c(&packed).to_le_bytes());
    out.extend_from_slice(&packed);
    Ok(out)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            if shift == 63 && b > 1 {
                return None;
            }
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Some(v);
            }
        }
        None
    }

    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        if n > self.remaining() {
            return None;
        }
        let b = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Some(b)
    }

    fn byte(&mut self) -> Option<u8> {
        self.bytes(1).map(|b| b[0])
    }

    fn str(&mut self) -> Option<&'a str> {
        let n = usize::try_from(self.varint()?).ok()?;
        std::str::from_utf8(self.bytes(n)?).ok()
    }

    fn opt(&mut self) -> Option<Option<u64>> {
        Some(self.varint()?.checked_sub(1))
    }

    /// A count of items at least `min_bytes` each: refused when the rest can't hold them.
    fn count(&mut self, min_bytes: usize) -> Option<usize> {
        let n = usize::try_from(self.varint()?).ok()?;
        (n <= self.remaining() / min_bytes.max(1)).then_some(n)
    }
}

/// Parse a snapshot. Errors describe what is wrong; callers fall back to reading manifests.
pub(crate) fn decode(data: &[u8]) -> anyhow::Result<Vec<(String, Arc<Marks>)>> {
    anyhow::ensure!(data.len() >= HEADER && &data[..4] == MAGIC, "not a marks snapshot");
    anyhow::ensure!(data[4] == VERSION, "marks snapshot format {} is not supported", data[4]);
    let crc = u32::from_le_bytes(data[5..9].try_into().expect("4 bytes"));
    let packed = &data[HEADER..];
    anyhow::ensure!(crc32c::crc32c(packed) == crc, "marks snapshot checksum mismatch");
    // Streaming: the buffer grows with the data actually decompressed, not a declared size.
    let body = zstd::stream::decode_all(packed)?;
    parse_body(&body).ok_or_else(|| anyhow::anyhow!("marks snapshot is malformed"))
}

fn parse_body(body: &[u8]) -> Option<Vec<(String, Arc<Marks>)>> {
    let mut r = Reader { buf: body, pos: 0 };
    // key len + written_ms + head flag + 2 counts: at least 5 bytes an entry.
    let n = r.count(5)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let key = r.str()?.to_string();
        let written_ms = r.varint()?;
        let head = match r.byte()? {
            0 => None,
            1 => Some(WalHead { node: r.str()?.to_string(), file: r.varint()? }),
            _ => return None,
        };
        let nseg = r.count(17)?;
        let mut segments = Vec::with_capacity(nseg);
        for _ in 0..nseg {
            let id = SegmentId(r.bytes(16)?.try_into().ok()?);
            let live = match r.byte()? {
                0 => None,
                1 => Some(LiveGen(u128::from_le_bytes(r.bytes(16)?.try_into().ok()?))),
                _ => return None,
            };
            segments.push((id, live));
        }
        // node, cluster, from, to, until, max_seq_no, skip count: at least 7 bytes.
        let nwal = r.count(7)?;
        let mut wal = Vec::with_capacity(nwal);
        for _ in 0..nwal {
            let node = r.str()?.to_string();
            let cluster = r.str()?.to_string();
            let from = r.varint()?;
            let to = r.opt()?;
            let until_ms = r.opt()?;
            let max_seq_no = match r.opt()? {
                Some(x) => Some(i64::try_from(x).ok()? - 1),
                None => None,
            };
            let nskip = r.count(2)?;
            let mut skip = Vec::with_capacity(nskip);
            for _ in 0..nskip {
                skip.push([r.varint()?, r.varint()?]);
            }
            // `exact` only matters to replay, which reads manifests, not this snapshot.
            wal.push(WalSource { node, cluster, from, to, until_ms, max_seq_no, skip, exact: false });
        }
        out.push((key, Arc::new(Marks { segments: segments.into_boxed_slice(), wal: wal.into_boxed_slice(), head, written_ms })));
    }
    (r.remaining() == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<(String, Arc<Marks>)> {
        let a = Marks {
            segments: vec![(SegmentId([7; 16]), None), (SegmentId([9; 16]), Some(LiveGen(u128::MAX - 3)))].into_boxed_slice(),
            wal: vec![
                WalSource { to: Some(12), until_ms: Some(1_700_000_000_000), max_seq_no: Some(-1), skip: vec![[3, 4], [9, 9]], ..WalSource::open("node-a", "c1", 2) },
                WalSource { max_seq_no: Some(i64::MAX - 1), ..WalSource::open("n", "c1", 0) },
                WalSource::open("node-b", "c1", 40),
            ]
            .into_boxed_slice(),
            head: Some(WalHead { node: "node-a".into(), file: 13 }),
            written_ms: 1_700_000_000_123,
        };
        let b = Marks { segments: Box::new([]), wal: Box::new([]), head: None, written_ms: 0 };
        vec![("search/c/c1/i/u/0/a.json".into(), Arc::new(a)), ("search/c/c2/i/é/3/b.json".into(), Arc::new(b))]
    }

    fn same(a: &[(String, Arc<Marks>)], b: &[(String, Arc<Marks>)]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|((ka, ma), (kb, mb))| {
                ka == kb && ma.segments == mb.segments && ma.wal == mb.wal && ma.head == mb.head && ma.written_ms == mb.written_ms
            })
    }

    #[test]
    fn round_trip() {
        let entries = sample();
        let data = encode(&entries, 3).unwrap();
        assert!(same(&decode(&data).unwrap(), &entries));
        assert!(decode(&encode(&[], 1).unwrap()).unwrap().is_empty());
    }

    #[test]
    fn damage_is_detected() {
        let data = encode(&sample(), 3).unwrap();
        assert!(decode(&data[..data.len() - 1]).is_err(), "truncated");
        let mut flipped = data.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(decode(&flipped).is_err(), "bit flip");
        let mut future = data.clone();
        future[4] = 9;
        assert!(decode(&future).is_err(), "unknown format");
        assert!(decode(b"RSG").is_err());
        assert!(decode(b"").is_err());
    }

    #[test]
    fn hostile_counts_and_lengths_do_not_allocate() {
        // A body claiming 2^60 entries, or a string of 2^60 bytes, is refused before allocating.
        let mut body = Vec::new();
        put_varint(&mut body, 1 << 60);
        assert!(parse_body(&body).is_none());
        let mut body = Vec::new();
        put_varint(&mut body, 1);
        put_varint(&mut body, 1 << 60);
        body.extend_from_slice(&[0; 8]);
        assert!(parse_body(&body).is_none());
        // Trailing bytes and overlong varints are malformed too.
        let mut body = Vec::new();
        put_varint(&mut body, 0);
        body.push(0);
        assert!(parse_body(&body).is_none());
        assert!(parse_body(&[0xff; 11]).is_none());
    }
}
