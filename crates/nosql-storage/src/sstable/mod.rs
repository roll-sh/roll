//! SSTables: our own format, designed for range reads from object storage.
//!
//! ```text
//! <id>.data    block*                        block := compressed bytes, crc32c (u32 LE)
//! <id>.index   zstd(index) crc32c "RNSI"      loaded whole when the SSTable opens
//! ```
//!
//! A **block** holds ~64 KiB (uncompressed) of entries and decompresses on its own, so a point
//! read costs one ranged GET. Entries are, per partition in token order: a partition header
//! (key, partition deletion, static row), rows and range tombstone markers in clustering order,
//! and an end marker. Blocks are cut between entries, so a wide partition spans several blocks;
//! such a *continuation* block's index entry records the first position in it, the deletion
//! open there and the partition deletion, so a slice read can start in the middle of a
//! partition without reading its beginning.
//!
//! The **index** has one entry per block (the first partition key in it, offsets, sizes), the
//! bloom filter over partition keys and the [`SstStats`]. It is sparse (O(blocks), not
//! O(partitions)): a lookup binary-searches the blocks, then scans one block. Everything is
//! checksummed; a corrupt or truncated object is an error, never a panic.

pub mod bloom;
pub mod cursor;
pub mod stats;
pub mod writer;

use crate::codec::{BytesReader, put_bound, put_bytes, put_deletion, put_uvarint};
use crate::compare::{ClusteringComparator, compare_bounds};
use crate::model::*;
use crate::tombstones::Marker;
use anyhow::{Context, Result, bail};
use bloom::Bloom;
use bytes::Bytes;
use futures::future::BoxFuture;
use std::cmp::Ordering;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
pub use stats::SstStats;

/// Block compression.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Codec {
    None,
    /// LZ4 (block format).
    Lz4,
    /// zstd level 1, the default. Measured on row data (`tests::sstable::bench_block_codecs`):
    /// it stores 2–3x less than LZ4 while decoding at ~80% of LZ4's speed (well over 1 GB/s),
    /// and decoded blocks are cached, so object storage bytes and GET sizes win.
    #[default]
    Zstd,
}

impl Codec {
    fn id(self) -> u8 {
        match self {
            Codec::None => 0,
            Codec::Lz4 => 1,
            Codec::Zstd => 2,
        }
    }

    fn from_id(id: u8) -> Result<Self> {
        Ok(match id {
            0 => Codec::None,
            1 => Codec::Lz4,
            2 => Codec::Zstd,
            _ => bail!("corrupt sstable: unknown codec {id}"),
        })
    }

    pub fn compress(self, raw: &[u8]) -> Result<Vec<u8>> {
        Ok(match self {
            Codec::None => raw.to_vec(),
            Codec::Lz4 => lz4_flex::block::compress(raw),
            Codec::Zstd => zstd::bulk::compress(raw, 1)?,
        })
    }

    pub fn decompress(self, data: &[u8], raw_len: usize) -> Result<Vec<u8>> {
        let out = match self {
            Codec::None => data.to_vec(),
            Codec::Lz4 => lz4_flex::block::decompress(data, raw_len).map_err(|e| anyhow::anyhow!("corrupt block: {e}"))?,
            Codec::Zstd => zstd::bulk::decompress(data, raw_len).context("corrupt block")?,
        };
        if out.len() != raw_len {
            bail!("corrupt block: {} bytes, expected {raw_len}", out.len());
        }
        Ok(out)
    }
}

/// Largest block accepted from an index (blocks are cut at ~64 KiB; one huge row can exceed
/// that, but not this).
const MAX_BLOCK: u32 = 256 << 20;
const INDEX_MAGIC: &[u8; 4] = b"RNSI";
const FORMAT: u8 = 1;

pub(crate) const E_PARTITION: u8 = 1;
pub(crate) const E_ROW: u8 = 2;
pub(crate) const E_MARKER: u8 = 3;
pub(crate) const E_END: u8 = 4;

/// Where a block that starts inside a partition picks up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Continuation {
    /// The position of the first entry (a row's clustering as its inclusive start).
    pub first: Bound,
    /// The range tombstone deletion open at the start of the block.
    pub open: Option<DeletionTime>,
    pub partition_deletion: DeletionTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockMeta {
    pub offset: u64,
    /// Compressed length, without the 4-byte checksum.
    pub clen: u32,
    pub rlen: u32,
    /// The first partition in the block (starting, or continued from the previous block).
    pub key: PartitionKey,
    pub cont: Option<Continuation>,
}

impl BlockMeta {
    pub fn stored_len(&self) -> u64 {
        self.clen as u64 + 4
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Index {
    pub codec: Codec,
    pub blocks: Vec<BlockMeta>,
    pub bloom: Bloom,
    pub stats: SstStats,
}

impl Index {
    pub fn data_size(&self) -> u64 {
        self.blocks.last().map_or(0, |b| b.offset + b.stored_len())
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.push(FORMAT);
        out.push(self.codec.id());
        put_uvarint(&mut out, self.blocks.len() as u64);
        for b in &self.blocks {
            put_uvarint(&mut out, b.clen as u64);
            put_uvarint(&mut out, b.rlen as u64);
            out.extend_from_slice(&b.key.token.to_be_bytes());
            put_bytes(&mut out, &b.key.key);
            match &b.cont {
                None => out.push(0),
                Some(c) => {
                    out.push(if c.open.is_some() { 2 } else { 1 });
                    put_bound(&mut out, &c.first);
                    if let Some(d) = &c.open {
                        put_deletion(&mut out, d);
                    }
                    put_deletion(&mut out, &c.partition_deletion);
                }
            }
        }
        self.bloom.encode(&mut out);
        self.stats.encode(&mut out);
        let mut z = zstd::bulk::compress(&out, 3)?;
        let crc = crc32c::crc32c(&z);
        z.extend_from_slice(&crc.to_le_bytes());
        z.extend_from_slice(INDEX_MAGIC);
        Ok(z)
    }

    pub fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() < 8 || &raw[raw.len() - 4..] != INDEX_MAGIC {
            bail!("corrupt sstable index: bad trailer (truncated?)");
        }
        let body = &raw[..raw.len() - 8];
        let crc = u32::from_le_bytes(raw[raw.len() - 8..raw.len() - 4].try_into().expect("4 bytes"));
        if crc32c::crc32c(body) != crc {
            bail!("corrupt sstable index: checksum mismatch");
        }
        let payload = Bytes::from(zstd::stream::decode_all(body).context("corrupt sstable index")?);
        let mut r = BytesReader::new(&payload);
        if r.r.u8()? != FORMAT {
            bail!("unsupported sstable format");
        }
        let codec = Codec::from_id(r.r.u8()?)?;
        let n = r.r.count(12)?;
        let mut blocks: Vec<BlockMeta> = Vec::with_capacity(n);
        let mut offset = 0u64;
        for i in 0..n {
            let clen = r.r.uvarint()?;
            let rlen = r.r.uvarint()?;
            if clen > MAX_BLOCK as u64 || rlen > MAX_BLOCK as u64 {
                bail!("corrupt sstable index: block {i} of {clen}/{rlen} bytes");
            }
            let key = PartitionKey { token: r.r.i64_be()?, key: r.bytes()? };
            let cont = match r.r.u8()? {
                0 => None,
                f @ (1 | 2) => {
                    let first = r.bound()?;
                    let open = if f == 2 { Some(r.deletion()?) } else { None };
                    Some(Continuation { first, open, partition_deletion: r.deletion()? })
                }
                f => bail!("corrupt sstable index: block flag {f}"),
            };
            if i == 0 && cont.is_some() {
                bail!("corrupt sstable index: first block continues a partition");
            }
            if let Some(prev) = blocks.last()
                && key < prev.key
            {
                bail!("corrupt sstable index: blocks out of order");
            }
            blocks.push(BlockMeta { offset, clen: clen as u32, rlen: rlen as u32, key, cont });
            offset += clen + 4;
        }
        let bloom = Bloom::decode(&mut r.r)?;
        let stats = SstStats::decode(&mut r)?;
        if !r.r.is_empty() {
            bail!("corrupt sstable index: trailing bytes");
        }
        Ok(Index { codec, blocks, bloom, stats })
    }

    /// Whether the partition may be in this SSTable (token range and bloom filter).
    pub fn may_contain(&self, key: &PartitionKey) -> bool {
        !self.blocks.is_empty() && *key >= self.stats.first_key && *key <= self.stats.last_key && self.bloom.may_contain(&key.key)
    }

    /// The block holding `key`'s partition header, if it can be anywhere.
    pub fn header_block(&self, key: &PartitionKey) -> Option<usize> {
        let i = self.blocks.partition_point(|b| b.key < *key);
        if let Some(b) = self.blocks.get(i)
            && b.key == *key
            && b.cont.is_none()
        {
            return Some(i);
        }
        i.checked_sub(1)
    }

    /// The first block whose first partition is `>= key` or which contains `key`: where a scan
    /// from `key` starts.
    pub fn scan_block(&self, key: &PartitionKey) -> usize {
        self.header_block(key).unwrap_or(0)
    }

    /// The last block that holds part of the partition whose header is in block `header`.
    pub fn last_block_of(&self, key: &PartitionKey, header: usize) -> usize {
        let mut j = header;
        while j + 1 < self.blocks.len() && self.blocks[j + 1].cont.is_some() && self.blocks[j + 1].key == *key {
            j += 1;
        }
        j
    }

    /// Among blocks `header..=last` of one partition, the last that starts at or before `pos`.
    pub fn seek_block(&self, cmp: &dyn ClusteringComparator, header: usize, last: usize, pos: &Bound) -> usize {
        let conts = &self.blocks[header + 1..=last];
        let n = conts.partition_point(|b| b.cont.as_ref().is_some_and(|c| compare_bounds(cmp, &c.first, pos) != Ordering::Greater));
        header + n
    }
}

/// Reads byte ranges of objects: S3 range GETs in production.
pub trait BlockSource: Send + Sync + 'static {
    fn read<'a>(&'a self, key: &'a str, range: Range<u64>) -> BoxFuture<'a, Result<Bytes>>;
}

impl BlockSource for pg_storage::Objects {
    fn read<'a>(&'a self, key: &'a str, range: Range<u64>) -> BoxFuture<'a, Result<Bytes>> {
        Box::pin(async move {
            let want = range.end - range.start;
            let b = self.get_range(key, range).await?.ok_or_else(|| anyhow::anyhow!("{key} is missing"))?;
            if b.len() as u64 != want {
                bail!("{key}: short read ({} of {want} bytes)", b.len());
            }
            Ok(b)
        })
    }
}

static NEXT_UID: AtomicU64 = AtomicU64::new(1);

/// An open SSTable: its index in memory, data fetched by block on demand.
pub struct SsTable {
    /// Unique in this process (cache keys).
    pub uid: u64,
    /// `nosql/db/<owner>/sst/<table>/<sst-id>`: `.data` and `.index` are appended.
    pub key: String,
    pub index: Index,
    pub index_size: u64,
    /// LCS level / UCS level.
    pub level: u32,
    /// The run it belongs to: the SSTables one flush or compaction wrote share it (ICS).
    pub run: u64,
    /// When it was written (Unix ms).
    pub created_ms: i64,
    pub source: Arc<dyn BlockSource>,
    /// Point reads its bloom filter let through that found no partition (ScyllaDB's
    /// `bloom_filter_false_positives`, summed over a table's SSTables).
    pub bloom_false_positives: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for SsTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SsTable").field("key", &self.key).field("level", &self.level).field("blocks", &self.index.blocks.len()).finish()
    }
}

impl SsTable {
    pub fn new(key: String, index: Index, index_size: u64, level: u32, created_ms: i64, source: Arc<dyn BlockSource>) -> Self {
        let uid = NEXT_UID.fetch_add(1, AtomicOrdering::Relaxed);
        Self { uid, key, index, index_size, level, run: uid, created_ms, source, bloom_false_positives: Default::default() }
    }

    pub fn with_run(mut self, run: u64) -> Self {
        self.run = run;
        self
    }

    pub fn data_key(&self) -> String {
        format!("{}.data", self.key)
    }

    pub fn index_key(&self) -> String {
        format!("{}.index", self.key)
    }

    pub fn data_size(&self) -> u64 {
        self.index.data_size()
    }

    pub fn stats(&self) -> &SstStats {
        &self.index.stats
    }

    /// Load an SSTable's index.
    pub async fn open(objects: &pg_storage::Objects, key: String, level: u32, created_ms: i64) -> Result<Self> {
        let index_key = format!("{key}.index");
        let raw = objects.get(&index_key).await?.ok_or_else(|| anyhow::anyhow!("{index_key} is missing"))?;
        let size = raw.len() as u64;
        let index = tokio::task::spawn_blocking(move || Index::decode(&raw)).await?.with_context(|| index_key.clone())?;
        Ok(Self::new(key, index, size, level, created_ms, Arc::new(objects.clone())))
    }

    /// Check and decompress one stored block.
    /// Verify, decompress and check the trailer of block `i`. Done once per block fetched (the
    /// cache keeps the result), so reads can trust the trailer ([`split_block`]).
    pub fn decode_block(&self, i: usize, stored: &[u8]) -> Result<Bytes> {
        decode_block(self.index.codec, &self.index.blocks[i], stored)
            .and_then(|raw| check_trailer(&raw).map(|()| raw))
            .with_context(|| format!("{} block {i}", self.data_key()))
    }
}

/// Verify a stored block's checksum and decompress it.
pub fn decode_block(codec: Codec, meta: &BlockMeta, stored: &[u8]) -> Result<Bytes> {
    if stored.len() as u64 != meta.stored_len() {
        bail!("corrupt block: {} bytes stored, expected {}", stored.len(), meta.stored_len());
    }
    let (data, crc) = stored.split_at(meta.clen as usize);
    if crc32c::crc32c(data) != u32::from_le_bytes(crc.try_into().expect("4 bytes")) {
        bail!("corrupt block: checksum mismatch");
    }
    Ok(Bytes::from(codec.decompress(data, meta.rlen as usize)?))
}

/// A partition header entry.
#[derive(Clone, Debug, PartialEq)]
pub struct Header {
    pub key: PartitionKey,
    pub deletion: DeletionTime,
    pub static_row: Option<Row>,
}

/// One entry of a decoded block.
pub(crate) enum Entry {
    Partition(Header),
    /// A row, decoded lazily: its clustering and the byte range of its encoding.
    Row(Clustering, Range<usize>),
    Marker(Marker),
    End,
}

pub(crate) fn read_entry(r: &mut BytesReader) -> Result<Option<Entry>> {
    if r.r.is_empty() {
        return Ok(None);
    }
    Ok(Some(match r.r.u8()? {
        E_PARTITION => {
            let token = r.r.i64_be()?;
            let key = r.bytes()?;
            let flags = r.r.u8()?;
            if flags & !3 != 0 {
                bail!("corrupt block: partition flags {flags:#x}");
            }
            let deletion = if flags & 1 != 0 { r.deletion()? } else { DeletionTime::LIVE };
            let static_row = if flags & 2 != 0 { Some(r.row(false)?) } else { None };
            Entry::Partition(Header { key: PartitionKey { token, key }, deletion, static_row })
        }
        E_ROW => {
            let len = r.r.len()?;
            let start = r.r.pos();
            let mut peek = r.clone();
            peek.r.u8()?;
            let c = peek.clustering()?;
            if peek.r.pos() > start + len {
                bail!("corrupt block: row overruns its length");
            }
            r.r.skip(len)?;
            Entry::Row(c, start..start + len)
        }
        E_MARKER => {
            let bound = r.bound()?;
            let flags = r.r.u8()?;
            if flags == 0 || flags & !3 != 0 {
                bail!("corrupt block: marker flags {flags:#x}");
            }
            let close = if flags & 1 != 0 { Some(r.deletion()?) } else { None };
            let open = if flags & 2 != 0 { Some(r.deletion()?) } else { None };
            Entry::Marker(Marker { bound, close, open })
        }
        E_END => Entry::End,
        t => bail!("corrupt block: entry tag {t}"),
    }))
}

/// A decoded block's partition header offsets, read in place from its trailer.
#[derive(Clone, Debug, Default)]
pub(crate) struct HeaderOffsets {
    /// `u32` little-endian offsets, ascending.
    table: Bytes,
}

impl HeaderOffsets {
    pub(crate) fn len(&self) -> usize {
        self.table.len() / 4
    }

    /// Offset `i` (`i < len()`).
    pub(crate) fn get(&self, i: usize) -> u32 {
        u32::from_le_bytes(self.table[4 * i..4 * i + 4].try_into().expect("4"))
    }
}

/// Where a decoded block's entries end and its offset table starts.
fn trailer(raw: &[u8]) -> Result<(usize, usize)> {
    let len = raw.len();
    if len < 4 {
        bail!("corrupt block: no trailer");
    }
    let n = u32::from_le_bytes(raw[len - 4..].try_into().expect("4")) as usize;
    let table = n.checked_mul(4).filter(|t| t + 4 <= len).ok_or_else(|| anyhow::anyhow!("corrupt block: bad trailer"))?;
    Ok((len - 4 - table, n))
}

/// Check a decoded block's trailer: every partition offset is inside the entries, ascending.
pub(crate) fn check_trailer(raw: &[u8]) -> Result<()> {
    let (end, n) = trailer(raw)?;
    let mut last = None;
    for i in 0..n {
        let at = end + 4 * i;
        let o = u32::from_le_bytes(raw[at..at + 4].try_into().expect("4"));
        if o as usize >= end || last.is_some_and(|l| l >= o) {
            bail!("corrupt block: bad partition offset");
        }
        last = Some(o);
    }
    Ok(())
}

/// Split a decoded block, checked by [`check_trailer`] when it was decoded, into its entries and
/// its partition header offsets. Constant time: a point read uses a few offsets of hundreds.
pub(crate) fn split_block(raw: &Bytes) -> Result<(Bytes, HeaderOffsets)> {
    let (end, _) = trailer(raw)?;
    Ok((raw.slice(..end), HeaderOffsets { table: raw.slice(end..raw.len() - 4) }))
}

/// How the key of the partition header at `offset` of a block's entries compares with `key`,
/// without copying it (a point read probes several per block).
pub(crate) fn header_key_cmp(entries: &Bytes, offset: u32, key: &PartitionKey) -> Result<Ordering> {
    let mut r = BytesReader::at(entries, offset as usize)?;
    if r.r.u8()? != E_PARTITION {
        bail!("corrupt block: partition offset doesn't point at a header");
    }
    let token = r.r.i64_be()?;
    let bytes = r.r.bytes()?;
    // `PartitionKey`'s order: token, then key bytes.
    Ok(token.cmp(&key.token).then_with(|| bytes.cmp(&key.key[..])))
}

/// Decode the row whose encoding is `range` of `block`.
pub(crate) fn decode_row(block: &Bytes, range: Range<usize>) -> Result<Row> {
    let mut r = BytesReader::at(block, range.start)?;
    let row = r.row(true)?;
    if r.r.pos() != range.end {
        bail!("corrupt block: row length mismatch");
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codecs_round_trip_and_detect_corruption() {
        let raw: Vec<u8> = (0..70_000u32).map(|i| (i % 97) as u8).collect();
        for codec in [Codec::None, Codec::Lz4, Codec::Zstd] {
            let c = codec.compress(&raw).unwrap();
            let mut stored = c.clone();
            stored.extend_from_slice(&crc32c::crc32c(&c).to_le_bytes());
            let meta = BlockMeta { offset: 0, clen: c.len() as u32, rlen: raw.len() as u32, key: PartitionKey::default(), cont: None };
            assert_eq!(&decode_block(codec, &meta, &stored).unwrap()[..], &raw[..]);
            let mut bad = stored.clone();
            bad[c.len() / 2] ^= 1;
            assert!(decode_block(codec, &meta, &bad).is_err());
            assert!(decode_block(codec, &meta, &stored[..stored.len() - 1]).is_err());
        }
    }

    /// A decoded block of partition headers (with the given offsets in the trailer).
    fn block_of(keys: &[PartitionKey], offsets: Option<Vec<u32>>) -> Bytes {
        let mut raw = Vec::new();
        let mut at = Vec::new();
        for k in keys {
            at.push(raw.len() as u32);
            raw.push(E_PARTITION);
            raw.extend_from_slice(&k.token.to_be_bytes());
            put_bytes(&mut raw, &k.key);
        }
        let offsets = offsets.unwrap_or(at);
        for o in &offsets {
            raw.extend_from_slice(&o.to_le_bytes());
        }
        raw.extend_from_slice(&(offsets.len() as u32).to_le_bytes());
        Bytes::from(raw)
    }

    #[test]
    fn trailer_is_checked_at_decode_and_split_in_place() {
        let keys: Vec<PartitionKey> = [(-5, "b"), (-5, "c"), (3, ""), (3, "a"), (9, "zz")].iter().map(|(t, k)| PartitionKey::new(*t, k.as_bytes().to_vec())).collect();
        let raw = block_of(&keys, None);
        check_trailer(&raw).unwrap();
        let (entries, offsets) = split_block(&raw).unwrap();
        assert_eq!(offsets.len(), keys.len());
        for (i, k) in keys.iter().enumerate() {
            let o = offsets.get(i);
            assert_eq!(header_key_cmp(&entries, o, k).unwrap(), Ordering::Equal);
            // The comparison is `PartitionKey`'s order, whatever the probe.
            for probe in keys.iter().chain([PartitionKey::new(-5, b"a".to_vec()), PartitionKey::new(3, b"b".to_vec()), PartitionKey::new(i64::MAX, Vec::new())].iter()) {
                assert_eq!(header_key_cmp(&entries, o, probe).unwrap(), k.cmp(probe), "{k:?} vs {probe:?}");
            }
        }
        // An offset that isn't a header is an error, not a panic.
        assert!(header_key_cmp(&entries, 1, &keys[0]).is_err());
        assert!(header_key_cmp(&entries, entries.len() as u32 + 1, &keys[0]).is_err());

        let bad = |offsets: Vec<u32>| check_trailer(&block_of(&keys[..2], Some(offsets))).unwrap_err().to_string();
        let len = block_of(&keys[..2], None).len() as u32;
        assert_eq!(bad(vec![0, 0]), "corrupt block: bad partition offset");
        assert_eq!(bad(vec![12, 0]), "corrupt block: bad partition offset");
        assert_eq!(bad(vec![0, len]), "corrupt block: bad partition offset");
        assert_eq!(check_trailer(&[1, 0]).unwrap_err().to_string(), "corrupt block: no trailer");
        assert_eq!(check_trailer(&[0, 0, 0, 9]).unwrap_err().to_string(), "corrupt block: bad trailer");
        assert!(split_block(&Bytes::from_static(&[9, 0, 0, 0])).is_err());
        let (e, o) = split_block(&Bytes::from_static(&[0, 0, 0, 0])).unwrap();
        assert_eq!((e.len(), o.len()), (0, 0));
    }
}
