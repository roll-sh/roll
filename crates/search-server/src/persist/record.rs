//! The WAL record of one shard commit cut.
//!
//! search-index's [`Shard::flush`](search_index::Shard::flush) returns a consistent cut of a
//! shard (every segment, its live docs, the max seq no) without publishing anything to
//! searchers. A cut record describes such a cut relative to what the shard's log already holds:
//! it lists every segment of the cut, carries the bytes only of segments the log doesn't have
//! yet (new since the last commit point or the last record), and the deletions only of segments
//! whose deletions changed — as a delta of newly deleted words, or whole when that is smaller.
//! Replaying the records past a commit point in log order rebuilds the latest acknowledged cut
//! exactly: same segments (same ids and bytes), same live docs, same seq nos and versions.
//!
//! ```text
//! magic 0xC7 | version 1 | max_seq_no i64 | primary_term i64 | numbers_len u32 | numbers JSON |
//! segment count u32 | per segment:
//!   id [16] | max_doc u32 | del_count u32 | flags u8 (1 = bytes, 2 = live whole, 4 = live delta)
//!   [bytes: len u64 | bytes] [live whole: len u32 | codec live-docs bytes]
//!   [live delta: count u32 | (word index u32, newly deleted bits u64) × count]
//! ```
//!
//! All integers are little-endian. Decoding borrows from the record and checks every length
//! against the bytes actually present before using it.

use anyhow::{Context, bail, ensure};

const MAGIC: u8 = 0xC7;
const VERSION: u8 = 1;

const F_BYTES: u8 = 1;
const F_LIVE_WHOLE: u8 = 2;
const F_LIVE_DELTA: u8 = 4;

/// How a segment's deletions appear in a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveChange<'a> {
    /// Same as in the log already.
    Unchanged,
    /// The codec's live-docs encoding (`LiveDocs::encode`).
    Whole(&'a [u8]),
    /// Words of the live-docs bitset with the bits of docs deleted since the log's last state.
    Delta(Vec<(u32, u64)>),
}

/// One segment of a cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutSegment<'a> {
    pub id: [u8; 16],
    pub max_doc: u32,
    pub del_count: u32,
    /// The segment file, when the log doesn't have it yet.
    pub data: Option<&'a [u8]>,
    pub live: LiveChange<'a>,
}

/// A decoded (or to be encoded) cut record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cut<'a> {
    pub max_seq_no: i64,
    pub primary_term: i64,
    /// The index's field numbering (JSON), when it changed since the log's last record.
    pub numbers: Option<&'a [u8]>,
    pub segments: Vec<CutSegment<'a>>,
}

impl Cut<'_> {
    /// Encoded size, without encoding.
    pub fn encoded_len(&self) -> usize {
        let mut n = 2 + 8 + 8 + 4 + self.numbers.map_or(0, <[u8]>::len) + 4;
        for s in &self.segments {
            n += 16 + 4 + 4 + 1;
            if let Some(d) = s.data {
                n += 8 + d.len();
            }
            n += match &s.live {
                LiveChange::Unchanged => 0,
                LiveChange::Whole(b) => 4 + b.len(),
                LiveChange::Delta(d) => 4 + d.len() * 12,
            };
        }
        n
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.push(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&self.max_seq_no.to_le_bytes());
        out.extend_from_slice(&self.primary_term.to_le_bytes());
        let numbers = self.numbers.unwrap_or(&[]);
        out.extend_from_slice(&(numbers.len() as u32).to_le_bytes());
        out.extend_from_slice(numbers);
        out.extend_from_slice(&(self.segments.len() as u32).to_le_bytes());
        for s in &self.segments {
            out.extend_from_slice(&s.id);
            out.extend_from_slice(&s.max_doc.to_le_bytes());
            out.extend_from_slice(&s.del_count.to_le_bytes());
            let mut flags = 0;
            if s.data.is_some() {
                flags |= F_BYTES;
            }
            flags |= match s.live {
                LiveChange::Unchanged => 0,
                LiveChange::Whole(_) => F_LIVE_WHOLE,
                LiveChange::Delta(_) => F_LIVE_DELTA,
            };
            out.push(flags);
            if let Some(d) = s.data {
                out.extend_from_slice(&(d.len() as u64).to_le_bytes());
                out.extend_from_slice(d);
            }
            match &s.live {
                LiveChange::Unchanged => {}
                LiveChange::Whole(b) => {
                    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                    out.extend_from_slice(b);
                }
                LiveChange::Delta(d) => {
                    out.extend_from_slice(&(d.len() as u32).to_le_bytes());
                    for (i, w) in d {
                        out.extend_from_slice(&i.to_le_bytes());
                        out.extend_from_slice(&w.to_le_bytes());
                    }
                }
            }
        }
        debug_assert_eq!(out.len(), self.encoded_len());
        out
    }
}

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        ensure!(n <= self.b.len(), "cut record truncated: {n} bytes needed, {} left", self.b.len());
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    fn u8(&mut self) -> anyhow::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> anyhow::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> anyhow::Result<i64> {
        Ok(self.u64()? as i64)
    }
    /// A count of items of `item` bytes each, checked against what is left.
    fn count(&mut self, item: usize) -> anyhow::Result<usize> {
        let n = self.u32()? as usize;
        ensure!(n.saturating_mul(item) <= self.b.len(), "cut record truncated: {n} items of {item} bytes announced, {} bytes left", self.b.len());
        Ok(n)
    }
}

/// Parses a cut record (borrowing segment bytes from it).
pub fn decode(b: &[u8]) -> anyhow::Result<Cut<'_>> {
    let mut r = Reader { b };
    let (magic, version) = (r.u8()?, r.u8()?);
    ensure!(magic == MAGIC, "not a cut record (magic {magic:#x})");
    if version != VERSION {
        bail!("unsupported cut record version {version}");
    }
    let max_seq_no = r.i64()?;
    let primary_term = r.i64()?;
    let nlen = r.u32()? as usize;
    let numbers = r.take(nlen)?;
    let numbers = (!numbers.is_empty()).then_some(numbers);
    // Each segment entry is at least 25 bytes.
    let n = r.count(25)?;
    let mut segments = Vec::with_capacity(n);
    for _ in 0..n {
        let id: [u8; 16] = r.take(16)?.try_into().unwrap();
        let max_doc = r.u32()?;
        let del_count = r.u32()?;
        let flags = r.u8()?;
        ensure!(flags & !(F_BYTES | F_LIVE_WHOLE | F_LIVE_DELTA) == 0, "cut record: unknown segment flags {flags:#x}");
        ensure!(flags & (F_LIVE_WHOLE | F_LIVE_DELTA) != (F_LIVE_WHOLE | F_LIVE_DELTA), "cut record: live docs both whole and delta");
        ensure!(del_count <= max_doc, "cut record: {del_count} deletions in a segment of {max_doc} docs");
        let data = if flags & F_BYTES != 0 {
            let len = r.u64()?;
            let len = usize::try_from(len).context("cut record: segment too large")?;
            Some(r.take(len)?)
        } else {
            None
        };
        let live = if flags & F_LIVE_WHOLE != 0 {
            let len = r.u32()? as usize;
            LiveChange::Whole(r.take(len)?)
        } else if flags & F_LIVE_DELTA != 0 {
            let words = (max_doc as usize).div_ceil(64);
            let k = r.count(12)?;
            let mut d = Vec::with_capacity(k);
            for _ in 0..k {
                let i = r.u32()?;
                let w = r.u64()?;
                ensure!((i as usize) < words, "cut record: live docs word {i} past the segment's {words}");
                d.push((i, w));
            }
            LiveChange::Delta(d)
        } else {
            LiveChange::Unchanged
        };
        segments.push(CutSegment { id, max_doc, del_count, data, live });
    }
    ensure!(r.b.is_empty(), "cut record has {} trailing bytes", r.b.len());
    Ok(Cut { max_seq_no, primary_term, numbers, segments })
}

/// The words of `now` with bits of docs deleted since `before` (live in `before`, not in
/// `now`). Both describe the same segment.
pub fn live_delta(before: &[u64], now: &[u64]) -> Vec<(u32, u64)> {
    before
        .iter()
        .zip(now)
        .enumerate()
        .filter_map(|(i, (b, n))| {
            let gone = b & !n;
            (gone != 0).then_some((i as u32, gone))
        })
        .collect()
}

/// Applies [`live_delta`] words to a live-docs bitset.
pub fn apply_delta(live: &mut search_codec::LiveDocs, delta: &[(u32, u64)]) {
    for &(i, mut w) in delta {
        while w != 0 {
            let b = w.trailing_zeros();
            live.delete(i * 64 + b);
            w &= w - 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_codec::LiveDocs;

    fn sample<'a>(seg: &'a [u8], live: &'a [u8]) -> Cut<'a> {
        Cut {
            max_seq_no: 42,
            primary_term: 3,
            numbers: Some(br#"[["title",16]]"#),
            segments: vec![
                CutSegment { id: [1; 16], max_doc: 10, del_count: 0, data: Some(seg), live: LiveChange::Unchanged },
                CutSegment { id: [2; 16], max_doc: 200, del_count: 2, data: None, live: LiveChange::Delta(vec![(0, 0b101), (3, 1 << 63)]) },
                CutSegment { id: [3; 16], max_doc: 5, del_count: 1, data: None, live: LiveChange::Whole(live) },
            ],
        }
    }

    #[test]
    fn round_trips() {
        let seg = vec![7u8; 1000];
        let live = vec![9u8; 50];
        let cut = sample(&seg, &live);
        let enc = cut.encode();
        assert_eq!(enc.len(), cut.encoded_len());
        assert_eq!(decode(&enc).unwrap(), cut);
        let empty = Cut { max_seq_no: -1, primary_term: 1, numbers: None, segments: vec![] };
        assert_eq!(decode(&empty.encode()).unwrap(), empty);
    }

    #[test]
    fn rejects_damage_without_overallocating() {
        let seg = vec![7u8; 1000];
        let live = vec![9u8; 50];
        let enc = sample(&seg, &live).encode();
        // Every truncation fails cleanly.
        for n in 0..enc.len() {
            assert!(decode(&enc[..n]).is_err(), "truncated at {n}");
        }
        let mut trailing = enc.clone();
        trailing.push(0);
        assert!(decode(&trailing).is_err());
        let mut bad = enc.clone();
        bad[0] = 0;
        assert!(decode(&bad).is_err());
        // A huge announced segment count with no bytes behind it.
        let mut huge = Cut { max_seq_no: 0, primary_term: 1, numbers: None, segments: vec![] }.encode();
        let at = huge.len() - 4;
        huge[at..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&huge).is_err());
    }

    #[test]
    fn live_deltas() {
        let mut before = LiveDocs::all_live(300);
        before.delete(5);
        let mut now = before.clone();
        for d in [0, 64, 65, 299] {
            now.delete(d);
        }
        let delta = live_delta(before.words(), now.words());
        assert_eq!(delta, vec![(0, 1), (1, 0b11), (4, 1 << (299 - 256))]);
        let mut rebuilt = before.clone();
        apply_delta(&mut rebuilt, &delta);
        assert_eq!(rebuilt, now);
        assert_eq!(rebuilt.num_deleted(), 5);
    }
}
