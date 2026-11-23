//! Sorted string tables: immutable files of (storage key, value) in key order.
//!
//! Format 1 (`MAGIC`; what builds before format 2 write, and still a transaction's runs):
//!
//! ```text
//! [block 0] [block 1] … [index] [bloom] [footer: index off/len, bloom off/len, entries, magic]
//! block  = codec byte (0 raw, 1 zstd + raw length) + entries + crc32
//! entry  = shared varint | unshared varint | value_len+1 varint (0 = tombstone) | key suffix | value
//! index  = per block: last key, offset, length
//! bloom  = over user keys (storage key minus the 8-byte version)
//! ```
//!
//! Format 2 (`MAGIC2`): a block is the block cache's form (`Block`, see there) as it is, so a
//! block read from the file is used as it was decompressed, not decoded into another form:
//!
//! ```text
//! [block 0] … [index] [bloom] [footer: index off/len, bloom off/len, entries, format, magic2]
//! block  = codec byte (0 raw, 1 zstd, 2 lz4; the last two + raw length) + body + crc32
//! body   = entries | values | restart words (u64 each) | restart value offsets (u32 each)
//!          | version (8) | entry count (u32) | restarts (u32) | common (u16) | flags (u8:
//!          bit 0 a value is derived, bits 4-7 the log of the entries between restarts)
//! index  = per block: last key, offset, length, entry count
//! ```
//!
//! A build reads both; it refuses a file (and a manifest) of a format past `FORMAT`.

use super::cache::{BlockCache, Buf, CachedBlock};
use crate::error::{Result, internal};
use bytes::Bytes;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const MAGIC: u64 = 0x726f_6c6c_7373_7431; // "rollsst1"
/// Format 2's magic: builds before it read any other as a bad footer and refuse the store.
const MAGIC2: u64 = 0x726f_6c6c_7373_7432; // "rollsst2"
/// Where format 1's blocks end (their bytes), and format 2's before it took `ROW_BLOCK` and
/// `INDEX_BLOCK`.
pub(crate) const BLOCK_TARGET: usize = 16 * 1024;
/// Where a format 2 block of a table's rows ends, in the bytes format 1 would take for its
/// entries (`SstWriter::raw1`). A read that misses the block cache decompresses a whole block
/// for the entry it wants: a heap fetch of an index scan over a table bigger than the cache
/// (TPC-H's at 256 MB) did so for 16 kB, 40-60% of q07's, q09's and q17's CPU. Half of it,
/// with the file's dictionary (`DICT_BYTES`), costs a scan no more decompression a byte than
/// 16 kB without one (8 kB: 2,089 ns a kB, 16 kB: 2,632), while 4 kB costs scans 8% more a
/// byte and four times the blocks: HANDOFF "Store blocks and caches".
const ROW_BLOCK: usize = 8 << 10;
/// `ROW_BLOCK` for a secondary index's entries, which are read by probes of a key or a short
/// range (a nested loop's inner index scan) more than whole.
const INDEX_BLOCK: usize = 4 << 10;
/// A format 2 file's zstd dictionary (`finalize_dict`): about this many bytes of its first
/// `DICT_SAMPLE` bytes of blocks with their statistics. Small blocks compress as well as
/// 16 kB ones did without it (TPC-H SF1's 8 kB / 4 kB blocks 553 MB of block bytes against
/// 547), and decompress faster a byte (the entropy tables are the dictionary's, not built per
/// block).
const DICT_BYTES: usize = 16 << 10;
const DICT_SAMPLE: usize = 256 << 10;
const FOOTER_LEN: usize = 48;
const FOOTER2_LEN: usize = 56;
/// Format 4's footer: format 2's after the dictionary's offset and length.
const FOOTER4_LEN: usize = 72;
/// The format this build writes the store's files in (a transaction's runs are format 1:
/// `SstWriter::create_run`), but for those with a dictionary (`FORMAT_DICT`).
pub const FORMAT: u32 = 2;
/// Format 2's blocks compressed with a zstd dictionary the file holds (codec 3), the
/// dictionary between the blocks and the index, where format 4's footer says (`SstMeta::
/// dict`). The newest file format this build reads.
pub const FORMAT_DICT: u32 = 4;
/// A file with column blocks (format 5, `storage::colblock`): its footer is format 4's, with a
/// dictionary's offset and length 0 when it has none. Written only by a store whose data
/// directory has column blocks switched on (`StoreOptions::colblocks`, off by default).
pub const FORMAT_COLS: u32 = 5;
/// Format 2's block trailer after the restart arrays: version, entries, restarts, common, flags.
const TRAILER: usize = 8 + 4 + 4 + 2 + 1;

/// The zstd level of format 2's blocks. Their entries leave out what format 1's repeat (an
/// index entry's row key, the block's version), so they compress less well byte for byte but
/// are a third smaller (TPC-H SF1: 1,412 MB against 2,046) and take 15% fewer bytes stored
/// (530 MB against 621). Level -1 writes 5% fewer than format 1 at level 1 for TPC-H and the bulk
/// script's files, but 14% more for the OLTP benchmark's accounts (its rows compress worse at
/// any negative level): HANDOFF "Storage format v2: design".
const ZSTD_LEVEL: i32 = 1;

/// How a file's blocks are compressed (the writer's choice; each block records its own, and
/// one compression didn't shrink by an eighth is stored raw).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    None,
    Zstd,
    Lz4,
}
const BLOOM_BITS_PER_KEY: usize = 10;
const BLOOM_K: u32 = 7;

/// The bytes `put_varint` writes for `v`.
fn varint_len(v: usize) -> usize {
    (usize::BITS - (v | 1).leading_zeros()).div_ceil(7) as usize
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[inline]
pub fn get_varint(b: &[u8], pos: &mut usize) -> Result<u64> {
    // (One byte, the common case: lengths, scales, entry headers.)
    if let Some(&byte) = b.get(*pos)
        && byte < 0x80
    {
        *pos += 1;
        return Ok(byte as u64);
    }
    get_varint_long(b, pos)
}

fn get_varint_long(b: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = *b.get(*pos).ok_or_else(|| internal("truncated varint"))?;
        *pos += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte < 0x80 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(internal("varint overflow"));
        }
    }
}

/// 64-bit hash for bloom filters (FNV-1a with a final mix).
pub fn hash64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &c in b {
        h ^= c as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// Format 2's bloom filter hash: `hash64`'s final mix over the key taken 8 bytes at a time (a
/// multiply and a rotation a word; FNV-1a's multiply a byte was a fifth of `SstWriter::add`,
/// and a point read hashes its key again for every file it looks in).
pub fn hash64_2(b: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = (b.len() as u64).wrapping_mul(K);
    let (words, r) = b.as_chunks::<8>();
    for w in words {
        h = (h ^ u64::from_le_bytes(*w)).wrapping_mul(K).rotate_left(31);
    }
    if !r.is_empty() {
        // (The last 1-7 bytes zero-padded, as a word: from the key's last 8 bytes, shifted, when
        // it has 8. Copying them into a padded buffer was a call to memcpy and a store the
        // word's load waited on, for every key a file's filter or a point read hashes.)
        let t = match b.len().checked_sub(8) {
            Some(at) => u64::from_le_bytes(b[at..].try_into().unwrap()) >> (64 - 8 * r.len()),
            None => {
                let mut t = [0u8; 8];
                t[..r.len()].copy_from_slice(r);
                u64::from_le_bytes(t)
            }
        };
        h = (h ^ t).wrapping_mul(K).rotate_left(31);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// How many bytes `a` and `b` start with alike, 8 at a time.
#[inline]
fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 8 <= n {
        let x = u64::from_le_bytes(a[i..i + 8].try_into().unwrap()) ^ u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if x != 0 {
            return i + (x.trailing_zeros() / 8) as usize;
        }
        i += 8;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

pub fn user_key(storage_key: &[u8]) -> &[u8] {
    &storage_key[..storage_key.len().saturating_sub(8)]
}

pub fn key_ts(storage_key: &[u8]) -> u64 {
    let n = storage_key.len();
    if n < 8 {
        return 0;
    }
    !u64::from_be_bytes(storage_key[n - 8..].try_into().unwrap())
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SstMeta {
    pub id: u64,
    pub size: u64,
    pub entries: u64,
    #[serde(with = "hex_bytes")]
    pub first_key: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub last_key: Vec<u8>,
    pub min_ts: u64,
    pub max_ts: u64,
    /// A transaction's spilled run linked in as its commit's file (`Store::commit_sorted`):
    /// every entry is a version at this commit ts, whatever the run's keys say (format 2;
    /// files without it are read as before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_ts: Option<u64>,
    /// How many of its entries are deletions, as its writer counted them (not stored: 0 for
    /// a file of an earlier session). What `Store::maybe_compact` weighs L0 by.
    #[serde(skip)]
    pub deletes: u64,
    /// The generation of its dead-entry set (`DeadSet`, in `dead_path(id, generation)`): the
    /// entries a compaction found no snapshot can read, left in the file instead of
    /// rewriting it (none: every entry is live).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dv: Option<u64>,
    /// How many entries that set holds.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub dead: u64,
    /// Its blocks are compressed with a zstd dictionary it holds (format 4, `FORMAT_DICT`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dict: bool,
    /// Some of its blocks are column blocks (format 5, `FORMAT_COLS`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cols: bool,
    /// Its blocks a compaction may copy as they are (`SstWriter::copy_block`), as its writer
    /// saw them: every user key in one version, none a deletion, none sharing its key with
    /// the block before or after. Known only to the process that wrote the file (not stored:
    /// a file of an earlier session is written again whole once).
    #[serde(skip)]
    pub clean: Option<Arc<CleanBlocks>>,
}

/// A file's blocks that hold one version of each of their user keys, no deletion, and no user
/// key of the block before or after (`SstMeta::clean`): what a compaction would write of such
/// a block when nothing newer has a key in its range is the block itself.
#[derive(Debug, Default)]
pub struct CleanBlocks {
    bits: Vec<u64>,
    len: usize,
}

impl CleanBlocks {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, b: usize) -> bool {
        b < self.len && self.bits[b / 64] >> (b % 64) & 1 != 0
    }

    fn push(&mut self, clean: bool) {
        if self.len.is_multiple_of(64) {
            self.bits.push(0);
        }
        self.bits[self.len / 64] |= (clean as u64) << (self.len % 64);
        self.len += 1;
    }

    fn unset(&mut self, b: usize) {
        if b < self.len {
            self.bits[b / 64] &= !(1 << (b % 64));
        }
    }

    /// How many are clean.
    pub fn count(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Where file `id`'s dead-entry set of generation `generation` is (`SstMeta::dv`).
pub fn dead_path(dir: &Path, id: u64, generation: u64) -> PathBuf {
    dir.join(format!("{id:016x}-{generation:x}.dv"))
}

/// The entries of a store file that a compaction found dead (a version no snapshot reads: a
/// deleted row's or index entry's, an older version of a key), by block and their place in
/// it (the file's blocks as written). A block read from the file is used without them
/// (`Block::without`), so every reader sees the file as a rewrite without them would have
/// been, while the compaction wrote this set instead of the file again (HANDOFF "Storage:
/// dead-entry sets"). In a file of its own:
///
/// ```text
/// "rolldead" | blocks (u32) | per block: block (u32), words (u32), words (u64 each) | crc32
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeadSet {
    /// By block, ascending: bit i of the words is entry i.
    blocks: Vec<(u32, Box<[u64]>)>,
    count: u64,
}

const DEAD_MAGIC: &[u8; 8] = b"rolldead";

impl DeadSet {
    /// Block `b`'s dead entries (None: none).
    pub fn block(&self, b: usize) -> Option<&[u64]> {
        // (The last block first: a set being made has its entries added in order.)
        if let Some((lb, w)) = self.blocks.last()
            && *lb as usize == b
        {
            return Some(w);
        }
        let i = self.blocks.binary_search_by_key(&(b as u32), |e| e.0).ok()?;
        Some(&self.blocks[i].1)
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn is_dead(&self, b: usize, i: usize) -> bool {
        self.block(b).is_some_and(|w| w.get(i / 64).is_some_and(|x| x >> (i % 64) & 1 != 0))
    }

    /// This set with entry `i` of block `b` dead too (blocks added in ascending order, or
    /// to the last one).
    pub fn add(&mut self, b: usize, i: usize) {
        let b = b as u32;
        let at = match self.blocks.last() {
            Some((lb, _)) if *lb == b => self.blocks.len() - 1,
            Some((lb, _)) if *lb < b => {
                self.blocks.push((b, Box::default()));
                self.blocks.len() - 1
            }
            _ => match self.blocks.binary_search_by_key(&b, |e| e.0) {
                Ok(at) => at,
                Err(at) => {
                    self.blocks.insert(at, (b, Box::default()));
                    at
                }
            },
        };
        let w = &mut self.blocks[at].1;
        if w.len() <= i / 64 {
            let mut v = std::mem::take(w).into_vec();
            v.resize(i / 64 + 1, 0);
            *w = v.into_boxed_slice();
        }
        if w[i / 64] >> (i % 64) & 1 == 0 {
            w[i / 64] |= 1 << (i % 64);
            self.count += 1;
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let words: usize = self.blocks.iter().map(|b| b.1.len()).sum();
        let mut out = Vec::with_capacity(16 + self.blocks.len() * 8 + words * 8);
        out.extend_from_slice(DEAD_MAGIC);
        out.extend_from_slice(&(self.blocks.len() as u32).to_le_bytes());
        for (b, w) in &self.blocks {
            out.extend_from_slice(&b.to_le_bytes());
            out.extend_from_slice(&(w.len() as u32).to_le_bytes());
            for x in w.iter() {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        let crc = crc32fast::hash(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    pub fn decode(b: &[u8]) -> Result<DeadSet> {
        let bad = || internal("bad dead-entry set");
        let n = b.len().checked_sub(4).ok_or_else(bad)?;
        if crc32fast::hash(&b[..n]) != u32::from_le_bytes(b[n..].try_into().unwrap()) || b.get(..8) != Some(DEAD_MAGIC) {
            return Err(bad());
        }
        let mut p = 8;
        let u32_at = |p: &mut usize| -> Result<u32> {
            let v = b.get(*p..*p + 4).ok_or_else(bad)?;
            *p += 4;
            Ok(u32::from_le_bytes(v.try_into().unwrap()))
        };
        let nb = u32_at(&mut p)? as usize;
        let mut set = DeadSet::default();
        for _ in 0..nb {
            let blk = u32_at(&mut p)?;
            let nw = u32_at(&mut p)? as usize;
            let w = b.get(p..p + nw * 8).ok_or_else(bad)?;
            p += nw * 8;
            let w: Box<[u64]> = w.as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect();
            set.count += w.iter().map(|x| x.count_ones() as u64).sum::<u64>();
            if set.blocks.last().is_some_and(|l| l.0 >= blk) {
                return Err(bad());
            }
            set.blocks.push((blk, w));
        }
        if p != n {
            return Err(bad());
        }
        Ok(set)
    }
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
        s.serialize_str(&h)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(serde::de::Error::custom))
            .collect()
    }
}

/// `x % d` for a fixed `d`, by multiplication (Lemire, Kaser and Kurz, "Faster remainder by
/// direct computation"): exactly `%`'s result, without a 64-bit division (tens of cycles,
/// two per key's bloom bits).
#[derive(Clone, Copy)]
pub(crate) struct FastMod {
    m: u128,
    d: u64,
    /// 2^64 mod d (`bloom_bits`).
    wrap: u64,
}

impl FastMod {
    pub(crate) fn new(d: u64) -> FastMod {
        let d1 = d.max(1);
        FastMod {
            m: (u128::MAX / d1 as u128).wrapping_add(1),
            d,
            wrap: (u64::MAX % d1 + 1) % d1,
        }
    }

    #[inline]
    pub(crate) fn rem(self, a: u64) -> u64 {
        let low = self.m.wrapping_mul(a as u128);
        let d = self.d as u128;
        let bottom = ((low as u64 as u128) * d) >> 64;
        let top = (low >> 64) * d;
        ((top + bottom) >> 64) as u64
    }
}

/// The bloom filter bits of a key's hash `h` (`nbits` = the filter's bits): bit i is
/// (h1 + i h2 mod 2^64) mod nbits, for i < `BLOOM_K`. Each from the one before by additions
/// (h2's remainder added, 2^64's taken away when the 64-bit sum wraps), not a remainder each:
/// building a file's filter was a fifth of a compaction's instructions.
#[inline]
fn bloom_bits(h: u64, nbits: FastMod) -> impl Iterator<Item = usize> {
    let (h1, h2) = (h, (h >> 32) | 1);
    let n = nbits.d;
    // (Sums below 2n: a filter's bits are far fewer than 2^63.)
    debug_assert!(n < 1 << 62);
    let (step, wrap) = (nbits.rem(h2), nbits.wrap);
    let (mut v, mut r) = (h1, nbits.rem(h1));
    (0..BLOOM_K).map(move |i| {
        if i > 0 {
            let over;
            (v, over) = v.overflowing_add(h2);
            r += step;
            if r >= n {
                r -= n;
            }
            if over {
                r = if r >= wrap { r - wrap } else { r + n - wrap };
            }
        }
        r as usize
    })
}

fn bloom_set(bloom: &mut [u8], nbits: FastMod, h: u64) {
    for bit in bloom_bits(h, nbits) {
        bloom[bit / 8] |= 1 << (bit % 8);
    }
}

/// Whether a user key is a secondary index's entry (a relation's prefix whose index id isn't
/// 0, `keys::index_prefix`): read by scans of its index's ranges, never looked up alone by
/// a reader that a bloom filter would spare a block.
fn index_entry(uk: &[u8]) -> bool {
    uk.len() >= 8 && uk[..4] != [0; 4] && uk[4..8] != [0; 4]
}

pub fn sst_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id:016x}.sst"))
}

pub struct SstWriter {
    path: PathBuf,
    id: u64,
    out: BufWriter<super::durable::DFile>,
    /// The block's entries (format 2: its entries alone, `vals` holds their values).
    block: Vec<u8>,
    block_entries: usize,
    prev_key: Vec<u8>,
    /// Per block: last key, offset, length, entries.
    index: Vec<(Vec<u8>, u64, u32, u32)>,
    /// Writes format 2 (`FORMAT`): the store's files; a transaction's runs are format 1.
    v2: bool,
    /// Its zstd context, made at its first block (`zstd::bulk::compress` sets one up per
    /// block).
    zc: Option<zstd::bulk::Compressor<'static>>,
    /// Format 2's block being made (`put2`, `body2`), with a whole key every 1 << `rlog`
    /// entries.
    b2: Block2,
    rlog: u8,
    /// Where a block ends: past this many bytes as format 1 counts them (`raw1`); none:
    /// `BLOCK_TARGET` for format 1, `ROW_BLOCK` or `INDEX_BLOCK` for format 2.
    block_target: Option<usize>,
    /// The bytes the block would take in format 1 (where format 2's blocks end too: a block
    /// holds the entries it held in format 1, and the cache holds it as it did).
    raw1: usize,
    hashes: Vec<u64>,
    last_user_hash: Option<u64>,
    /// A key not an index entry was added: the file gets a bloom filter (one of index
    /// entries alone gets none, `index_entry`).
    rows: bool,
    offset: u64,
    entries: u64,
    first_key: Option<Vec<u8>>,
    min_ts: u64,
    max_ts: u64,
    codec: Codec,
    /// fsync the file when it is finished (not for a transaction's spilled writes, which
    /// a crash discards).
    sync: bool,
    /// The bloom filter, when sized up front (`expect_keys`): its bits are set as keys come
    /// instead of keeping every key's hash until `finish`.
    bloom: Option<(Vec<u8>, FastMod)>,
    /// Values are a spilled run's (`txn::spill`: a tag byte first, bit 0 clear for a deletion).
    tagged: bool,
    /// Deletions added (`SstMeta::deletes`).
    deletes: u64,
    /// One index's entries (`index_run`): not hashed, and no filter made.
    index_run: bool,
    /// Compress its blocks with a zstd dictionary of about `.0` bytes trained from its first
    /// `.1` bytes of blocks (`set_dict`): the blocks before it are held back (`held`: body,
    /// last key, entries) until it is made.
    dict: Option<(usize, usize)>,
    held: Vec<(Vec<u8>, Vec<u8>, u32)>,
    held_bytes: usize,
    /// The dictionary and a compressor with it (or none, when training found none: plain
    /// zstd blocks).
    zdict: Option<Option<(Vec<u8>, zstd::bulk::Compressor<'static>)>>,
    /// Its blocks so far (held or written) that are clean (`CleanBlocks`), and whether the
    /// block being made is not.
    clean: CleanBlocks,
    dirty: bool,
    /// The file whose dictionary and bloom filter it took to copy blocks of (`adopt`), the
    /// blocks it copied, and the filter (empty: the file had none).
    adopted: Option<Arc<SstReader>>,
    copied: Vec<u32>,
    carried: Vec<u8>,
    /// How many entries the copied blocks hold (each a user key of its own: clean blocks).
    copied_keys: u64,
    /// The block being made may grow to twice its target (`stretch`).
    stretched: bool,
    /// Its blocks of rows are written as column blocks where they fit one (`colblocks`), and
    /// whether one was (format 5).
    colblocks: bool,
    wrote_cols: bool,
}

/// A file with fewer bytes of blocks than this gets no dictionary (its blocks are plain zstd).
const DICT_MIN_SAMPLE: usize = 64 << 10;

/// Format 2's block as `SstWriter` makes it: the values (after the entries when it is done),
/// each restart's whole key, where its entry and value start, the block's version (its first
/// key's), the bytes entry 0 stores of its key, whether a value is derived.
#[derive(Default)]
struct Block2 {
    vals: Vec<u8>,
    rkeys: Vec<u8>,
    rends: Vec<u32>,
    roffs: Vec<u32>,
    rvals: Vec<u32>,
    ts: Option<[u8; 8]>,
    stored0: usize,
    derived: bool,
    /// Each stored value's length, in order (`colblocks` writers only); `odd`: an entry
    /// without one (a deletion, a derived value): not a column block.
    lens: Vec<u32>,
    odd: bool,
}

impl Block2 {
    fn clear(&mut self) {
        self.lens.clear();
        self.odd = false;
        self.vals.clear();
        self.rkeys.clear();
        self.rends.clear();
        self.roffs.clear();
        self.rvals.clear();
        self.ts = None;
        self.stored0 = 0;
        self.derived = false;
    }
}

impl SstWriter {
    /// A file of a transaction's spilled writes (`txn::spill`): uncompressed, not fsynced.
    pub fn create_temp(dir: &Path, id: u64) -> Result<Self> {
        let mut w = Self::create_with(dir, id, Codec::None, false)?;
        w.sync = false;
        w.tagged = true;
        Ok(w)
    }

    /// A linkable run (`Store::commit_sorted`'s `linked`): compressed as the store's files
    /// are, not fsynced (linking does). One that no other source holds keys of goes to L1
    /// as it is when its commit links it, so its entries are written once. Format 2, its
    /// values plain: every entry is of one command at one journal place, which its
    /// transaction's reads put back in front of each value (`SstReader::retagged`), and every
    /// key of one version, which reads after linking take for the commit's (`SstMeta::
    /// commit_ts`). (Builds before this wrote format 1, each value tagged: still read.)
    pub fn create_run(dir: &Path, id: u64, compress: bool) -> Result<Self> {
        let codec = if compress { Codec::Zstd } else { Codec::None };
        let mut w = Self::create_with(dir, id, codec, true)?;
        w.sync = false;
        w.plain_blocks();
        Ok(w)
    }

    /// Blocks as format 2 wrote them before small blocks and dictionaries: ending at
    /// `BLOCK_TARGET`, compressed without a dictionary. For files written on a statement's path
    /// or short-lived (a transaction's runs, flushes to L0): compressing against a dictionary
    /// takes twice the match-finding time (`ZSTD_compressBlock_fast_dictMatchState`: COPY in
    /// +4.5% of its instructions, INSERT ... SELECT +5.8%), and they are written again in the
    /// bottom level's form when a compaction or a clean close moves them to L1.
    pub(crate) fn plain_blocks(&mut self) {
        if self.entries == 0 {
            self.block_target = Some(BLOCK_TARGET);
            self.dict = None;
            self.colblocks = false;
        }
    }

    /// Write its blocks of a table's rows as column blocks where they fit one (format 5,
    /// `storage::colblock`): an L1 writer (one with a dictionary) of a store that has them
    /// switched on, before its first entry.
    pub(crate) fn colblocks(&mut self) {
        if self.entries == 0 && self.v2 && self.dict.is_some() {
            self.colblocks = true;
        }
    }

    /// `create_run` as builds before format 2 runs wrote it: format 1, each value tagged
    /// (the tests' pinned old encoder; runs of deletions are still format 1, `create_temp`).
    #[doc(hidden)]
    pub fn create_run_v1(dir: &Path, id: u64, compress: bool) -> Result<Self> {
        let codec = if compress { Codec::Zstd } else { Codec::None };
        let mut w = Self::create_with(dir, id, codec, false)?;
        w.sync = false;
        w.tagged = true;
        Ok(w)
    }

    /// A whole key every 1 << `rlog` entries of format 2's blocks (`RLOG` but in tests).
    #[cfg(test)]
    pub(crate) fn restart_log(&mut self, rlog: u8) {
        assert!(self.entries == 0 && (1..=8).contains(&rlog));
        self.rlog = rlog;
    }

    /// End blocks past `n` bytes as format 1 counts them (by default `BLOCK_TARGET` for format
    /// 1, `ROW_BLOCK` / `INDEX_BLOCK` for format 2).
    #[cfg(test)]
    pub(crate) fn set_block_target(&mut self, n: usize) {
        self.block_target = Some(n);
    }

    /// Its blocks compressed without a dictionary (format 2), as builds before format 4 wrote.
    #[cfg(test)]
    pub(crate) fn no_dict(&mut self) {
        self.dict = None;
    }

    /// A run of one index's entries (`Txn::put_index_run`): a file of index entries alone gets
    /// no bloom filter (`finish`), so they aren't hashed for one as they come. (A row added
    /// anyway leaves the file without a filter, which every key passes.)
    pub fn index_run(&mut self) {
        self.index_run = true;
    }

    /// Size the bloom filter for about `n` user keys now (a big ingested commit's: 8 bytes
    /// a key less to hold until the end).
    pub fn expect_keys(&mut self, n: usize) {
        if self.entries == 0 {
            let b = vec![0u8; (n * BLOOM_BITS_PER_KEY).max(64).div_ceil(8)];
            let m = FastMod::new(b.len() as u64 * 8);
            self.bloom = Some((b, m));
        }
    }

    /// A store file (format 2), its blocks zstd-compressed when `compress`.
    pub fn create(dir: &Path, id: u64, compress: bool) -> Result<Self> {
        Self::create_with(dir, id, if compress { Codec::Zstd } else { Codec::None }, true)
    }

    /// A file of format 2 (`v2`) or 1, its blocks compressed by `codec` (format 1: zstd or
    /// none, as builds before format 2 read).
    pub fn create_with(dir: &Path, id: u64, codec: Codec, v2: bool) -> Result<Self> {
        let path = sst_path(dir, id);
        let tmp = path.with_extension("tmp");
        let out = BufWriter::with_capacity(256 * 1024, super::durable::DFile::create(&tmp)?);
        let codec = match (v2, codec) {
            (false, Codec::Lz4) => Codec::Zstd,
            (_, c) => c,
        };
        Ok(SstWriter {
            path,
            id,
            out,
            block: Vec::with_capacity(BLOCK_TARGET + 1024),
            block_entries: 0,
            prev_key: Vec::new(),
            index: Vec::new(),
            v2,
            zc: None,
            b2: Block2::default(),
            rlog: RLOG,
            block_target: None,
            raw1: 0,
            hashes: Vec::new(),
            last_user_hash: None,
            rows: false,
            offset: 0,
            entries: 0,
            first_key: None,
            min_ts: u64::MAX,
            max_ts: 0,
            codec,
            sync: true,
            bloom: None,
            tagged: false,
            deletes: 0,
            index_run: false,
            dict: (v2 && codec == Codec::Zstd).then_some((DICT_BYTES, DICT_SAMPLE)),
            held: Vec::new(),
            held_bytes: 0,
            zdict: None,
            clean: CleanBlocks::default(),
            dirty: false,
            adopted: None,
            copied: Vec::new(),
            carried: Vec::new(),
            copied_keys: 0,
            stretched: false,
            colblocks: false,
            wrote_cols: false,
        })
    }

    /// Whether blocks of `f` can be copied into it (`copy_block`): it took `f`'s dictionary
    /// and filter (`adopt`), or can still take them: a format 2 writer that wrote no block yet
    /// (blocks held for a dictionary are written with `f`'s), compressing as `f`'s blocks are
    /// read (a file with a dictionary; without one, a writer that doesn't compress).
    pub fn can_copy_from(&self, f: &Arc<SstReader>) -> bool {
        match &self.adopted {
            Some(a) => Arc::ptr_eq(a, f),
            None => {
                self.v2
                    && f.v2
                    && f.meta.commit_ts.is_none()
                    && f.retag.is_none()
                    && self.index.is_empty()
                    && self.zdict.is_none()
                    && !self.index_run
                    && self.bloom.is_none()
                    && match self.codec {
                        Codec::Zstd => f.ddict.is_some(),
                        Codec::None => f.ddict.is_none(),
                        Codec::Lz4 => false,
                    }
            }
        }
    }

    /// Take `f`'s dictionary for its blocks (the ones held back for training one too) and its
    /// bloom filter, to copy `f`'s blocks (`can_copy_from` first).
    pub fn adopt(&mut self, f: &Arc<SstReader>) -> Result<()> {
        if self.adopted.is_some() {
            return Ok(());
        }
        debug_assert!(self.can_copy_from(f));
        self.zdict = Some(match f.dict_bytes()? {
            Some(d) => {
                let c = zstd::bulk::Compressor::with_dictionary(ZSTD_LEVEL, &d).map_err(|e| internal(format!("zstd: {e}")))?;
                Some((d, c))
            }
            None => None,
        });
        self.dict = None;
        for (raw, last, n) in std::mem::take(&mut self.held) {
            self.write_block(&raw, last, n)?;
        }
        self.held_bytes = 0;
        self.carried = f.bloom.clone();
        self.min_ts = self.min_ts.min(f.meta.min_ts);
        self.max_ts = self.max_ts.max(f.meta.max_ts);
        self.adopted = Some(f.clone());
        Ok(())
    }

    /// Block `b` of the file it adopted (`adopt`), copied as it is stored: `data` (its codec
    /// byte to its crc), its last key and entry count from the file's index, its first key
    /// when it is this file's first entry. The block being made ends first. Every key of it
    /// must come after the keys added so far and before those added next.
    pub fn copy_block(&mut self, b: usize, data: &[u8], last: &[u8], n: u32, first: Option<&[u8]>) -> Result<()> {
        self.finish_block()?;
        if self.first_key.is_none() {
            self.first_key = Some(first.ok_or_else(|| internal("a copied block's first key"))?.to_vec());
        }
        self.out.write_all(data)?;
        self.index.push((last.to_vec(), self.offset, data.len() as u32, n));
        self.offset += data.len() as u64;
        self.entries += n as u64;
        self.prev_key.clear();
        self.prev_key.extend_from_slice(last);
        self.last_user_hash = None;
        self.rows |= !self.carried.is_empty();
        self.clean.push(true);
        self.copied.push(b as u32);
        self.copied_keys += n as u64;
        Ok(())
    }

    /// Whether the block being made holds entries, but under a quarter of its target: a
    /// block copied after it would leave it that small.
    pub fn pending_small(&self) -> bool {
        self.block_entries > 0 && self.block_bytes1() < self.target(index_entry(user_key(&self.prev_key))) / 4
    }

    /// Let the block being made grow to twice its target (until it ends, or `false`): it
    /// takes in a block that would otherwise be copied after it small (`pending_small`).
    pub fn stretch(&mut self, on: bool) {
        self.stretched = on;
    }

    /// Where a block of a table's rows (or index entries, `index`) ends.
    fn target(&self, index: bool) -> usize {
        match self.block_target {
            Some(t) => t,
            None if !self.v2 => BLOCK_TARGET,
            None if index => INDEX_BLOCK,
            None => ROW_BLOCK,
        }
    }

    /// How many blocks it copied.
    pub fn copied_blocks(&self) -> usize {
        self.copied.len()
    }

    /// Compress its blocks with a zstd dictionary of about `size` bytes trained from its first
    /// `sample` bytes of blocks (format 4; a format 2 zstd writer only, before its first
    /// block).
    #[cfg(test)]
    pub(crate) fn set_dict(&mut self, size: usize, sample: usize) {
        if self.v2 && self.codec == Codec::Zstd && self.entries == 0 {
            self.dict = Some((size, sample));
        }
    }

    pub fn entries(&self) -> u64 {
        self.entries
    }

    pub fn approx_size(&self) -> u64 {
        // (Blocks held back for a dictionary: about a third of their bytes once compressed.)
        self.offset + (self.block.len() + self.b2.vals.len() + self.held_bytes / 3) as u64
    }

    /// The block's bytes so far as format 1 counts them (`raw1`).
    fn block_bytes1(&self) -> usize {
        if self.v2 { self.raw1 } else { self.block.len() }
    }

    /// Keys must arrive in strictly increasing order.
    pub fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        if self.first_key.is_none() {
            self.first_key = Some(key.to_vec());
        }
        let at_start = self.block_entries == 0;
        if self.v2 {
            self.put2(key, value);
        } else {
            self.put1(key, value);
        }
        self.prev_key.clear();
        self.prev_key.extend_from_slice(key);
        self.block_entries += 1;
        self.entries += 1;
        let deleted = match value {
            None => true,
            Some(v) => self.tagged && v.first().is_none_or(|t| t & 1 == 0),
        };
        self.deletes += deleted as u64;
        self.dirty |= deleted;
        let ts = key_ts(key);
        self.min_ts = self.min_ts.min(ts);
        self.max_ts = self.max_ts.max(ts);
        let index = index_entry(user_key(key));
        self.rows |= !index;
        if !self.index_run {
            let h = if self.v2 { hash64_2(user_key(key)) } else { hash64(user_key(key)) };
            if self.last_user_hash != Some(h) {
                match &mut self.bloom {
                    Some((b, m)) => bloom_set(b, *m, h),
                    None => self.hashes.push(h),
                }
                self.last_user_hash = Some(h);
            } else {
                // (Another version of the key before: this block isn't clean, nor the one
                // before when that one ended with it. Equal hashes of two keys only cost a
                // copy.)
                self.dirty = true;
                if at_start && let Some(b) = self.clean.len().checked_sub(1) {
                    self.clean.unset(b);
                }
            }
        } else {
            self.dirty = true;
        }
        let target = self.target(index) << self.stretched as u32;
        if self.block_bytes1() >= target {
            self.finish_block()?;
        }
        Ok(())
    }

    /// Format 1's entry: the key's bytes after those it shares with the one before (none at
    /// a restart), then the value.
    fn put1(&mut self, key: &[u8], value: Option<&[u8]>) {
        // (A whole key at every restart: `Block::decode` keeps the restarts where they are.)
        let shared = if self.block_entries.is_multiple_of(RESTART) { 0 } else { common_prefix(&self.prev_key, key) };
        put_varint(&mut self.block, shared as u64);
        put_varint(&mut self.block, (key.len() - shared) as u64);
        put_varint(
            &mut self.block,
            value.map(|v| v.len() as u64 + 1).unwrap_or(0),
        );
        self.block.extend_from_slice(&key[shared..]);
        if let Some(v) = value {
            self.block.extend_from_slice(v);
        }
    }

    /// Format 2's entry, in the form `Block` reads (`Block::decode_with` makes the same of a
    /// format 1 block): the version left out when it is the block's, a value the key's tail
    /// derives as that length, other values after the block's entries.
    fn put2(&mut self, key: &[u8], value: Option<&[u8]>) {
        let b = &mut self.b2;
        let klen = key.len();
        let ts = *b.ts.get_or_insert_with(|| <[u8; 8]>::try_from(&key[klen.saturating_sub(8)..]).unwrap_or([0xFF; 8]));
        let mut flags = 0;
        let mut ukey = klen;
        if klen >= 8 && key[klen - 8..] == ts {
            flags |= F_TS;
            ukey -= 8;
        }
        let tail = value.and_then(|v| derived_tail(key, v));
        if tail.is_some() {
            flags |= F_DERIVED;
            b.derived = true;
        }
        let restart = self.block_entries.is_multiple_of(1 << self.rlog);
        let common = if restart { 0 } else { common_prefix(&self.prev_key, key) };
        let vl = value.map_or(0, |v| v.len() + 1);
        // (Format 1's entry: its three lengths, the key's bytes past the shared ones, the value.)
        self.raw1 += varint_len(common) + varint_len(klen - common) + varint_len(vl) + klen - common + vl.saturating_sub(1);
        let share = if restart {
            b.roffs.push(self.block.len() as u32);
            b.rvals.push(b.vals.len() as u32);
            b.rkeys.extend_from_slice(key);
            b.rends.push(b.rkeys.len() as u32);
            if self.block_entries == 0 {
                b.stored0 = ukey;
            }
            0
        } else {
            common.min(ukey)
        };
        put_varint(&mut self.block, share as u64);
        put_varint(&mut self.block, ((ukey - share) << 2 | flags) as u64);
        put_varint(&mut self.block, tail.unwrap_or(vl) as u64);
        self.block.extend_from_slice(&key[share..ukey]);
        if let (Some(v), None) = (value, tail) {
            b.vals.extend_from_slice(v);
            if self.colblocks {
                b.lens.push(v.len() as u32);
            }
        } else {
            b.odd = true;
        }
    }

    /// Format 2's block body: the entries, their values, and what `Block::load` reads from
    /// its end (the restarts as `Block::restarts` holds them).
    fn body2(&mut self) -> Vec<u8> {
        let b = &mut self.b2;
        let mut raw = std::mem::take(&mut self.block);
        let vbase = raw.len() as u32;
        // (Format 5: a block of rows alone, its values as columns when they fit them.)
        let cols = self.colblocks && !b.odd && b.lens.len() == self.block_entries && super::colblock::encode_lens(&b.vals, &b.lens, vbase as usize, &mut raw);
        if !cols {
            raw.extend_from_slice(&b.vals);
        }
        self.wrote_cols |= cols;
        let nr = b.roffs.len();
        let rkey_at = |i: usize| &b.rkeys[if i == 0 { 0 } else { b.rends[i - 1] as usize }..b.rends[i] as usize];
        let common = match nr {
            0 => 0,
            n => {
                let (x, y) = (rkey_at(0), rkey_at(n - 1));
                x.iter().zip(y).take_while(|(p, q)| p == q).count().min(b.stored0)
            }
        };
        raw.reserve(nr * 12 + TRAILER);
        for (i, off) in b.roffs.iter().enumerate() {
            let w = word(&rkey_at(i)[common..]) >> 32 << 32 | *off as u64;
            raw.extend_from_slice(&w.to_le_bytes());
        }
        for v in &b.rvals {
            raw.extend_from_slice(&(v + vbase).to_le_bytes());
        }
        raw.extend_from_slice(&b.ts.unwrap_or([0xFF; 8]));
        raw.extend_from_slice(&(self.block_entries as u32).to_le_bytes());
        raw.extend_from_slice(&(nr as u32).to_le_bytes());
        raw.extend_from_slice(&(common as u16).to_le_bytes());
        raw.push(b.derived as u8 | (cols as u8) << 1 | self.rlog << 4);
        b.clear();
        raw
    }

    fn finish_block(&mut self) -> Result<()> {
        if self.block_entries == 0 {
            return Ok(());
        }
        let raw = if self.v2 { self.body2() } else { std::mem::take(&mut self.block) };
        let n = self.block_entries as u32;
        self.block_entries = 0;
        self.raw1 = 0;
        self.clean.push(!self.dirty);
        self.dirty = false;
        self.stretched = false;
        if let (Some((_, sample)), None) = (self.dict, &self.zdict) {
            self.held_bytes += raw.len();
            self.held.push((raw, self.prev_key.clone(), n));
            self.block = Vec::with_capacity(BLOCK_TARGET + 1024);
            if self.held_bytes >= sample {
                self.train()?;
            }
            return Ok(());
        }
        let last = self.prev_key.clone();
        self.write_block(&raw, last, n)?;
        self.block = raw;
        self.block.clear();
        Ok(())
    }

    /// The dictionary trained from the blocks held back (`held`), then those written with it.
    fn train(&mut self) -> Result<()> {
        let (size, _) = self.dict.expect("a dictionary asked for");
        let made = if self.held_bytes < DICT_MIN_SAMPLE || self.held.len() < 8 {
            None
        } else {
            let samples: Vec<&[u8]> = self.held.iter().map(|h| h.0.as_slice()).collect();
            finalize_dict(&samples, size)
        };
        self.zdict = Some(match made {
            Some(d) => {
                let c = zstd::bulk::Compressor::with_dictionary(ZSTD_LEVEL, &d).map_err(|e| internal(format!("zstd: {e}")))?;
                Some((d, c))
            }
            None => None,
        });
        for (raw, last, n) in std::mem::take(&mut self.held) {
            self.write_block(&raw, last, n)?;
        }
        self.held_bytes = 0;
        Ok(())
    }

    /// A finished block's body compressed (or not) and written, with its index entry.
    fn write_block(&mut self, raw: &[u8], last: Vec<u8>, n: u32) -> Result<()> {
        let mut data = Vec::with_capacity(raw.len() + 9);
        let zstd_err = |e: std::io::Error| internal(format!("zstd: {e}"));
        let packed = match self.codec {
            Codec::None => None,
            Codec::Zstd if matches!(self.zdict, Some(Some(_))) => {
                let (_, zc) = self.zdict.as_mut().and_then(|z| z.as_mut()).expect("a dictionary");
                Some((3, zc.compress(raw).map_err(zstd_err)?))
            }
            Codec::Zstd => {
                if self.zc.is_none() {
                    self.zc = Some(zstd::bulk::Compressor::new(if self.v2 { ZSTD_LEVEL } else { 1 }).map_err(zstd_err)?);
                }
                let zc = self.zc.as_mut().expect("a zstd context");
                Some((1, zc.compress(raw).map_err(zstd_err)?))
            }
            Codec::Lz4 => Some((2, lz4_flex::block::compress(raw))),
        };
        match packed {
            Some((tag, c)) if c.len() < raw.len() * 7 / 8 => {
                data.push(tag);
                data.extend_from_slice(&(raw.len() as u32).to_le_bytes());
                data.extend_from_slice(&c);
            }
            _ => {
                data.push(0);
                data.extend_from_slice(raw);
            }
        }
        let crc = crc32fast::hash(&data);
        data.extend_from_slice(&crc.to_le_bytes());
        self.out.write_all(&data)?;
        self.index.push((last, self.offset, data.len() as u32, n));
        self.offset += data.len() as u64;
        Ok(())
    }

    pub fn finish(mut self) -> Result<SstMeta> {
        self.finish_block()?;
        if self.dict.is_some() && self.zdict.is_none() {
            self.train()?;
        }
        // (Format 4: the dictionary after the blocks, with its crc32.)
        let dict = match self.zdict.take() {
            Some(Some((mut d, _))) => {
                let at = self.offset;
                d.extend_from_slice(&crc32fast::hash(&d).to_le_bytes());
                self.out.write_all(&d)?;
                self.offset += d.len() as u64;
                Some((at, d.len() as u64))
            }
            _ => None,
        };
        let mut index = Vec::new();
        put_varint(&mut index, self.index.len() as u64);
        for (k, off, len, n) in &self.index {
            put_varint(&mut index, k.len() as u64);
            index.extend_from_slice(k);
            put_varint(&mut index, *off);
            put_varint(&mut index, *len as u64);
            if self.v2 {
                put_varint(&mut index, *n as u64);
            }
        }
        let index_off = self.offset;
        self.out.write_all(&index)?;
        self.offset += index.len() as u64;

        let bloom = match self.bloom.take() {
            // (Index entries are scanned, not read by key: no filter, so no time building one
            // and no memory holding it. An empty filter passes every key, in builds before
            // this one too.)
            _ if !self.rows || self.index_run => Vec::new(),
            Some((b, _)) => b,
            None if !self.copied.is_empty() => self.copied_bloom()?,
            None => {
                let nbits = (self.hashes.len() * BLOOM_BITS_PER_KEY).max(64);
                let mut bloom = vec![0u8; nbits.div_ceil(8)];
                let m = FastMod::new(bloom.len() as u64 * 8);
                for h in &self.hashes {
                    bloom_set(&mut bloom, m, *h);
                }
                bloom
            }
        };
        let bloom_off = self.offset;
        self.out.write_all(&bloom)?;
        self.offset += bloom.len() as u64;

        let mut footer = Vec::with_capacity(FOOTER4_LEN);
        let tail: &[u64] = match (self.v2, dict) {
            (false, _) => &[MAGIC],
            (true, _) if self.wrote_cols => &[FORMAT_COLS as u64, MAGIC2],
            (true, None) => &[FORMAT as u64, MAGIC2],
            (true, Some(_)) => &[FORMAT_DICT as u64, MAGIC2],
        };
        let head: &[u64] = match dict {
            Some((at, n)) => &[at, n],
            None if self.wrote_cols => &[0, 0],
            None => &[],
        };
        for v in head.iter().chain(&[index_off, index.len() as u64, bloom_off, bloom.len() as u64, self.entries]).chain(tail) {
            footer.extend_from_slice(&v.to_le_bytes());
        }
        self.out.write_all(&footer)?;
        self.offset += footer.len() as u64;
        let file = self
            .out
            .into_inner()
            .map_err(|e| internal(format!("flush sst: {e}")))?;
        if self.sync {
            super::fileio::sync(|| file.sync_all())?;
        }
        super::fileio::wrote_sst(self.offset);
        super::durable::rename(&self.path.with_extension("tmp"), &self.path)?;
        Ok(SstMeta {
            id: self.id,
            size: self.offset,
            entries: self.entries,
            first_key: self.first_key.unwrap_or_default(),
            last_key: self.prev_key,
            min_ts: if self.min_ts == u64::MAX {
                0
            } else {
                self.min_ts
            },
            max_ts: self.max_ts,
            commit_ts: None,
            deletes: self.deletes,
            dv: None,
            dead: 0,
            dict: dict.is_some(),
            cols: self.wrote_cols,
            clean: self.v2.then(|| Arc::new(std::mem::take(&mut self.clean))),
        })
    }

    /// The bloom filter of a file with copied blocks: the adopted file's with the bits of the
    /// keys added set too (a superset of the keys: no key it holds is turned away), while
    /// that is about what a filter made for its keys would be (no more than twice the bits,
    /// at most 60% of them set: 10 bits a key with 7 set each sets about half); else made
    /// for its keys, the copied blocks' read again for theirs.
    fn copied_bloom(&mut self) -> Result<Vec<u8>> {
        let keys = self.copied_keys as usize + self.hashes.len();
        let mut b = std::mem::take(&mut self.carried);
        if !b.is_empty() {
            let m = FastMod::new(b.len() as u64 * 8);
            for h in &self.hashes {
                bloom_set(&mut b, m, *h);
            }
            let bits = b.len() * 8;
            let set: usize = b.iter().map(|x| x.count_ones() as usize).sum();
            if keys * BLOOM_BITS_PER_KEY * 2 >= bits && set * 10 <= bits * 6 {
                return Ok(b);
            }
        }
        let f = self.adopted.clone().ok_or_else(|| internal("copied blocks without their file"))?;
        let mut pos = BlockPos::default();
        for &i in &self.copied {
            let blk = f.raw_block(i as usize)?;
            let mut last = None;
            blk.at(0, &mut pos);
            while pos.index() < blk.len() {
                let h = hash64_2(user_key(pos.key()));
                if last != Some(h) {
                    self.hashes.push(h);
                    last = Some(h);
                }
                blk.step(&mut pos);
            }
        }
        let nbits = (self.hashes.len() * BLOOM_BITS_PER_KEY).max(64);
        let mut bloom = vec![0u8; nbits.div_ceil(8)];
        let m = FastMod::new(bloom.len() as u64 * 8);
        for h in &self.hashes {
            bloom_set(&mut bloom, m, *h);
        }
        Ok(bloom)
    }

    pub fn abandon(self) {
        let _ = super::durable::remove_file(&self.path.with_extension("tmp"));
    }
}

pub struct SstReader {
    pub meta: SstMeta,
    file: File,
    path: PathBuf,
    /// Its blocks are format 2's (`Block::load`), else format 1's (`Block::decode`).
    v2: bool,
    index: BlockIndex,
    bloom: Vec<u8>,
    /// Remainders by the bloom filter's bit count.
    bloom_mod: FastMod,
    cache: Arc<BlockCache>,
    cache_id: u64,
    obsolete: AtomicBool,
    /// Its dead entries (`SstMeta::dv`), left out of every block read.
    dead: Option<Arc<DeadSet>>,
    /// A run of format 2 read by its transaction (`retagged`): the run's tag put in front of
    /// every value, as format 1's runs store it.
    retag: Option<Box<[u8]>>,
    /// A newer reader of the file has another dead-entry set: this one's file goes when it
    /// does (the sst file stays).
    dead_obsolete: AtomicBool,
    /// The dictionary its codec 3 blocks were compressed with (format 4), and where it is
    /// in the file (with its crc32).
    ddict: Option<zstd::dict::DecoderDictionary<'static>>,
    dict_at: Option<(u64, u64)>,
    /// Where its block index is in the file.
    index_at: (u64, u64),
}

/// The longest key `Block::run_rows` makes in a buffer of its own (a table's row keys are 12 to
/// 24 bytes; an index entry's often more: those go the general way).
const KEY: usize = 32;

/// Entries between restarts in a cached block: every `RESTART`th entry stores its whole key
/// (shared = 0), so a seek searches the restarts and decodes at most `RESTART - 1` entries.
/// (Format 2 records its interval in each block, `Block::rlog`: this is what it writes.)
const RLOG: u8 = 4;
const RESTART: usize = 1 << RLOG;

/// A block as the cache holds it: its entries in a compact form of the file's (values are
/// slices of them), a whole key every `RESTART` entries, and where those entries start.
/// Keys are rebuilt as a cursor (`BlockPos`) moves, never stored whole: a copy of every key
/// and an offset table per entry made the cache hold about 60% more than the entries' bytes
/// (TPC-H SF1: 3.2 GB for 2.0 GB of entries), so a 2 GB cache held two thirds of it.
///
/// The compact form (`Block::decode`) leaves out what an entry repeats: the 8-byte version
/// at a key's end when it is the block's (`ts`; a bulk load or a compaction's output has
/// one version in nearly every block), and an index entry's value when it is the row's
/// storage key the index key already ends with (`derived`: the table's prefix, then the
/// key's last bytes before the version). TPC-H SF1's entries take 1.4 GB so, against 2.1 GB
/// in the files' form. An entry is `shared | unshared << 2 | flags | value field | key
/// suffix`, the entries' values after all of them (in entry order): a seek walks the keys
/// alone, a few cache lines, not the rows between them. Flag `F_TS` puts the block's
/// version after the suffix, `F_DERIVED` makes the value field the length of the key's tail
/// the value ends with.
pub struct Block {
    /// The entries, their values, then (from `rs`) the restart words and the restarts' value
    /// offsets, then (from `lk`) the block's last key: one allocation a block. (Each in an
    /// allocation of its own took about 150 bytes a block more, which the cache didn't count:
    /// with blocks half the size, TPC-H at 2 GB peaked 25 MB higher.)
    ///
    /// The restart words: every `RESTART`th entry's key as a number in the high 32 bits (as
    /// `BlockIndex::words`, of 4 bytes: those after the `common` bytes all restarts' keys
    /// start with), its offset in the low ones (u64 little-endian, `restart`). A seek
    /// searches the numbers, not the entries: a point read's block is seldom in the CPU's
    /// cache, and every restart it compared was a miss (three quarters of `seek`). The value
    /// offsets: where each restart's entry's value is (u32, `rval`; each value follows the
    /// one before's).
    data: Bytes,
    rs: u32,
    nr: u32,
    lk: u32,
    common: u32,
    len: usize,
    /// The version (a key's last 8 bytes) entries flagged `F_TS` end with.
    ts: [u8; 8],
    /// Whether some entry's value is derived (`expand` then copies the values).
    derived: bool,
    /// Entries between restarts: 1 << `rlog` (`RLOG` but in a format 2 block that says
    /// otherwise).
    rlog: u8,
    /// Whether `data` is a piece of the block cache's arena (`cache::Buf`), not the heap's.
    pooled: bool,
    /// The bytes `data`'s allocations hold (`held_by`).
    held: u32,
    /// A column block's section (format 5, `colblock`; None: format 2's values). Its entries'
    /// value offsets start at `data`'s end (`ColBlk::base`), past every slice of `data`: each
    /// reader of a value by offset finds it missing there, where a check it makes anyway
    /// fails, and only then asks for the row (`col_row`, `col_rows`). Format 2's blocks pay
    /// nothing for it.
    cols: Option<Box<ColBlk>>,
}

/// A column block's parsed section, where its value offsets start (`data`'s length), and its
/// rows' bytes once a reader wanted them all (a compaction's merge: `Block::col_rows`).
pub struct ColBlk {
    pub dir: super::colblock::ColDir,
    base: u32,
    rows: std::sync::OnceLock<Bytes>,
}

// (A block's size is what the cache counts it at (`mem_bytes`): the column section's pointer
// took the room `common` gave up.)
const _: () = assert!(std::mem::size_of::<Block>() == 80);

const F_TS: usize = 1;
const F_DERIVED: usize = 2;

/// A position in a block: entry `i`, its whole key and value, and where the next one starts.
#[derive(Default)]
pub struct BlockPos {
    i: usize,
    next: u32,
    /// Where the value of the entry after `i` starts (if it has one stored).
    vnext: u32,
    /// `shared` of entry `i`.
    shared: u32,
    key: Vec<u8>,
    val_off: u32,
    /// `TOMBSTONE`: a deletion; `DERIVED`: the value is `dval`.
    val_len: u32,
    /// A derived value, made from the key.
    dval: Vec<u8>,
}

const TOMBSTONE: u32 = u32::MAX;
const DERIVED: u32 = u32::MAX - 1;

impl Clone for BlockPos {
    fn clone(&self) -> BlockPos {
        let mut p = BlockPos::default();
        p.clone_from(self);
        p
    }

    /// (Into this one's key buffers: a point reader keeps the position its block was found
    /// at, read after read, without an allocation each.)
    fn clone_from(&mut self, o: &BlockPos) {
        let BlockPos { i, next, vnext, shared, key, val_off, val_len, dval } = self;
        (*i, *next, *vnext, *shared, *val_off, *val_len) = (o.i, o.next, o.vnext, o.shared, o.val_off, o.val_len);
        key.clone_from(&o.key);
        dval.clone_from(&o.dval);
    }
}

impl BlockPos {
    /// Room for `n` bytes in the key buffer (a point read's keys: made once, not grown entry
    /// by entry).
    pub fn reserve_key(&mut self, n: usize) {
        self.key.reserve(n);
    }

    pub fn index(&self) -> usize {
        self.i
    }

    pub fn key(&self) -> &[u8] {
        &self.key
    }

    /// The length of the key's prefix stored as the entry before's: what it shares with that
    /// key's user key, at most (0 at a restart, every `RESTART` entries).
    pub fn shared(&self) -> usize {
        self.shared as usize
    }
}

/// `v` extended by `s`: a few bytes (a key's tail after the one before) copied in place, not
/// by a call.
#[inline]
pub fn extend_short(v: &mut Vec<u8>, s: &[u8]) {
    if s.len() > 4 {
        return v.extend_from_slice(s);
    }
    v.reserve(s.len());
    for &x in s {
        v.push(x);
    }
}

/// The header of the entry at `pos`: (shared, unshared, value field), `pos` past it.
#[inline]
fn entry_head(b: &[u8], pos: &mut usize) -> Result<(usize, usize, usize)> {
    let shared = get_varint(b, pos)? as usize;
    let unshared = get_varint(b, pos)? as usize;
    let vl = get_varint(b, pos)? as usize;
    Ok((shared, unshared, vl))
}

/// `entry_head` of an entry `Block::decode` wrote, inline: a seek reads up to `RESTART`
/// of them (the call was a tenth of TPC-H q20).
///
/// Two-byte numbers (a row's value length) are read inline too, and longer ones by a call that
/// returns where they end: a call given `pos` itself kept the caller's place in memory, stored
/// at every byte of the entries a seek reads.
#[inline(always)]
fn head(b: &[u8], pos: &mut usize) -> (usize, usize, usize) {
    #[inline(always)]
    fn v(b: &[u8], pos: &mut usize) -> usize {
        let x = b[*pos];
        if x < 0x80 {
            *pos += 1;
            return x as usize;
        }
        let y = b[*pos + 1];
        if y < 0x80 {
            *pos += 2;
            return (x & 0x7f) as usize | (y as usize) << 7;
        }
        let (v, n) = varint_at(b, *pos);
        *pos += n;
        v
    }
    let shared = v(b, pos);
    let unshared = v(b, pos);
    (shared, unshared, v(b, pos))
}

/// The number of a checked block's varint at `at` and its length in bytes (`head`'s numbers
/// of three bytes or more).
#[cold]
#[inline(never)]
fn varint_at(b: &[u8], at: usize) -> (usize, usize) {
    let mut p = at;
    let v = get_varint_long(b, &mut p).expect("a checked block");
    (v as usize, p - at)
}

/// `a ++ b` compared with `key`.
fn cmp_split(a: &[u8], b: &[u8], key: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let n = a.len().min(key.len());
    match a[..n].cmp(&key[..n]) {
        Equal if key.len() > a.len() => b.cmp(&key[a.len()..]),
        Equal if b.is_empty() && key.len() == a.len() => Equal,
        Equal => Greater,
        o => o,
    }
}

/// The length of the user key's tail a value made of `key`'s first 4 bytes, 4 zero bytes and
/// that tail is (a secondary index entry's row key), if `v` is one.
fn derived_tail(key: &[u8], v: &[u8]) -> Option<usize> {
    let ulen = key.len().checked_sub(8)?;
    let n = v.len().checked_sub(8)?;
    (ulen >= 4 && n <= ulen && v[..4] == key[..4] && v[4..8] == [0; 4] && v[8..] == key[ulen - n..ulen])
        .then_some(n)
}

impl Block {
    /// Its restarts.
    #[inline(always)]
    fn nres(&self) -> usize {
        self.nr as usize
    }

    /// Restart `r`'s word (`data`).
    #[inline(always)]
    fn restart(&self, r: usize) -> u64 {
        let i = self.rs as usize + 8 * r;
        u64::from_le_bytes(self.data[i..i + 8].try_into().unwrap())
    }

    /// Where restart `r`'s entry's value is (`data`).
    #[inline(always)]
    fn rval(&self, r: usize) -> u32 {
        let i = self.rs as usize + 8 * self.nr as usize + 4 * r;
        u32::from_le_bytes(self.data[i..i + 4].try_into().unwrap())
    }

    /// The restart words and value offsets of entries and values `data`, then `last_key`:
    /// a block's `data` (`rs`, `lk` where they start).
    fn assemble(mut data: Vec<u8>, restarts: &[u64], rvals: &[u32], last_key: &[u8]) -> (Bytes, u32, u32) {
        let rs = data.len();
        data.reserve_exact(restarts.len() * 12 + last_key.len());
        for r in restarts {
            data.extend_from_slice(&r.to_le_bytes());
        }
        for v in rvals {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let lk = data.len();
        data.extend_from_slice(last_key);
        (Bytes::from(data), rs as u32, lk as u32)
    }

    #[cfg(test)]
    fn restarts_vec(&self) -> Vec<u64> {
        (0..self.nres()).map(|r| self.restart(r)).collect()
    }

    #[cfg(test)]
    fn rvals_vec(&self) -> Vec<u32> {
        (0..self.nres()).map(|r| self.rval(r)).collect()
    }

    /// The entries' and values' bytes (`data` before the restart words).
    #[cfg(test)]
    fn body(&self) -> &[u8] {
        &self.data[..self.rs as usize]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn last_key(&self) -> Option<&[u8]> {
        (self.len > 0).then_some(&self.data[self.lk as usize..])
    }

    /// Decode the entry at `pos.next` into `pos` (entry `pos.i`).
    #[inline]
    fn read(&self, pos: &mut BlockPos) {
        let b = &self.data[..];
        let mut p = pos.next as usize;
        let (shared, us, vf) = head(b, &mut p);
        let unshared = us >> 2;
        pos.shared = shared as u32;
        pos.key.truncate(shared);
        extend_short(&mut pos.key, &b[p..p + unshared]);
        p += unshared;
        if us & F_TS != 0 {
            pos.key.extend_from_slice(&self.ts);
        }
        if us & F_DERIVED != 0 {
            let ulen = pos.key.len() - 8;
            pos.dval.clear();
            pos.dval.extend_from_slice(&pos.key[..4]);
            pos.dval.extend_from_slice(&[0; 4]);
            pos.dval.extend_from_slice(&pos.key[ulen - vf..ulen]);
            pos.val_len = DERIVED;
        } else if vf == 0 {
            pos.val_len = TOMBSTONE;
        } else {
            pos.val_off = pos.vnext;
            pos.val_len = (vf - 1) as u32;
            pos.vnext += pos.val_len;
        }
        pos.next = p as u32;
    }

    /// `pos` at entry `i` (`len`: past the end).
    pub fn at(&self, i: usize, pos: &mut BlockPos) {
        if i >= self.len {
            pos.i = self.len;
            return;
        }
        let r = i >> self.rlog;
        pos.next = self.restart(r) as u32;
        pos.vnext = self.rval(r);
        pos.key.clear();
        pos.i = r << self.rlog;
        self.read(pos);
        while pos.i < i {
            pos.i += 1;
            self.read(pos);
        }
    }

    /// `pos` on to the next entry.
    pub fn step(&self, pos: &mut BlockPos) {
        pos.i += 1;
        if pos.i < self.len {
            self.read(pos);
        }
    }

    /// The whole key of restart `r` compared with `key` (inlined, as `first_restart`).
    #[inline(always)]
    fn restart_cmp(&self, r: usize, key: &[u8]) -> std::cmp::Ordering {
        let mut p = self.restart(r) as u32 as usize;
        let (_, us, _) = head(&self.data, &mut p);
        let stored = &self.data[p..p + (us >> 2)];
        cmp_split(stored, if us & F_TS != 0 { &self.ts } else { &[] }, key)
    }

    /// The first restart whose key is >= `key` (`restarts.len()`: none). (Inlined: `seek`, a
    /// point read's, keeps it in its body now that the multi-get calls it too.)
    #[inline(always)]
    fn first_restart(&self, key: &[u8]) -> usize {
        let n = self.nres();
        if n == 0 {
            return 0;
        }
        let c = self.common as usize;
        let m = c.min(key.len());
        // (Restart 0 stores its key's first `common` bytes whole.)
        let mut p = self.restart(0) as u32 as usize;
        head(&self.data, &mut p);
        match key[..m].cmp(&self.data[p..p + m]) {
            std::cmp::Ordering::Less => return 0,
            std::cmp::Ordering::Greater => return n,
            std::cmp::Ordering::Equal if key.len() < c => return 0,
            std::cmp::Ordering::Equal => {}
        }
        let w = (word(&key[c..]) >> 32) as u32;
        let word_of = |r: u64| (r >> 32) as u32;
        let (mut lo, mut hi) = (0, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if word_of(self.restart(mid)) < w {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        while lo < n && word_of(self.restart(lo)) == w && self.restart_cmp(lo, key).is_lt() {
            lo += 1;
        }
        lo
    }

    /// Ask for the cache lines a `seek` here reads first, its restarts' numbers and restart
    /// 0's key, ahead of it (a hint: `PointReader::get_sorted`).
    pub fn prefetch_restarts(&self) {
        prefetch(&self.data[self.rs as usize..self.rs as usize + 8 * self.nres()], 2);
        if self.nres() > 0 {
            prefetch(self.data.get(self.restart(0) as u32 as usize..).unwrap_or_default(), 1);
        }
    }

    /// The restart a `seek` of `key` decodes from: the one before the first whose key is >=
    /// `key` (`seek_from`). (Inlined: `seek`, every point read's, stays the one function it
    /// was: with these, or `first_restart`, as calls a point select took 0.4-0.9% more
    /// instructions.)
    #[inline(always)]
    pub fn restart_for(&self, key: &[u8]) -> usize {
        self.first_restart(key).saturating_sub(1)
    }

    /// Ask for restart `r`'s entries and the first of their values, which a `seek_from` it
    /// decodes (a hint).
    pub fn prefetch_restart(&self, r: usize) {
        if r < self.nres() {
            prefetch(self.data.get(self.restart(r) as u32 as usize..).unwrap_or_default(), 2);
            prefetch(self.data.get(self.rval(r) as usize..).unwrap_or_default(), 2);
        }
    }

    /// `pos` at the first entry whose key is >= `key` (`len`: none).
    pub fn seek(&self, key: &[u8], pos: &mut BlockPos) {
        self.seek_from(self.restart_for(key), key, pos)
    }

    /// `seek` of `key` from restart `r`, `restart_for(key)`.
    ///
    /// The entries from the restart's on, none of their keys made until the one found: each
    /// entry's key is the one before's first `shared` bytes and its stored tail, so with `m` the
    /// bytes the key before shares with `key` (and that key below `key`), an entry sharing more
    /// than `m` bytes with it is below `key` too, sharing `m`; one sharing at most `m` starts as
    /// `key` does, and its tail alone is compared with the rest of `key`. (A key made and
    /// compared for each entry, by calls to memcpy and memcmp, was most of a multi-get's seeks.)
    #[inline(always)]
    pub fn seek_from(&self, r: usize, key: &[u8], pos: &mut BlockPos) {
        if r >= self.nres() {
            pos.i = self.len;
            return;
        }
        use std::cmp::Ordering::*;
        let b = &self.data[..];
        let end = ((r + 1) << self.rlog).min(self.len);
        let (mut p, mut v) = (self.restart(r) as u32 as usize, self.rval(r));
        let mut m = 0;
        for i in r << self.rlog..end {
            let at = (p, v);
            let (shared, us, vf) = head(b, &mut p);
            let unshared = us >> 2;
            if shared <= m {
                // (Its first `shared` bytes are `key`'s: its tail, then the block's version,
                // against the rest.)
                let tail = &b[p..p + unshared];
                let rest = &key[shared.min(key.len())..];
                let n = common_prefix(tail, rest);
                let o = if n < tail.len() {
                    match rest.get(n) {
                        Some(&k) => tail[n].cmp(&k),
                        None => Greater,
                    }
                } else if us & F_TS != 0 {
                    let rest = &rest[n..];
                    let t = common_prefix(&self.ts, rest);
                    m = shared + n + t;
                    match (self.ts.get(t), rest.get(t)) {
                        (Some(x), Some(k)) => x.cmp(k),
                        (Some(_), None) => Greater,
                        (None, Some(_)) => Less,
                        (None, None) => Equal,
                    }
                } else if n < rest.len() {
                    Less
                } else {
                    Equal
                };
                if o != Less {
                    // (Made from `key`'s first `shared` bytes, as `read` makes it from the key
                    // before's.)
                    pos.key.clear();
                    pos.key.extend_from_slice(&key[..shared]);
                    (pos.next, pos.vnext) = (at.0 as u32, at.1);
                    pos.i = i;
                    self.read(pos);
                    return;
                }
                if n < tail.len() || us & F_TS == 0 {
                    m = shared + n;
                }
            }
            p += unshared;
            if us & F_DERIVED == 0 && vf > 0 {
                v += (vf - 1) as u32;
            }
        }
        // (The next restart's entry: its key is >= `key`, or the block ends.)
        (pos.next, pos.vnext) = (p as u32, v);
        pos.i = end - 1;
        self.step(pos);
    }

    /// `seek` from `pos`, an entry of this block: ascending lookups find the next key where
    /// they are or a few entries on (a nested loop's inner rows, an index's fetches), without
    /// a search. `pos` is at the first entry >= `key` when its key is >= `key` and the entry
    /// before is below it: it shares less with this one than `key` does (`shared` is what
    /// they share where it stops before the version; not at a restart, which shares nothing).
    pub fn seek_on(&self, key: &[u8], pos: &mut BlockPos) {
        if pos.i < self.len {
            let k = pos.key.as_slice();
            if k >= key {
                let shared = pos.shared as usize;
                let below = pos.i == 0
                    || (!pos.i.is_multiple_of(1 << self.rlog)
                        && shared + 8 < k.len()
                        && key.len() > shared
                        && key[..shared + 1] == k[..shared + 1]);
                if below {
                    return;
                }
            } else {
                for _ in 0..4 {
                    self.step(pos);
                    if pos.i >= self.len || pos.key.as_slice() >= key {
                        return;
                    }
                }
            }
        }
        self.seek(key, pos)
    }

    /// The value at `pos` (None: a tombstone).
    pub fn value(&self, pos: &BlockPos) -> Option<Bytes> {
        match pos.val_len {
            TOMBSTONE => None,
            DERIVED => Some(Bytes::copy_from_slice(&pos.dval)),
            n => {
                let (a, b) = (pos.val_off as usize, (pos.val_off + n) as usize);
                Some(match b <= self.data.len() {
                    true => self.data.slice(a..b),
                    false => self.col_row(pos),
                })
            }
        }
    }

    /// Where the value at `pos` is in the block's bytes, asked for (`prefetch`): None for a
    /// deletion or a value made from the key (`value` makes those), or a column block's row.
    pub fn value_span(&self, pos: &BlockPos) -> Option<(u32, u32)> {
        match pos.val_len {
            TOMBSTONE | DERIVED => None,
            n => {
                prefetch(self.data.get(pos.val_off as usize..(pos.val_off + n) as usize)?, 2);
                Some((pos.val_off, n))
            }
        }
    }

    /// A column block's section (format 5).
    #[inline]
    pub fn col_dir(&self) -> Option<&super::colblock::ColDir> {
        self.cols.as_deref().map(|c| &c.dir)
    }

    /// Whether it is a column block (format 5).
    #[inline]
    pub fn is_cols(&self) -> bool {
        self.cols.is_some()
    }

    /// The block's buffer (a column block's section is in it).
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// Whether the entry at `pos` has a value (not a deletion).
    #[inline]
    pub fn has_value(&self, pos: &BlockPos) -> bool {
        pos.val_len != TOMBSTONE
    }

    /// The row at `pos` of a column block, made alone from its columns (or a slice of its rows
    /// when they're made).
    #[cold]
    #[inline(never)]
    fn col_row(&self, pos: &BlockPos) -> Bytes {
        let c = self.cols.as_ref().expect("a column block's value");
        let off = (pos.val_off - c.base) as usize;
        match c.rows.get() {
            Some(rows) => rows.slice(off..off + pos.val_len as usize),
            None => {
                let mut v = Vec::with_capacity(pos.val_len as usize);
                c.dir.row(&self.data, pos.i, &mut v);
                Bytes::from(v)
            }
        }
    }

    /// A column block's rows' bytes, made from its columns once (kept while the block lives:
    /// a compaction's or a one-pass reader's block), and where its value offsets start.
    #[cold]
    #[inline(never)]
    pub fn col_rows(&self) -> (&Bytes, usize) {
        let c = self.cols.as_ref().expect("a column block's values");
        (c.rows.get_or_init(|| Bytes::from(c.dir.rows(&self.data))), c.base as usize)
    }

    /// The value at `off` (`len` bytes) of a column block, borrowed from its rows made.
    #[cold]
    #[inline(never)]
    fn col_value_ref(&self, off: u32, len: u32) -> &[u8] {
        let (rows, base) = self.col_rows();
        &rows[off as usize - base..(off + len) as usize - base]
    }

    /// The value `value_span` gave, borrowed.
    pub fn span_ref(&self, (off, n): (u32, u32)) -> &[u8] {
        &self.data[off as usize..(off + n) as usize]
    }

    /// The value `value_span` gave.
    pub fn span(&self, (off, n): (u32, u32)) -> Bytes {
        self.data.slice(off as usize..(off + n) as usize)
    }

    /// The value at `pos`, borrowed. (A column block's from its rows made, `col_rows`: a
    /// reader of a few rows asks for `value`.)
    pub fn value_ref<'a>(&'a self, pos: &'a BlockPos) -> Option<&'a [u8]> {
        match pos.val_len {
            TOMBSTONE => None,
            DERIVED => Some(&pos.dval),
            n => Some(match self.data.get(pos.val_off as usize..(pos.val_off + n) as usize) {
                Some(v) => v,
                None => self.col_value_ref(pos.val_off, n),
            }),
        }
    }

    /// Whether the entry at `pos` is a row with values stored out of line (a column block's
    /// never is).
    #[inline]
    pub fn external(&self, pos: &BlockPos) -> bool {
        match pos.val_len {
            TOMBSTONE => false,
            DERIVED => super::toast::has_external(&pos.dval),
            n => self.data.get(pos.val_off as usize..(pos.val_off + n) as usize).is_some_and(super::toast::has_external),
        }
    }

    /// `push_value` of a column block's row (`buf`: the batch's index of the block,
    /// `RowBatch::buf_block`), not made.
    pub fn push_col(&self, pos: &BlockPos, key: &[u8], buf: u32, batch: &mut super::batch::RowBatch) {
        debug_assert!(self.is_cols() && pos.val_len != TOMBSTONE && pos.val_len != DERIVED);
        batch.push_col(key, buf, pos.val_len as usize, pos.i as u32);
    }

    /// The entry at `pos` (not a tombstone) into `batch` under `key`: its value referenced in
    /// the block's bytes, or copied when derived. (Not a column block's: `push_col`.)
    pub fn push_value(&self, pos: &BlockPos, key: &[u8], batch: &mut super::batch::RowBatch) {
        debug_assert!(!self.is_cols());
        match pos.val_len {
            TOMBSTONE => debug_assert!(false, "a tombstone pushed"),
            DERIVED => batch.push_copy(key, &pos.dval),
            n => batch.push_in(key, &self.data, pos.val_off as usize, n as usize),
        }
    }

    /// The block without the entries `dead` names (bit i: entry i), in the form a writer
    /// would have made of the others (`decode_with` of them: its last key the last one left,
    /// as readers holding a block take its last key for one of its entries).
    pub fn without(&self, dead: &[u64]) -> Block {
        let mut raw = Vec::with_capacity(self.data.len() + self.len * 4);
        let mut pos = BlockPos::default();
        self.at(0, &mut pos);
        while pos.i < self.len {
            if dead.get(pos.i / 64).is_none_or(|w| w >> (pos.i % 64) & 1 == 0) {
                let v = self.value_ref(&pos);
                put_varint(&mut raw, 0);
                put_varint(&mut raw, pos.key.len() as u64);
                put_varint(&mut raw, v.map_or(0, |v| v.len() as u64 + 1));
                raw.extend_from_slice(&pos.key);
                raw.extend_from_slice(v.unwrap_or_default());
            }
            self.step(&mut pos);
        }
        Block::decode_with(&raw, None, self.rlog).expect("entries of a checked block")
    }

    /// The block as a format 1 run of command `cid` stores it: each value tagged 3 and `cid`
    /// in front, each deletion a value of tag 2 and `cid` (a run of format 2 read by its
    /// transaction: `SstReader::retagged`).
    fn tagged(&self, cid: &[u8]) -> Block {
        let mut raw = Vec::with_capacity(self.data.len() + self.len * (cid.len() + 5));
        let mut pos = BlockPos::default();
        self.at(0, &mut pos);
        while pos.i < self.len {
            let v = self.value_ref(&pos);
            put_varint(&mut raw, 0);
            put_varint(&mut raw, pos.key.len() as u64);
            put_varint(&mut raw, (1 + cid.len() + v.map_or(0, <[u8]>::len)) as u64 + 1);
            raw.extend_from_slice(&pos.key);
            raw.push(if v.is_some() { 3 } else { 2 });
            raw.extend_from_slice(cid);
            raw.extend_from_slice(v.unwrap_or_default());
            self.step(&mut pos);
        }
        Block::decode_with(&raw, None, self.rlog).expect("entries of a checked block")
    }

    /// The version every `F_TS` entry has (the block's).
    pub fn version(&self) -> u64 {
        !u64::from_be_bytes(self.ts)
    }

    /// The entries after `pos` into `batch` while each is a row (its value stored: not
    /// derived, not a deletion) of a version at or before `ts`, of another user key than the
    /// row before, and (`resolve`) without out-of-line values; up to `max`. The caller pushed
    /// `pos`'s entry as `batch`'s last row (under its user key) and knows every entry of the
    /// block is in range. Each key is the last row's first `shared` bytes and the entry's own
    /// (less its version, when stored), made in the batch: no `BlockPos` key per entry. `pos`
    /// ends on the first entry not taken (or past the end). How many it took, and the last
    /// one's version.
    #[inline(always)]
    pub fn run_rows(&self, pos: &mut BlockPos, ts: u64, max: usize, resolve: bool, batch: &mut super::batch::RowBatch) -> (usize, u64) {
        debug_assert!(!self.is_cols());
        self.run_rows_in::<false>(pos, ts, max, resolve, batch, 0)
    }

    /// `run_rows` of a column block: its rows into the batch as rows of buffer `cbuf`
    /// (`RowBatch::buf_block`), not made. (Never out of line: none of its rows are.)
    pub fn run_rows_col(&self, pos: &mut BlockPos, ts: u64, max: usize, batch: &mut super::batch::RowBatch, cbuf: u32) -> (usize, u64) {
        debug_assert!(self.is_cols());
        self.run_rows_in::<true>(pos, ts, max, false, batch, cbuf)
    }

    /// Row `r` of a column block (`len` bytes), made alone from its columns.
    pub fn row_at(&self, r: usize, len: usize) -> Bytes {
        let c = self.cols.as_ref().expect("a column block");
        let mut v = Vec::with_capacity(len);
        c.dir.row(&self.data, r, &mut v);
        Bytes::from(v)
    }

    #[inline(always)]
    fn run_rows_in<const COL: bool>(&self, pos: &mut BlockPos, ts: u64, max: usize, resolve: bool, batch: &mut super::batch::RowBatch, cbuf: u32) -> (usize, u64) {
        let b = &self.data[..];
        let row0 = pos.i as u32 + 1;
        let (mut p, mut v) = (pos.next as usize, pos.vnext as usize);
        let mut i = pos.i + 1;
        let Some(last) = batch.last_key() else {
            self.step(pos);
            return (0, 0);
        };
        // (The key being made, when it fits `KEY`: a key's bytes past those it shares with the
        // row before go in as a 16-byte move, the key into the batch as a `KEY`-byte one, each
        // cut back to its length: no copy call per row. Longer keys go the general way.)
        let mut kb = [0u8; KEY + 16];
        let mut klen = last.len();
        let mut short = klen <= KEY;
        if short {
            kb[..klen].copy_from_slice(last);
        }
        let own = self.version() <= ts;
        let mut ver = self.ts;
        let buf = if COL { cbuf } else { batch.buf_index(&self.data) };
        let mut n = 0;
        while n < max && i < self.len {
            let at = p;
            let (shared, us, vf) = head(b, &mut p);
            let unshared = us >> 2;
            // (The last row's user key again, an older version: shared whole.)
            if us & F_DERIVED != 0 || vf == 0 || shared >= klen {
                p = at;
                break;
            }
            // (The entry's user key's own bytes: all it stores at the block's version, else
            // all but its version's 8.)
            let ukn = match us & F_TS {
                0 if unshared >= 8 && !u64::from_be_bytes(b[p + unshared - 8..p + unshared].try_into().unwrap()) <= ts => unshared - 8,
                0 => {
                    p = at;
                    break;
                }
                _ if own => unshared,
                _ => {
                    p = at;
                    break;
                }
            };
            let len = vf - 1;
            let suffix = &b[p..p + ukn];
            // (At a restart, the key is stored whole: the last row's again is an older version.)
            let again = shared == 0
                && match short {
                    true => kb[..klen] == *suffix,
                    false => batch.last_key() == Some(suffix),
                };
            if again || (!COL && resolve && super::toast::has_external(&b[v..v + len])) {
                p = at;
                break;
            }
            let nk = shared + ukn;
            if short && nk <= KEY {
                match b.get(p..p + 16) {
                    Some(w) if ukn <= 16 => kb[shared..shared + 16].copy_from_slice(w),
                    _ => kb[shared..nk].copy_from_slice(suffix),
                }
                batch.push_key::<KEY>(kb[..KEY].try_into().unwrap(), nk, buf, v, len);
            } else {
                batch.push_shared(shared, suffix, buf, v, len);
                short = nk <= KEY;
                if short {
                    kb[..nk].copy_from_slice(batch.last_key().expect("a row"));
                }
            }
            ver = if ukn == unshared { self.ts } else { b[p + ukn..p + unshared].try_into().unwrap() };
            p += unshared;
            v += len;
            klen = nk;
            n += 1;
            i += 1;
        }
        if COL && n > 0 {
            batch.note_rows(n, row0);
        }
        if n > 0 {
            // (On the entry stopped at: its key from the last row's and its version.)
            pos.key.clear();
            pos.key.extend_from_slice(batch.last_key().expect("a row"));
            pos.key.extend_from_slice(&ver);
            (pos.i, pos.next, pos.vnext) = (i - 1, p as u32, v as u32);
        }
        self.step(pos);
        (n, !u64::from_be_bytes(ver))
    }
}

/// A key `zip_rows` makes in a buffer of its own: up to `KEY` bytes of user key and its version,
/// with room for a 16-byte move past them.
const ZK: usize = KEY + 8 + 16;

/// One block's side of `zip_rows`: the entry it is on, decoded, its whole key in `k`.
struct ZipSide<'a> {
    blk: &'a Block,
    i: usize,
    /// Where the entry and its value start (`at`, `vat`), where the next ones do (`p`, `v`).
    at: usize,
    vat: usize,
    p: usize,
    v: usize,
    k: [u8; ZK],
    klen: usize,
    shared: usize,
    /// The entry's value: `TOMBSTONE`, `DERIVED` or its length (at `vat`).
    vlen: u32,
    /// A derived value's key tail (`DERIVED`: the value is the key's first 4 bytes, 4 zero
    /// bytes and the user key's last `dtail` bytes).
    dtail: usize,
    /// Moved off the entry `pos` was on.
    moved: bool,
    /// On an entry whose key is too long for `k` (`k` still the entry before's): ended here.
    stuck: bool,
    /// The batch's index of the block's bytes (`RowBatch::buf_index`).
    buf: u32,
}

impl<'a> ZipSide<'a> {
    /// On `pos`'s entry (None: its key is too long for `k`).
    fn new(blk: &'a Block, pos: &BlockPos, buf: u32) -> Option<ZipSide<'a>> {
        let klen = pos.key.len();
        if !(8..=KEY + 8).contains(&klen) || pos.i >= blk.len {
            return None;
        }
        let mut k = [0u8; ZK];
        k[..klen].copy_from_slice(&pos.key);
        let vlen = pos.val_len;
        let vat = if vlen == TOMBSTONE || vlen == DERIVED { pos.vnext as usize } else { pos.val_off as usize };
        let (p, v, shared) = (pos.next as usize, pos.vnext as usize, pos.shared as usize);
        let dtail = if vlen == DERIVED { pos.dval.len().saturating_sub(8) } else { 0 };
        Some(ZipSide { blk, i: pos.i, at: 0, vat, p, v, k, klen, shared, vlen, dtail, moved: false, stuck: false, buf })
    }

    #[inline(always)]
    fn ended(&self) -> bool {
        self.i >= self.blk.len || self.stuck
    }

    #[inline(always)]
    fn ver(&self) -> u64 {
        !u64::from_be_bytes(self.k[self.klen - 8..self.klen].try_into().unwrap())
    }

    /// On to the next entry; false when it is past the block or its key is too long for `k`
    /// (the side ends there: `stuck`).
    #[inline(always)]
    fn step(&mut self) -> bool {
        self.moved = true;
        self.i += 1;
        if self.i >= self.blk.len {
            return false;
        }
        let b = &self.blk.data[..];
        let mut p = self.p;
        let (shared, us, vf) = head(b, &mut p);
        let unshared = us >> 2;
        let klen = shared + unshared + if us & F_TS != 0 { 8 } else { 0 };
        (self.at, self.vat) = (self.p, self.v);
        if !(8..=KEY + 8).contains(&klen) {
            self.stuck = true;
            return false;
        }
        match b.get(p..p + 16) {
            Some(w) if unshared <= 16 => self.k[shared..shared + 16].copy_from_slice(w),
            _ => self.k[shared..shared + unshared].copy_from_slice(&b[p..p + unshared]),
        }
        if us & F_TS != 0 {
            self.k[klen - 8..klen].copy_from_slice(&self.blk.ts);
        }
        self.p = p + unshared;
        self.vlen = if us & F_DERIVED != 0 {
            self.dtail = vf;
            DERIVED
        } else if vf == 0 {
            TOMBSTONE
        } else {
            self.v += vf - 1;
            (vf - 1) as u32
        };
        self.klen = klen;
        self.shared = shared;
        true
    }

    /// `pos` on the entry this side is on, as `Block::step` would have left it.
    fn hand_back(&self, pos: &mut BlockPos) {
        if !self.moved {
            return;
        }
        if self.i >= self.blk.len {
            pos.i = self.blk.len;
            return;
        }
        // (`read` keeps the key's first `shared` bytes, which the entry before's has too: `k`
        // holds this entry's key, or the one before's when stuck.)
        pos.key.clear();
        pos.key.extend_from_slice(&self.k[..self.klen]);
        (pos.next, pos.vnext, pos.i) = (self.at as u32, self.vat as u32, self.i);
        self.blk.read(pos);
    }
}

/// `a`'s first `n` bytes compared with `b`'s, 8 bytes at a time: the last 1-7 as the word they
/// start, the bytes past `n` shifted out. (A slice comparison of them was a call to memcmp at
/// every comparison of keys whose length isn't a multiple of 8: a row key is 17 bytes.)
#[inline(always)]
fn cmp_words(a: &[u8; ZK], b: &[u8; ZK], n: usize) -> std::cmp::Ordering {
    const { assert!(ZK.is_multiple_of(8)) };
    let n = n.min(ZK);
    let mut i = 0;
    while i + 8 <= n {
        let x = u64::from_be_bytes(a[i..i + 8].try_into().unwrap());
        let y = u64::from_be_bytes(b[i..i + 8].try_into().unwrap());
        if x != y {
            return x.cmp(&y);
        }
        i += 8;
    }
    if i == n {
        return std::cmp::Ordering::Equal;
    }
    // (`i` is a multiple of 8 below `n`, so `i + 8 <= ZK`.)
    let shift = 64 - 8 * (n - i) as u32;
    let x = u64::from_be_bytes(a[i..i + 8].try_into().unwrap()) >> shift;
    let y = u64::from_be_bytes(b[i..i + 8].try_into().unwrap()) >> shift;
    x.cmp(&y)
}

/// `a`'s first `n` bytes compared with `b`'s for `n` in 8..=32, by the words that start and end
/// them (overlapping: past the first word's equal bytes, the last word orders what follows).
#[inline(always)]
fn cmp_ends(a: &[u8], b: &[u8], n: usize) -> std::cmp::Ordering {
    if n >= 16 {
        let w = |s: &[u8], i: usize| u128::from_be_bytes(s[i..i + 16].try_into().unwrap());
        w(a, 0).cmp(&w(b, 0)).then_with(|| w(a, n - 16).cmp(&w(b, n - 16)))
    } else {
        let w = |s: &[u8], i: usize| u64::from_be_bytes(s[i..i + 8].try_into().unwrap());
        w(a, 0).cmp(&w(b, 0)).then_with(|| w(a, n - 8).cmp(&w(b, n - 8)))
    }
}

/// `a == b` for the short keys a merge passes from entry to entry (8 to 32 bytes: a row's key
/// is 17, an index entry's about 22) without a call to bcmp; others as slices.
#[inline(always)]
pub(crate) fn short_eq(a: &[u8], b: &[u8]) -> bool {
    let n = a.len();
    if n != b.len() {
        return false;
    }
    if (8..=32).contains(&n) { cmp_ends(a, b, n).is_eq() } else { a == b }
}

/// `a.cmp(b)` for short keys, as `short_eq`.
#[inline(always)]
pub(crate) fn short_cmp(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    let n = a.len().min(b.len());
    if (8..=32).contains(&n) { cmp_ends(a, b, n).then(a.len().cmp(&b.len())) } else { a.cmp(b) }
}

/// A zip's `bound` in a buffer as its sides' keys are, when it fits one: then compared with a
/// side's key by words (`past`), not by a call to memcmp for every entry.
struct ZipBound<'a> {
    b: &'a [u8],
    w: [u8; ZK],
}

impl<'a> ZipBound<'a> {
    fn new(b: &'a [u8]) -> ZipBound<'a> {
        let mut w = [0u8; ZK];
        if b.len() <= ZK {
            w[..b.len()].copy_from_slice(b);
        }
        ZipBound { b, w }
    }

    /// Whether key `k` (its first `klen` bytes) sorts at or past the bound.
    #[inline(always)]
    fn past(&self, k: &[u8; ZK], klen: usize) -> bool {
        let blen = self.b.len();
        if blen > ZK {
            return &k[..klen] >= self.b;
        }
        match cmp_words(k, &self.w, klen.min(blen)) {
            std::cmp::Ordering::Equal => klen >= blen,
            o => o.is_gt(),
        }
    }
}

/// What `zip_rows` did: the rows it pushed, whether it moved either side, and the user key it
/// decided last (`Scan::decided`: in `decided`) with that version's commit ts, and the deleted
/// keys it passed.
pub struct Zipped {
    pub rows: usize,
    pub moved: bool,
    pub decided_ts: u64,
    pub deleted: u32,
}

/// The most sources `zip_rows` merges.
pub const ZIP_MAX: usize = 4;

/// Blocks' entries merged into `batch` (up to `ZIP_MAX` blocks), from the entries their
/// positions are on (each the head of its source; together a scan's smallest heads, below
/// `bound`, the next source's head, if any): a key's newest version at or before `ts` as a row
/// (not a deletion), each of its versions in these blocks passed. Every entry of the blocks is
/// in the scan's range. It stops, the general merge left to read the entry, at a version newer
/// than `ts`, a derived value, an out-of-line value (`resolve`), a key at or past `bound`, a key
/// too long for its buffer, the end of any block, or `max` rows. The positions are left on each
/// side's first entry not passed, as `Block::step` leaves them. (A call of its own: the
/// single-source loops it sits beside compile as without it.)
#[inline(never)]
pub fn zip_rows(
    srcs: &mut [(&Block, &mut BlockPos)],
    bound: Option<&[u8]>,
    ts: u64,
    max: usize,
    resolve: bool,
    batch: &mut super::batch::RowBatch,
    decided: &mut Vec<u8>,
) -> Zipped {
    let mut z = Zipped { rows: 0, moved: false, decided_ts: 0, deleted: 0 };
    let mut s: smallvec::SmallVec<[ZipSide; ZIP_MAX]> = smallvec::SmallVec::new();
    for (b, p) in srcs.iter() {
        let buf = batch.buf_index(&b.data);
        match ZipSide::new(b, p, buf) {
            Some(side) => s.push(side),
            None => return z,
        }
    }
    let a = (bound, ts, max, resolve);
    let (u, ulen) = match s.len() {
        2 => zip_n::<2>((&mut s[..]).try_into().unwrap(), a, batch, &mut z),
        3 => zip_n::<3>((&mut s[..]).try_into().unwrap(), a, batch, &mut z),
        4 => zip_n::<4>((&mut s[..]).try_into().unwrap(), a, batch, &mut z),
        _ => return z,
    };
    if z.moved {
        decided.clear();
        decided.extend_from_slice(&u[..ulen]);
    }
    for (side, (_, p)) in s.iter().zip(srcs.iter_mut()) {
        side.hand_back(p);
    }
    z
}

/// `zip_rows`' merge of `N` sides (`bound`, `ts`, `max`, `resolve` its own): the user key it
/// decided last and its length.
#[inline(always)]
fn zip_n<const N: usize>(
    s: &mut [ZipSide; N],
    (bound, ts, max, resolve): (Option<&[u8]>, u64, usize, bool),
    batch: &mut super::batch::RowBatch,
    z: &mut Zipped,
) -> ([u8; ZK], usize) {
    let mut u = [0u8; ZK];
    let mut ulen = 0;
    let bound = bound.map(ZipBound::new);
    while z.rows < max {
        let mut m = 0;
        for j in 1..N {
            if cmp_words(&s[j].k, &s[m].k, s[j].klen.min(s[m].klen)).is_lt() {
                m = j;
            }
        }
        let x = &mut s[m];
        if bound.as_ref().is_some_and(|bd| bd.past(&x.k, x.klen)) {
            break;
        }
        let ver = x.ver();
        let xl = x.klen - 8;
        if ver > ts || x.vlen == DERIVED || xl > KEY {
            break;
        }
        if x.vlen == TOMBSTONE {
            z.deleted += 1;
        } else {
            let (off, len) = (x.vat, x.vlen as usize);
            if resolve && super::toast::has_external(&x.blk.data[off..off + len]) {
                break;
            }
            batch.push_key::<KEY>(x.k[..KEY].try_into().unwrap(), xl, x.buf, off, len);
            z.rows += 1;
        }
        z.moved = true;
        z.decided_ts = ver;
        // (The whole buffer: a fixed copy, not a call to memcpy for `xl` bytes. Only the first
        // `ulen` bytes are read.)
        u = x.k;
        ulen = xl;
        // The key's older versions passed: on its own side those sharing the whole user key
        // (stored whole at a restart: compared), on the others the same user key.
        while x.step() && (x.shared >= ulen || (x.shared == 0 && x.klen - 8 == ulen && cmp_words(&x.k, &u, ulen).is_eq())) {}
        let mut ended = false;
        for y in s.iter_mut() {
            while !y.ended() && y.klen - 8 == ulen && cmp_words(&y.k, &u, ulen).is_eq() {
                y.step();
            }
            ended |= y.ended();
        }
        if ended {
            break;
        }
    }
    (u, ulen)
}

/// Blocks' entries in key order, every version and deletion (a compaction's merge; up to
/// `ZIP_MAX` blocks), each passed to `f` (storage key, value: None for a deletion), from the
/// entries their positions are on (each the head of its source; together the smallest heads,
/// below `bound`, the next source's head, if any), while every block has one, below `bound`,
/// whose key fits a buffer. The positions are left on each side's first entry not passed, as
/// `Block::step` leaves them. Whether it passed any.
#[inline(never)]
pub fn zip_entries(srcs: &mut [(&Block, &mut BlockPos)], bound: Option<&[u8]>, f: &mut dyn FnMut(&[u8], Option<&[u8]>) -> Result<()>) -> Result<bool> {
    let mut s: smallvec::SmallVec<[ZipSide; ZIP_MAX]> = smallvec::SmallVec::new();
    for (b, p) in srcs.iter() {
        match ZipSide::new(b, p, 0) {
            Some(side) => s.push(side),
            None => return Ok(false),
        }
    }
    let moved = match s.len() {
        2 => entries_n::<2>((&mut s[..]).try_into().unwrap(), bound, f)?,
        3 => entries_n::<3>((&mut s[..]).try_into().unwrap(), bound, f)?,
        4 => entries_n::<4>((&mut s[..]).try_into().unwrap(), bound, f)?,
        _ => false,
    };
    for (side, (_, p)) in s.iter().zip(srcs.iter_mut()) {
        side.hand_back(p);
    }
    Ok(moved)
}

/// `zip_entries`' merge of `N` sides.
#[inline(always)]
fn entries_n<const N: usize>(s: &mut [ZipSide; N], bound: Option<&[u8]>, f: &mut dyn FnMut(&[u8], Option<&[u8]>) -> Result<()>) -> Result<bool> {
    let mut moved = false;
    let mut dval = [0u8; ZK];
    let bound = bound.map(ZipBound::new);
    loop {
        let mut m = 0;
        for j in 1..N {
            if cmp_words(&s[j].k, &s[m].k, s[j].klen.min(s[m].klen)).is_lt() {
                m = j;
            }
        }
        let x = &mut s[m];
        if bound.as_ref().is_some_and(|bd| bd.past(&x.k, x.klen)) {
            break;
        }
        let k = &x.k[..x.klen];
        let v = match x.vlen {
            TOMBSTONE => None,
            DERIVED => {
                let ulen = x.klen - 8;
                let n = 8 + x.dtail;
                dval[..4].copy_from_slice(&k[..4]);
                dval[4..8].fill(0);
                dval[8..n].copy_from_slice(&k[ulen - x.dtail..ulen]);
                Some(&dval[..n])
            }
            n => Some(&x.blk.data[x.vat..x.vat + n as usize]),
        };
        f(k, v)?;
        moved = true;
        if !x.step() {
            break;
        }
    }
    Ok(moved)
}

impl Block {

    /// The bytes it holds (what the block cache counts): its buffer's (`held`) and the `Arc`
    /// it is cached in, at the allocator's size.
    pub fn mem_bytes(&self) -> usize {
        let arc = 2 * std::mem::size_of::<usize>() + std::mem::size_of::<Block>();
        match &self.cols {
            None => self.held as usize + super::cache::alloc_bytes(arc),
            Some(c) => self.held as usize + super::cache::alloc_bytes(arc) + c.dir.mem_bytes(),
        }
    }

    /// What a buffer of `len` bytes holds: its piece of the cache's arena and the owner
    /// `Bytes` keeps of it (`buf`), else the heap's allocation and the header `Bytes` makes of
    /// it once it is cloned (a range read's values), each at the allocator's size.
    fn held_by(len: usize, buf: Option<&Buf>) -> u32 {
        use super::cache::alloc_bytes;
        (match buf {
            Some(b) => b.piece + Buf::owner_bytes(),
            None => alloc_bytes(len) + alloc_bytes(3 * std::mem::size_of::<usize>()),
        }) as u32
    }

    /// The block with its buffer copied into `buf` (of its length), a piece of the cache's
    /// arena.
    fn moved_to(mut self, mut buf: Buf) -> Block {
        buf.as_mut().copy_from_slice(&self.data);
        self.held = Block::held_by(buf.as_ref().len(), Some(&buf));
        self.pooled = true;
        self.data = buf.freeze();
        self
    }

    /// Every entry with its key whole, for readers that go backwards or read a block once.
    pub fn expand(&self) -> Expanded {
        if self.is_cols() {
            return self.expand_cols();
        }
        let mut keys = Vec::with_capacity(self.data.len());
        let mut ents = Vec::with_capacity(self.len);
        // (Derived values exist only as a cursor makes them: copied out, with the others.)
        let mut vals = Vec::new();
        let mut pos = BlockPos::default();
        self.at(0, &mut pos);
        while pos.i < self.len {
            keys.extend_from_slice(&pos.key);
            let (mut val_off, val_len) = (pos.val_off, pos.val_len);
            if self.derived && val_len != TOMBSTONE {
                val_off = vals.len() as u32;
                vals.extend_from_slice(self.value_ref(&pos).unwrap_or_default());
            }
            let val_len = if val_len == DERIVED { pos.dval.len() as u32 } else { val_len };
            ents.push(ExpandedEnt {
                key_end: keys.len() as u32,
                val_off,
                val_len,
            });
            self.step(&mut pos);
        }
        Expanded {
            data: if self.derived { Bytes::from(vals) } else { self.data.clone() },
            keys,
            ents,
        }
    }

    /// `expand` of a column block: its rows made into the `Expanded` (not kept with the block,
    /// which the cache may hold: a reverse scan's), the offsets into them.
    #[cold]
    #[inline(never)]
    fn expand_cols(&self) -> Expanded {
        let c = self.cols.as_ref().expect("a column block");
        let (rows, base) = match c.rows.get() {
            Some(r) => (r.clone(), c.base as usize),
            None => (Bytes::from(c.dir.rows(&self.data)), c.base as usize),
        };
        let mut keys = Vec::with_capacity(self.data.len());
        let mut ents = Vec::with_capacity(self.len);
        let mut pos = BlockPos::default();
        self.at(0, &mut pos);
        while pos.i < self.len {
            keys.extend_from_slice(&pos.key);
            ents.push(ExpandedEnt { key_end: keys.len() as u32, val_off: pos.val_off - base as u32, val_len: pos.val_len });
            self.step(&mut pos);
        }
        Expanded { data: rows, keys, ents }
    }

    /// A format 2 block as it was decompressed (`SstWriter::body2`): its entries and values
    /// are the compact form already, the restarts read from its end, the last key from the
    /// file's block index. Its buffer is `into`'s of its length when that gives one (the
    /// cache's arena, for a block going into the cache).
    fn load(raw: &[u8], last_key: &[u8], into: Option<&dyn Fn(usize) -> Option<Buf>>) -> Result<Block> {
        let bad = || internal("bad sst block");
        let n = raw.len();
        let t = raw.get(n.checked_sub(TRAILER).ok_or_else(bad)?..).ok_or_else(bad)?;
        let u32_at = |i: usize| u32::from_le_bytes(t[i..i + 4].try_into().unwrap()) as usize;
        let ts: [u8; 8] = t[..8].try_into().unwrap();
        let (len, nr) = (u32_at(8), u32_at(12));
        let common = u16::from_le_bytes([t[16], t[17]]) as usize;
        // (Flags: bit 0 a value is derived; bits 4-7 the restart interval's log.)
        let derived = t[18] & 1 != 0;
        let columns = t[18] & 2 != 0;
        let rlog = t[18] >> 4;
        let data_len = n.checked_sub(TRAILER + nr * 12).ok_or_else(bad)?;
        if !(1..=8).contains(&rlog) || nr != len.div_ceil(1 << rlog) {
            return Err(bad());
        }
        // (A column block (format 5): its section where the values were, its last 4 bytes
        // where it starts; the restarts' value offsets are into its rows' bytes from there.)
        let cdir = match columns {
            false => None,
            true => {
                let at = data_len.checked_sub(4).ok_or_else(bad)?;
                let vb = u32::from_le_bytes(raw[at..data_len].try_into().unwrap()) as usize;
                let d = super::colblock::ColDir::parse(raw, vb, data_len).filter(|d| d.n as usize == len && !derived).ok_or_else(bad)?;
                Some(d)
            }
        };
        let vend = cdir.as_ref().map_or(data_len, |d| d.vbase as usize + d.vlen as usize);
        let r = &raw[data_len..];
        if r[..nr * 8].as_chunks::<8>().0.iter().any(|c| u64::from_le_bytes(*c) as u32 as usize >= data_len)
            || r[nr * 8..nr * 12].as_chunks::<4>().0.iter().any(|c| u32::from_le_bytes(*c) as usize > vend)
        {
            return Err(bad());
        }
        // (The body as the file holds it up to its trailer, then the last key, in an allocation
        // of its own size: the entries, values and restart arrays are where `data` has them.)
        let lk = data_len + nr * 12;
        let total = lk + last_key.len();
        let buf = into.and_then(|f| f(total));
        let held = Block::held_by(total, buf.as_ref());
        let pooled = buf.is_some();
        // (A column block's value offsets moved to start at the buffer's end: `Block::cols`.)
        let shift = |d: &mut [u8]| {
            if let Some(c) = &cdir {
                let by = (total - c.vbase as usize) as u32;
                for w in d[data_len + nr * 8..data_len + nr * 12].as_chunks_mut::<4>().0 {
                    *w = (u32::from_le_bytes(*w) + by).to_le_bytes();
                }
            }
        };
        let data = match buf {
            Some(mut b) => {
                let d = b.as_mut();
                d[..lk].copy_from_slice(&raw[..lk]);
                d[lk..].copy_from_slice(last_key);
                shift(d);
                b.freeze()
            }
            None => {
                let mut data = Vec::with_capacity(total);
                data.extend_from_slice(&raw[..lk]);
                data.extend_from_slice(last_key);
                shift(&mut data);
                Bytes::from(data)
            }
        };
        Ok(Block {
            data,
            rs: data_len as u32,
            nr: nr as u32,
            lk: lk as u32,
            common: common as u32,
            len,
            ts,
            derived,
            rlog,
            pooled,
            held,
            cols: cdir.map(|dir| Box::new(ColBlk { dir, base: total as u32, rows: std::sync::OnceLock::new() })),
        })
    }

    /// Check a decompressed block of the file's format and write it in the compact form,
    /// with a whole key at every `RESTART`th entry (files written before restarts were
    /// share a prefix there).
    #[cfg(test)]
    fn decode(raw: Bytes) -> Result<Block> {
        Block::decode_with(&raw, None, RLOG)
    }

    /// `decode` of a block of a linked run (`SstMeta::commit_ts`) in the same pass as what
    /// `relink` makes of it: every key's version the commit's, the run's values plain ones.
    fn decode_linked(raw: &[u8], ts: u64) -> Result<Block> {
        Block::decode_with(raw, Some((!ts).to_be_bytes()), RLOG)
    }

    /// (`rlog`: the restart interval's log, `RLOG` but in tests.)
    fn decode_with(raw: &[u8], link: Option<[u8; 8]>, rlog: u8) -> Result<Block> {
        let bad = || internal("bad sst block");
        let mut restarts: Vec<u64> = Vec::with_capacity(raw.len() / 512 + 1);
        let mut out = Vec::with_capacity(raw.len());
        // (The values, put after every key when the block is done.)
        let mut vals = Vec::with_capacity(raw.len());
        let mut rvals: Vec<u32> = Vec::with_capacity(raw.len() / 512 + 1);
        let mut key: Vec<u8> = Vec::new();
        // (The last restart's key.)
        // (Every restart's key, and how much of the first one is stored.)
        let (mut rkeys, mut rends, mut stored0) = (Vec::new(), Vec::new(), 0);
        let mut ts = None;
        let mut derived = false;
        let (mut pos, mut n) = (0, 0usize);
        while pos < raw.len() {
            let (shared, unshared, vl) = entry_head(raw, &mut pos)?;
            let end = pos + unshared + vl.saturating_sub(1);
            if shared > key.len() || end > raw.len() {
                return Err(bad());
            }
            let suffix = &raw[pos..pos + unshared];
            // (What the key shares with the one before: the file's count may stop short, at
            // its restarts.)
            let common = shared + key[shared..].iter().zip(suffix).take_while(|(a, b)| a == b).count();
            key.truncate(shared);
            key.extend_from_slice(suffix);
            let mut value = (vl > 0).then(|| &raw[pos + unshared..end]);
            pos = end;
            let klen = key.len();
            if let Some(v) = link {
                // (User keys are prefix-free: what two keys share ends before either's
                // version, so no later key's shared bytes reach the version set here.)
                if klen < 8 || common > klen - 8 {
                    return Err(bad());
                }
                key[klen - 8..].copy_from_slice(&v);
                // (A run's value: a tag (1: present, 2: a command id follows), then it.)
                value = match value {
                    Some([tag, rest @ ..]) if tag & 1 != 0 => Some(match tag & 2 {
                        0 => rest,
                        _ => rest.get(4..).unwrap_or_default(),
                    }),
                    _ => None,
                };
            }
            // (The value field of the value kept: a linked run's has lost its tag.)
            let vl = value.map_or(0, |v| v.len() + 1);
            let ts = *ts.get_or_insert_with(|| {
                <[u8; 8]>::try_from(&key[klen.saturating_sub(8)..]).unwrap_or([0xFF; 8])
            });
            let mut flags = 0;
            let mut ukey = klen;
            if klen >= 8 && key[klen - 8..] == ts {
                flags |= F_TS;
                ukey -= 8;
            }
            let tail = value.and_then(|v| derived_tail(&key, v));
            if tail.is_some() {
                flags |= F_DERIVED;
                derived = true;
            }
            let share = if n.is_multiple_of(1 << rlog) {
                restarts.push(out.len() as u64);
                rvals.push(vals.len() as u32);
                rkeys.extend_from_slice(&key);
                rends.push(rkeys.len());
                if n == 0 {
                    stored0 = ukey;
                }
                0
            } else {
                common.min(ukey)
            };
            put_varint(&mut out, share as u64);
            put_varint(&mut out, ((ukey - share) << 2 | flags) as u64);
            put_varint(&mut out, tail.unwrap_or(vl) as u64);
            out.extend_from_slice(&key[share..ukey]);
            if let (Some(v), None) = (value, tail) {
                vals.extend_from_slice(v);
            }
            n += 1;
        }
        // (The values after every key: `vbase`.)
        let vbase = out.len() as u32;
        // (Copied into an allocation of its own size: shrinking the one sized for the raw
        // block keeps the raw block's bytes when it shrinks by less than half, as mimalloc's
        // realloc does, and a cache of blocks would hold them all.)
        let mut data = Vec::with_capacity(out.len() + vals.len() + restarts.len() * 12 + key.len());
        data.extend_from_slice(&out);
        data.extend_from_slice(&vals);
        rvals.iter_mut().for_each(|v| *v += vbase);
        let rkey_at = |i: usize| &rkeys[if i == 0 { 0 } else { rends[i - 1] }..rends[i]];
        let common = match rends.len() {
            0 => 0,
            k => {
                let (a, b) = (rkey_at(0), rkey_at(k - 1));
                a.iter().zip(b).take_while(|(x, y)| x == y).count().min(stored0)
            }
        };
        for (i, r) in restarts.iter_mut().enumerate() {
            *r |= word(&rkey_at(i)[common..]) >> 32 << 32;
        }
        let (data, rs, lk) = Block::assemble(data, &restarts, &rvals, &key);
        Ok(Block {
            held: Block::held_by(data.len(), None),
            data,
            rs,
            nr: restarts.len() as u32,
            lk,
            common: common as u32,
            len: n,
            ts: ts.unwrap_or([0xFF; 8]),
            derived,
            rlog,
            pooled: false,
            cols: None,
        })
    }
}

/// A block with every entry's key whole (`Block::expand`), indexed by entry: what reverse
/// scans and one-pass readers use.
pub struct Expanded {
    data: Bytes,
    keys: Vec<u8>,
    ents: Vec<ExpandedEnt>,
}

#[derive(Clone, Copy)]
struct ExpandedEnt {
    key_end: u32,
    val_off: u32,
    val_len: u32,
}

impl Expanded {
    pub fn len(&self) -> usize {
        self.ents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ents.is_empty()
    }

    pub fn key(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ents[i - 1].key_end as usize };
        &self.keys[start..self.ents[i].key_end as usize]
    }

    /// Entry `i`'s value (None: a tombstone).
    pub fn value(&self, i: usize) -> Option<Bytes> {
        let e = self.ents[i];
        (e.val_len != TOMBSTONE)
            .then(|| self.data.slice(e.val_off as usize..(e.val_off + e.val_len) as usize))
    }

    /// Entry `i`'s value, borrowed.
    pub fn value_ref(&self, i: usize) -> Option<&[u8]> {
        let e = self.ents[i];
        (e.val_len != TOMBSTONE)
            .then(|| &self.data[e.val_off as usize..(e.val_off + e.val_len) as usize])
    }

    /// The first entry whose key is >= `key`.
    pub fn seek(&self, key: &[u8]) -> usize {
        let (mut lo, mut hi) = (0, self.ents.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.key(mid) < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// Replace a storage key's version with `ts`.
pub fn set_ts(key: &mut [u8], ts: u64) {
    let n = key.len();
    if n >= 8 {
        key[n - 8..].copy_from_slice(&(!ts).to_be_bytes());
    }
}

/// A block of a linked run (`SstMeta::commit_ts`) in the store's format (the reference
/// `Block::decode_linked` is tested against): every key's version
/// is the commit's, and the run's values (a tag, the command id, the value: `txn::spill`)
/// are plain ones, a delete a tombstone. (A linked run has one version of each key.)
#[cfg(test)]
fn relink(raw: &[u8], ts: u64) -> Result<Bytes> {
    let mut out = Vec::with_capacity(raw.len());
    let (mut key, mut prev) = (Vec::new(), Vec::new());
    let (mut pos, mut n) = (0usize, 0usize);
    while pos < raw.len() {
        let (shared, unshared, vl) = entry_head(raw, &mut pos)?;
        key.truncate(shared);
        key.extend_from_slice(raw.get(pos..pos + unshared).ok_or_else(|| internal("truncated sst entry"))?);
        pos += unshared;
        let value = if vl == 0 {
            None
        } else {
            let v = raw.get(pos..pos + vl - 1).ok_or_else(|| internal("truncated sst entry"))?;
            pos += vl - 1;
            let tag = *v.first().ok_or_else(|| internal("a run value without its tag"))?;
            let at = if tag & 2 != 0 { 5 } else { 1 };
            (tag & 1 != 0).then(|| &v[at.min(v.len())..])
        };
        let mut k = key.clone();
        set_ts(&mut k, ts);
        let sh = if n.is_multiple_of(RESTART) {
            0
        } else {
            prev.iter().zip(&k).take_while(|(a, b)| a == b).count()
        };
        put_varint(&mut out, sh as u64);
        put_varint(&mut out, (k.len() - sh) as u64);
        put_varint(&mut out, value.map_or(0, |v| v.len() as u64 + 1));
        out.extend_from_slice(&k[sh..]);
        if let Some(v) = value {
            out.extend_from_slice(v);
        }
        prev = k;
        n += 1;
    }
    Ok(Bytes::from(out))
}

/// A zstd dictionary for blocks like `samples` (at least one): about `size` bytes of their
/// content (an even share from the middle of each) with the entropy tables of their
/// statistics at `ZSTD_LEVEL` (`ZDICT_finalizeDictionary`: no search for the best content,
/// a compression of the samples' worth of time).
pub(crate) fn finalize_dict(samples: &[&[u8]], size: usize) -> Option<Vec<u8>> {
    use zstd::zstd_safe::zstd_sys;
    let per = (size / samples.len().max(1)).max(64);
    let mut content = Vec::with_capacity(size + per);
    let (mut flat, mut sizes) = (Vec::new(), Vec::with_capacity(samples.len()));
    for s in samples {
        if content.len() < size {
            let n = per.min(s.len());
            let at = (s.len() - n) / 2;
            content.extend_from_slice(&s[at..at + n]);
        }
        flat.extend_from_slice(s);
        sizes.push(s.len());
    }
    let mut out = vec![0u8; content.len() + (16 << 10)];
    let params = zstd_sys::ZDICT_params_t { compressionLevel: ZSTD_LEVEL, notificationLevel: 0, dictID: 0 };
    // SAFETY: every pointer is to a live buffer of the length passed with it; `sizes` sums
    // to `flat`'s length.
    let r = unsafe {
        zstd_sys::ZDICT_finalizeDictionary(
            out.as_mut_ptr().cast(),
            out.len(),
            content.as_ptr().cast(),
            content.len(),
            flat.as_ptr().cast(),
            sizes.as_ptr(),
            sizes.len() as u32,
            params,
        )
    };
    // SAFETY: a plain query of a return code.
    if unsafe { zstd_sys::ZDICT_isError(r) } != 0 {
        return None;
    }
    out.truncate(r);
    Some(out)
}

/// What reading a block from its file takes besides the block it makes: the bytes read, a buffer
/// they are decompressed into and a zstd context, kept from read to read (`SCRATCH`) rather
/// than allocated for each (a 6-16 kB buffer of each and a 92 kB context a miss: the buffers'
/// sizes are the cached blocks', so freed among them in the allocator's pages they held pages
/// the cache's blocks only partly filled). The context remembers the dictionary it last used
/// (by address): zstd decodes a block whose context last had another dictionary as "cold",
/// prefetching the dictionary's content and tables and through its slower prefetching sequence
/// decoder (`ZSTD_decompressSequencesLong`, 39% of TPC-H q09's cycles at 256 MB with one
/// context a thread, as a heap fetch's blocks come from file after file); a context that last
/// had it decodes it "hot". At most `SCRATCH_KEPT` are kept, the least recently returned
/// dropped first (a context about 92 kB, buffers up to `SCRATCH_BYTES` each): a query's reads
/// go from file to file (a join's probes, a parallel scan's workers), and a pool that handed
/// out its newest context for another dictionary undid the warmest one (a fifth of TPC-H
/// q02's cycles at 256 MB in the cold decoder).
struct Scratch {
    dict: usize,
    dctx: zstd::zstd_safe::DCtx<'static>,
    read: Vec<u8>,
    out: Vec<u8>,
}
static SCRATCH: parking_lot::Mutex<Vec<Scratch>> = parking_lot::Mutex::new(Vec::new());
const SCRATCH_KEPT: usize = 32;
const SCRATCH_BYTES: usize = 256 << 10;

/// A kept `Scratch` whose context last used dictionary `dict` (0: none), the most recently
/// returned of them; else a new one (the kept ones keep their dictionaries).
fn scratch(dict: usize) -> Scratch {
    let taken = {
        let mut kept = SCRATCH.lock();
        kept.iter().rposition(|s| s.dict == dict).map(|i| kept.remove(i))
    };
    taken.unwrap_or_else(|| Scratch { dict: 0, dctx: zstd::zstd_safe::DCtx::create(), read: Vec::new(), out: Vec::new() })
}

/// `s` kept (the least recently returned dropped past `SCRATCH_KEPT`).
fn keep(mut s: Scratch) {
    for b in [&mut s.read, &mut s.out] {
        if b.capacity() > SCRATCH_BYTES {
            *b = Vec::new();
        }
    }
    let dropped = {
        let mut kept = SCRATCH.lock();
        kept.push(s);
        (kept.len() > SCRATCH_KEPT).then(|| kept.remove(0))
    };
    drop(dropped);
}

impl SstReader {
    pub fn open(dir: &Path, mut meta: SstMeta, cache: Arc<BlockCache>) -> Result<SstReader> {
        let path = sst_path(dir, meta.id);
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        if len < FOOTER_LEN as u64 {
            return Err(internal(format!("sst {} is truncated", path.display())));
        }
        let mut footer = [0u8; FOOTER4_LEN];
        let at = len.saturating_sub(FOOTER4_LEN as u64);
        let got = (len - at) as usize;
        file.read_exact_at(&mut footer[FOOTER4_LEN - got..], at)?;
        // (The footer's words counted from its end: the magic is `back(0)`.)
        let back = |i: usize| u64::from_le_bytes(footer[FOOTER4_LEN - 8 * i - 8..FOOTER4_LEN - 8 * i].try_into().unwrap());
        // (Format 1's six words are the last six; format 2's seven end with its format and
        // magic; format 4's nine start with its dictionary's offset and length.)
        let (v2, dict) = match back(0) {
            MAGIC => (false, None),
            MAGIC2 if got >= FOOTER2_LEN => match back(1) {
                f if f > FORMAT_COLS as u64 => {
                    return Err(internal(format!(
                        "sst {} is of storage format {f}, newer than this build reads ({FORMAT_COLS})",
                        path.display()
                    )));
                }
                f if f == FORMAT_COLS as u64 && got == FOOTER4_LEN => {
                    meta.cols = true;
                    (true, (back(7) != 0).then(|| (back(8), back(7))))
                }
                f if f == FORMAT_DICT as u64 && got == FOOTER4_LEN => (true, Some((back(8), back(7)))),
                f if f == FORMAT as u64 => (true, None),
                _ => return Err(internal(format!("sst {} has a bad footer", path.display()))),
            },
            _ => return Err(internal(format!("sst {} has a bad footer", path.display()))),
        };
        // (index offset, length, bloom offset, length: the first four of format 2's seven.)
        let f = |i: usize| back(if v2 { 6 - i } else { 5 - i });
        let ddict = match dict {
            None => None,
            Some((off, n)) => {
                let mut d = vec![0u8; n as usize];
                file.read_exact_at(&mut d, off)?;
                let Some(c) = d.len().checked_sub(4) else { return Err(internal(format!("sst {} has a bad footer", path.display()))) };
                if crc32fast::hash(&d[..c]).to_le_bytes() != d[c..] {
                    return Err(crate::error::Error::new(
                        crate::error::code::DATA_CORRUPTED,
                        format!("checksum mismatch in {} dictionary", path.display()),
                    ));
                }
                Some(zstd::dict::DecoderDictionary::copy(&d[..c]))
            }
        };
        let mut index_buf = vec![0u8; f(1) as usize];
        file.read_exact_at(&mut index_buf, f(0))?;
        let mut bloom = vec![0u8; f(3) as usize];
        file.read_exact_at(&mut bloom, f(2))?;
        let mut pos = 0;
        let n = get_varint(&index_buf, &mut pos)? as usize;
        let mut index = BlockIndex {
            keys: Vec::with_capacity(index_buf.len()),
            ends: Vec::with_capacity(n),
            at: Vec::with_capacity(n),
            common: 0,
            words: Vec::new(),
            top: Vec::new(),
        };
        for _ in 0..n {
            let kl = get_varint(&index_buf, &mut pos)? as usize;
            let k = index_buf.get(pos..pos + kl).ok_or_else(|| internal("bad sst index"))?;
            let start = index.keys.len();
            index.keys.extend_from_slice(k);
            if let Some(ts) = meta.commit_ts {
                set_ts(&mut index.keys[start..], ts);
            }
            pos += kl;
            let off = get_varint(&index_buf, &mut pos)?;
            let l = get_varint(&index_buf, &mut pos)? as u32;
            if v2 {
                // (The block's entry count: not used yet.)
                get_varint(&index_buf, &mut pos)?;
            }
            index.ends.push(index.keys.len() as u32);
            index.at.push((off, l));
        }
        index.keys.shrink_to_fit();
        index.index_words();
        // (Its places are a block's entries as the file holds them, which every form a block
        // is read in keeps: format 1's, a linked run's, format 2's.)
        let dead = match meta.dv {
            None => None,
            Some(g) => {
                let set = DeadSet::decode(&std::fs::read(dead_path(dir, meta.id, g))?)
                    .map_err(|e| internal(format!("sst {}: {e}", path.display())))?;
                if set.count() != meta.dead {
                    return Err(internal(format!("sst {}: its dead-entry set holds {} entries, not {}", path.display(), set.count(), meta.dead)));
                }
                Some(Arc::new(set))
            }
        };
        let cache_id = cache.next_file_id();
        Ok(SstReader {
            meta,
            file,
            path,
            v2,
            index,
            bloom_mod: FastMod::new(bloom.len() as u64 * 8),
            bloom,
            cache,
            cache_id,
            obsolete: AtomicBool::new(false),
            dead,
            dead_obsolete: AtomicBool::new(false),
            retag: None,
            ddict,
            dict_at: dict,
            index_at: (f(0), f(1)),
        })
    }

    /// Its dictionary as the file holds it (None: it has none), read again (a compaction
    /// copying its blocks writes it into its output, `SstWriter::adopt`).
    pub fn dict_bytes(&self) -> Result<Option<Vec<u8>>> {
        let Some((off, n)) = self.dict_at else { return Ok(None) };
        let mut d = vec![0u8; n as usize];
        self.file.read_exact_at(&mut d, off)?;
        d.truncate(d.len().saturating_sub(4));
        Ok(Some(d))
    }

    /// Each block's entry count, from its index read again (format 2: `SstWriter::finish`).
    pub fn block_entries(&self) -> Result<Vec<u32>> {
        if !self.v2 {
            return Err(internal("format 1 indexes hold no entry counts"));
        }
        let mut b = vec![0u8; self.index_at.1 as usize];
        self.file.read_exact_at(&mut b, self.index_at.0)?;
        let mut pos = 0;
        let n = get_varint(&b, &mut pos)? as usize;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let kl = get_varint(&b, &mut pos)? as usize;
            pos += kl;
            get_varint(&b, &mut pos)?;
            get_varint(&b, &mut pos)?;
            out.push(get_varint(&b, &mut pos)? as u32);
        }
        if out.len() != self.block_count() {
            return Err(internal("bad sst index"));
        }
        Ok(out)
    }

    /// Block `i` as the file stores it (its codec byte to its crc), checked, into `out`.
    pub fn stored_block(&self, i: usize, out: &mut Vec<u8>) -> Result<()> {
        let (off, len) = self.index.at[i];
        out.resize(len as usize, 0);
        self.file.read_exact_at(out, off)?;
        let c = out.len().checked_sub(4).filter(|&c| c > 0).ok_or_else(|| internal("sst block too short"))?;
        if crc32fast::hash(&out[..c]).to_le_bytes() != out[c..] {
            return Err(crate::error::Error::new(
                crate::error::code::DATA_CORRUPTED,
                format!("checksum mismatch in {} block {i}", self.path.display()),
            ));
        }
        Ok(())
    }

    /// Block `i`'s last key (from the index).
    pub fn block_last_key(&self, i: usize) -> &[u8] {
        self.index.key(i)
    }

    /// A run of format 2 (`SstWriter::create_run`), every entry of command `cid`, read as
    /// its transaction reads its runs (`txn::spill`'s values): a value as tag 3 (present, a
    /// command id follows), `cid`, the value; a deletion as tag 2 and `cid`. A run of format
    /// 1 stores them, and is read as it is.
    pub fn retagged(mut self, cid: u32) -> SstReader {
        if self.v2 {
            self.retag = Some(cid.to_be_bytes().into());
        }
        self
    }

    /// The file read again with `dead` as its dead-entry set, written to its file of
    /// generation `generation` (made durable before a manifest names it): its blocks are read
    /// afresh, under a cache id of its own, so none of this reader's cached ones is read.
    pub fn with_dead(&self, dir: &Path, dead: &DeadSet, generation: u64) -> Result<SstReader> {
        let path = dead_path(dir, self.meta.id, generation);
        super::fileio::sync(|| super::durable::replace(&path, &dead.encode(), true))?;
        let mut meta = self.meta.clone();
        meta.dv = Some(generation);
        meta.dead = dead.count();
        SstReader::open(dir, meta, self.cache.clone()).inspect_err(|_| {
            let _ = super::durable::remove_file(&path);
        })
    }

    /// Its dead entries (None: none).
    pub fn dead(&self) -> Option<&DeadSet> {
        self.dead.as_deref()
    }

    /// Entries a reader sees: those written less the dead ones.
    pub fn live_entries(&self) -> u64 {
        self.meta.entries - self.meta.dead
    }

    /// A newer reader of the file has another dead-entry set (`with_dead`): this one's set
    /// file is deleted when the reader goes.
    pub fn mark_dead_obsolete(&self) {
        self.dead_obsolete.store(true, Ordering::Release);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Delete the file once the last reader drops it.
    pub fn mark_obsolete(&self) {
        self.obsolete.store(true, Ordering::Release);
    }

    /// Whether the file may hold a user key in [start, end) (an empty `end`: unbounded).
    pub fn overlaps(&self, start: &[u8], end: &[u8]) -> bool {
        user_key(&self.meta.last_key) >= start && (end.is_empty() || user_key(&self.meta.first_key) < end)
    }

    pub fn may_contain(&self, user_key: &[u8]) -> bool {
        if self.bloom.is_empty() {
            return true;
        }
        let h = if self.v2 { hash64_2(user_key) } else { hash64(user_key) };
        bloom_bits(h, self.bloom_mod).all(|bit| self.bloom[bit / 8] & (1 << (bit % 8)) != 0)
    }

    /// Whether the file's first block is compressed (what the writer did with them all,
    /// but for blocks compression didn't shrink).
    pub fn first_block_compressed(&self) -> Result<bool> {
        let Some((off, _)) = self.index.at.first() else { return Ok(false) };
        let mut b = [0u8; 1];
        self.file.read_exact_at(&mut b, *off)?;
        Ok(b[0] != 0)
    }

    /// The file's format (`FORMAT` or 1).
    pub fn format(&self) -> u32 {
        match (self.v2, &self.ddict) {
            (false, _) => 1,
            _ if self.meta.cols => FORMAT_COLS,
            (true, None) => FORMAT,
            (true, Some(_)) => FORMAT_DICT,
        }
    }

    /// The bytes blocks `a..=b` take in the file.
    pub fn stored_bytes(&self, a: usize, b: usize) -> u64 {
        match (self.index.at.get(a), self.index.at.get(b)) {
            (Some(x), Some(y)) if a <= b => y.0 + y.1 as u64 - x.0,
            _ => 0,
        }
    }

    pub fn block_count(&self) -> usize {
        self.index.at.len()
    }

    /// Whether the file holds an entry whose user key is in [lo, hi] (both ends included):
    /// its range alone answers when its first key is in it; else the block index finds the
    /// one block that would hold it, read without filling the cache.
    pub fn has_user_key_in(&self, lo: &[u8], hi: &[u8]) -> Result<bool> {
        let (first, last) = (user_key(&self.meta.first_key), user_key(&self.meta.last_key));
        if last < lo || first > hi {
            return Ok(false);
        }
        if first >= lo {
            return Ok(true);
        }
        // (`lo` itself sorts before each of its versions: the first entry at or past it.)
        let i = self.find_block(lo);
        if i >= self.block_count() {
            return Ok(false);
        }
        let b = self.block_with(i, false)?;
        let mut pos = BlockPos::default();
        b.seek(lo, &mut pos);
        while pos.index() < b.len() {
            let uk = user_key(pos.key());
            if uk > hi {
                return Ok(false);
            }
            if uk >= lo {
                return Ok(true);
            }
            b.step(&mut pos);
        }
        // (Past the block's end: the next block's first entry, the next key after `lo`.)
        Ok(i + 1 < self.block_count() && {
            let b = self.block_with(i + 1, false)?;
            b.at(0, &mut pos);
            pos.index() < b.len() && user_key(pos.key()) <= hi
        })
    }

    /// Index of the first block whose last key is >= `key`.
    pub fn find_block(&self, key: &[u8]) -> usize {
        self.index.find(key)
    }

    /// Whether `key` is at or below block `b`'s last key: past a key `find_block` found in
    /// `b`, it is in `b` too.
    pub fn block_holds(&self, b: usize, key: &[u8]) -> bool {
        b < self.index.ends.len() && key <= self.index.key(b)
    }

    pub fn block(&self, i: usize) -> Result<CachedBlock> {
        self.block_with(i, true)
    }

    /// Block `i`, from the cache or the file; one read from the file goes into the cache
    /// when `fill` (compaction's reads don't: nobody asks for an input's blocks again, and
    /// they would push out what readers use).
    pub fn block_with(&self, i: usize, fill: bool) -> Result<CachedBlock> {
        if let Some(b) = self.cache.get(self.cache_id, i as u32) {
            return Ok(b);
        }
        let fill = fill && !super::in_read_once();
        // (A bulk read's block goes through a ring in a full cache: `BlockCache::insert_bulk`.)
        let bulk = fill && super::in_bulk_read();
        // (A block going into the cache has its buffer in the cache's own memory, `cache::Buf`.)
        let buf = |n: usize| self.cache.buffer(self.cache_id, i as u32, n, bulk);
        let dead = self.dead.as_ref().and_then(|d| d.block(i));
        let block = self.raw_block_in(i, (fill && dead.is_none()).then_some(&buf as &dyn Fn(usize) -> Option<Buf>))?;
        let mut block = match dead {
            Some(dead) => block.without(dead),
            None => block,
        };
        if !fill {
            return Ok(Arc::new(block));
        }
        if !block.pooled
            && let Some(b) = buf(block.data.len())
        {
            block = block.moved_to(b);
        }
        let block = Arc::new(block);
        let size = block.mem_bytes();
        if bulk {
            self.cache.insert_bulk(self.cache_id, i as u32, block.clone(), size);
        } else {
            self.cache.insert(self.cache_id, i as u32, block.clone(), size);
        }
        Ok(block)
    }

    /// Block `i` as the file holds it, its dead entries too (they are in their places: what
    /// `DeadSet` counts), read from the file without the cache.
    pub fn raw_block(&self, i: usize) -> Result<Block> {
        self.raw_block_in(i, None)
    }

    /// `raw_block`, its buffer `into`'s when that gives one (`Block::load`).
    fn raw_block_in(&self, i: usize, into: Option<&dyn Fn(usize) -> Option<Buf>>) -> Result<Block> {
        self.with_body(i, |raw| match self.meta.commit_ts {
            _ if self.v2 => {
                let mut b = Block::load(raw, self.index.key(i), into.filter(|_| self.retag.is_none()))?;
                // (A linked run of format 2: one version for every key, the block's, which
                // is the commit's.)
                if let Some(ts) = self.meta.commit_ts {
                    b.ts = (!ts).to_be_bytes();
                }
                Ok(match &self.retag {
                    Some(cid) => b.tagged(cid),
                    None => b,
                })
            }
            Some(ts) => Block::decode_linked(raw, ts),
            None => Block::decode_with(raw, None, RLOG),
        })
    }

    /// Block `i`'s body as stored, checked (its crc) and decompressed.
    #[cfg(test)]
    pub(crate) fn body(&self, i: usize) -> Result<Vec<u8>> {
        self.with_body(i, |b| Ok(b.to_vec()))
    }

    /// `f` of block `i`'s body as stored, checked (its crc) and decompressed, in a kept
    /// `Scratch`'s buffers.
    fn with_body<R>(&self, i: usize, f: impl FnOnce(&[u8]) -> Result<R>) -> Result<R> {
        let key = self.ddict.as_ref().map_or(0, |d| std::ptr::from_ref(d) as usize);
        let mut s = scratch(key);
        let r = self.body_in(i, &mut s, f);
        keep(s);
        r
    }

    fn body_in<R>(&self, i: usize, s: &mut Scratch, f: impl FnOnce(&[u8]) -> Result<R>) -> Result<R> {
        let (off, len) = self.index.at[i];
        let len = len as usize;
        if s.read.len() < len {
            s.read.resize(len, 0);
        }
        let buf = &mut s.read[..len];
        self.file.read_exact_at(buf, off)?;
        if len < 5 {
            return Err(internal("sst block too short"));
        }
        let crc_pos = len - 4;
        let crc = u32::from_le_bytes(buf[crc_pos..].try_into().unwrap());
        if crc32fast::hash(&buf[..crc_pos]) != crc {
            return Err(crate::error::Error::new(
                crate::error::code::DATA_CORRUPTED,
                format!("checksum mismatch in {} block {i}", self.path.display()),
            ));
        }
        // (A compressed block's raw length; a raw block's first bytes, unused.)
        let n = u32::from_le_bytes(buf[1..5].try_into().unwrap()) as usize;
        let src = &buf[5..crc_pos];
        let zstd_err = |e: usize| internal(format!("zstd: {}", zstd::zstd_safe::get_error_name(e)));
        match (buf[0], &self.ddict) {
            (0, _) => return f(&buf[1..crc_pos]),
            (1, _) => {
                s.out.clear();
                s.out.reserve(n);
                // (Its dictionary, if it had one, is no longer the context's.)
                s.dict = 0;
                s.dctx.decompress(&mut s.out, src).map_err(zstd_err)?;
            }
            (2, _) if self.v2 => {
                s.out.clear();
                s.out.resize(n, 0);
                let got = lz4_flex::block::decompress_into(src, &mut s.out).map_err(|e| internal(format!("lz4: {e}")))?;
                s.out.truncate(got);
            }
            (3, Some(d)) => {
                s.out.clear();
                s.out.reserve(n);
                let r = s.dctx.decompress_using_ddict(&mut s.out, src, d.as_ddict());
                s.dict = std::ptr::from_ref(d) as usize;
                r.map_err(zstd_err)?;
            }
            _ => return Err(internal("unknown block compression")),
        }
        if s.out.len() != n {
            return Err(internal("bad sst block"));
        }
        f(&s.out)
    }
}

/// A file's blocks: each one's last key (one buffer), offset and length. A point read's
/// `find` searches `words`, one number per block: a search over the keys themselves
/// (a vector each) missed the cache at every step, a sixth of TPC-H q20's fetches.
struct BlockIndex {
    keys: Vec<u8>,
    ends: Vec<u32>,
    at: Vec<(u64, u32)>,
    /// The bytes every last key starts with.
    common: usize,
    /// Each last key's 8 bytes after `common`, as a big-endian number (zeros past its
    /// end): a smaller word is a smaller key; equal words leave it to the keys.
    words: Vec<u64>,
    /// Every `TOP`th word: the search starts here (a few cache lines), then reads one run
    /// of `TOP` words.
    top: Vec<u64>,
}

const TOP: usize = 64;

/// Ask for the first `lines` cache lines of `b` ahead of their use: a hint, which the
/// hardware's own prefetching follows on from (nothing on other targets).
#[inline(always)]
fn prefetch(b: &[u8], lines: usize) {
    #[cfg(target_arch = "x86_64")]
    for off in (0..b.len().min(lines * 64)).step_by(64) {
        // SAFETY: the address is inside `b`; a prefetch reads nothing the program sees.
        unsafe { std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(b.as_ptr().add(off).cast()) }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (b, lines);
}

fn word(b: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    let n = b.len().min(8);
    w[..n].copy_from_slice(&b[..n]);
    u64::from_be_bytes(w)
}

impl BlockIndex {
    fn key(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
        &self.keys[start..self.ends[i] as usize]
    }

    fn index_words(&mut self) {
        let n = self.ends.len();
        if n == 0 {
            return;
        }
        // (Sorted keys: what the first and last share, all share.)
        let (a, b) = (self.key(0), self.key(n - 1));
        self.common = a.iter().zip(b).take_while(|(x, y)| x == y).count();
        let words: Vec<u64> = (0..n).map(|i| word(&self.key(i)[self.common..])).collect();
        self.top = words.iter().step_by(TOP).copied().collect();
        self.words = words;
    }

    /// The first block whose last key is >= `key` (`at.len()`: none).
    fn find(&self, key: &[u8]) -> usize {
        let n = self.words.len();
        if n == 0 {
            return 0;
        }
        let c = self.common;
        let m = c.min(key.len());
        match key[..m].cmp(&self.key(0)[..m]) {
            std::cmp::Ordering::Less => return 0,
            std::cmp::Ordering::Greater => return n,
            // (A prefix of what every last key starts with: below them all.)
            std::cmp::Ordering::Equal if key.len() < c => return 0,
            std::cmp::Ordering::Equal => {}
        }
        let w = word(&key[c..]);
        // (Sampled word j is words[j * TOP]: the first word >= w is after the last sample
        // below it and at or before the next.)
        let j = self.top.partition_point(|&x| x < w);
        let from = if j == 0 { 0 } else { (j - 1) * TOP + 1 };
        let to = (j * TOP + 1).min(n);
        let lo = from + self.words[from..to].partition_point(|&x| x < w);
        // (Equal words are few but for keys alike in their first 8 bytes past `common`.)
        let mut hi = lo;
        while hi < n && self.words[hi] == w {
            hi += 1;
            if hi - lo == 8 {
                hi = lo + self.words[lo..].partition_point(|&x| x == w);
                break;
            }
        }
        let (mut a, mut b) = (lo, hi);
        while a < b {
            let mid = (a + b) / 2;
            if self.key(mid) < key {
                a = mid + 1;
            } else {
                b = mid;
            }
        }
        a
    }
}

impl Drop for SstReader {
    fn drop(&mut self) {
        self.cache.forget_file(self.cache_id);
        let dead = || self.meta.dv.map(|g| dead_path(self.path.parent().unwrap_or(Path::new(".")), self.meta.id, g));
        if self.obsolete.load(Ordering::Acquire) {
            let _ = super::durable::remove_file(&self.path);
            if let Some(p) = dead() {
                let _ = super::durable::remove_file(&p);
            }
        } else if self.dead_obsolete.load(Ordering::Acquire)
            && let Some(p) = dead()
        {
            let _ = super::durable::remove_file(&p);
        }
    }
}

#[cfg(test)]
mod study;

#[cfg(test)]
mod tests {
    use super::*;

    /// `BlockIndex::find` is the first last key >= the key, as a search over the keys is:
    /// keys that share long prefixes, differ past the word, are prefixes of each other or
    /// of the common part, and keys outside the range.
    #[test]
    fn block_index_find() {
        let mut seed = 7u64;
        let mut rnd = |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for case in 0..200 {
            let pre: Vec<u8> = (0..rnd(6)).map(|_| rnd(3) as u8).collect();
            let mut keys: Vec<Vec<u8>> = (0..1 + rnd(if case % 3 == 0 { 40 } else { 400 }))
                .map(|_| {
                    let mut k = pre.clone();
                    k.extend((0..rnd(14)).map(|_| if case % 2 == 0 { rnd(3) as u8 } else { rnd(256) as u8 }));
                    k
                })
                .collect();
            keys.sort();
            keys.dedup();
            let mut ix = BlockIndex { keys: Vec::new(), ends: Vec::new(), at: Vec::new(), common: 0, words: Vec::new(), top: Vec::new() };
            for k in &keys {
                ix.keys.extend_from_slice(k);
                ix.ends.push(ix.keys.len() as u32);
                ix.at.push((0, 0));
            }
            ix.index_words();
            for _ in 0..60 {
                let mut probe = if rnd(2) == 0 { pre.clone() } else { keys[rnd(keys.len() as u64) as usize].clone() };
                probe.truncate(rnd(probe.len() as u64 + 1) as usize);
                probe.extend((0..rnd(12)).map(|_| rnd(4) as u8));
                let want = keys.partition_point(|k| k.as_slice() < probe.as_slice());
                assert_eq!(ix.find(&probe), want, "{keys:?} {probe:?}");
            }
            for (i, k) in keys.iter().enumerate() {
                assert_eq!(ix.find(k), i);
            }
        }
    }

    /// A full cache's bulk read goes through a ring (`BlockCache::insert_bulk`): the block it
    /// just read is in the cache for another reader, a block another reader read there stays,
    /// and the scan leaves no more of its blocks than the ring holds and pushes out nothing that
    /// was read twice.
    #[test]
    fn bulk_reads_go_through_a_ring() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::new(16 << 20);
        let file = |id: u64, table: u8, n: u32| {
            let mut w = SstWriter::create(dir.path(), id, true).unwrap();
            for i in 0..n {
                let mut k = vec![0, 0, 0x40, table, 0, 0, 0, 0];
                k.extend_from_slice(&i.to_be_bytes());
                k.extend_from_slice(&(!5u64).to_be_bytes());
                w.add(&k, Some(&[i as u8; 1000][..])).unwrap();
            }
            SstReader::open(dir.path(), w.finish().unwrap(), cache.clone()).unwrap()
        };
        // (About 25 MB each in the cache, which holds 16.)
        let (hot, fill, big) = (file(1, 1, 1000), file(2, 2, 24_000), file(3, 3, 24_000));
        let cached = |r: &SstReader| (0..r.block_count()).filter(|&i| r.cache.contains(r.cache_id, i as u32)).collect::<Vec<_>>();
        crate::storage::bulk_read(|| (0..fill.block_count()).for_each(|i| drop(fill.block(i).unwrap())));
        assert!(cached(&fill).len() > fill.block_count() / 2);
        for _ in 0..2 {
            (0..hot.block_count()).for_each(|i| drop(hot.block(i).unwrap()));
        }
        let read = big.block_count() / 2;
        crate::storage::bulk_read(|| {
            for i in 0..big.block_count() {
                drop(big.block(i).unwrap());
                if i == read {
                    assert!(big.cache.contains(big.cache_id, i as u32));
                    drop(big.cache.get(big.cache_id, i as u32).unwrap());
                }
            }
        });
        assert_eq!(cached(&hot).len(), hot.block_count());
        // (The rings, about 30 blocks here, and what room evictions left: not the 16 MB the scan
        // would fill.)
        let left = cached(&big);
        assert!(left.contains(&read) && left.len() < big.block_count() / 20, "{} of {}", left.len(), big.block_count());
    }

    /// A file of index entries alone has no bloom filter (every key passes); one with rows
    /// has one that passes every key it holds.
    #[test]
    fn index_entry_files_have_no_bloom_filter() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::new(1 << 20);
        let key = |index: u8, i: u32| {
            let mut k = vec![0, 0, 0x40, 0x41, 0, 0, 0, index];
            k.extend_from_slice(&i.to_be_bytes());
            k.extend_from_slice(&(!5u64).to_be_bytes());
            k
        };
        for (id, index) in [(1, 3u8), (2, 0)] {
            let mut w = SstWriter::create(dir.path(), id, true).unwrap();
            for i in 0..5000 {
                w.add(&key(index, i), Some(&b"v"[..])).unwrap();
            }
            let r = SstReader::open(dir.path(), w.finish().unwrap(), cache.clone()).unwrap();
            assert_eq!(r.bloom.is_empty(), index != 0);
            assert!((0..5000).all(|i| r.may_contain(user_key(&key(index, i)))));
            let passed = (5000..15000).filter(|&i| r.may_contain(user_key(&key(index, i)))).count();
            assert!(if index != 0 { passed == 10000 } else { passed < 500 }, "{passed}");
        }
    }

    /// `common_prefix` is the bytes two keys start with alike, whatever their lengths and
    /// wherever they differ; format 2's bloom hash spreads keys alike but in one byte (each
    /// byte of a word, past a word, in a tail) over its bits.
    #[test]
    fn common_prefix_and_hash64_2() {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..20_000 {
            let n = (next() % 40) as usize;
            let a: Vec<u8> = (0..n).map(|_| (next() % 3) as u8).collect();
            let mut b = a.clone();
            b.truncate((next() % (n as u64 + 1)) as usize);
            if next() % 2 == 0 && !b.is_empty() {
                let i = (next() % b.len() as u64) as usize;
                b[i] ^= 1 + (next() % 255) as u8;
            }
            b.extend((0..next() % 5).map(|_| (next() % 3) as u8));
            let want = a.iter().zip(&b).take_while(|(p, q)| p == q).count();
            assert_eq!(common_prefix(&a, &b), want, "{a:?} {b:?}");
            // The merges' comparisons without calls: as the slices', bytes past a key's end
            // in its buffer anything.
            assert_eq!(short_eq(&a, &b), a == b, "{a:?} {b:?}");
            assert_eq!(short_cmp(&a, &b), a.cmp(&b), "{a:?} {b:?}");
            let (mut ka, mut kb) = ([0u8; ZK], [0u8; ZK]);
            ka.iter_mut().chain(kb.iter_mut()).for_each(|x| *x = next() as u8);
            ka[..a.len()].copy_from_slice(&a);
            kb[..b.len()].copy_from_slice(&b);
            let m = a.len().min(b.len());
            assert_eq!(cmp_words(&ka, &kb, m), a[..m].cmp(&b[..m]), "{a:?} {b:?}");
            assert_eq!(ZipBound::new(&b).past(&ka, a.len()), a >= b, "{a:?} {b:?}");
        }
        let mut seen = std::collections::HashSet::new();
        let mut ones = [0u32; 64];
        for len in [3usize, 8, 13, 17, 25] {
            for i in 0..len {
                for v in 0..=255u8 {
                    let mut k = vec![7u8; len];
                    k[i] = v;
                    let h = hash64_2(&k);
                    seen.insert(h);
                    (0..64).for_each(|bit| ones[bit] += (h >> bit & 1) as u32);
                }
            }
        }
        let n = seen.len() as u32;
        assert!(n > 16_000, "{n}");
        // (The value files' filters were written with: the last bytes zero-padded in a buffer.)
        let padded = |b: &[u8]| {
            const K: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut h = (b.len() as u64).wrapping_mul(K);
            for w in b.chunks(8) {
                let mut t = [0u8; 8];
                t[..w.len()].copy_from_slice(w);
                h = (h ^ u64::from_le_bytes(t)).wrapping_mul(K).rotate_left(31);
            }
            h ^= h >> 33;
            h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
            h ^= h >> 33;
            h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
            h ^ (h >> 33)
        };
        for len in 0..40 {
            for _ in 0..50 {
                let k: Vec<u8> = (0..len).map(|_| next() as u8).collect();
                assert_eq!(hash64_2(&k), padded(&k), "{k:?}");
            }
        }
        assert!(ones.iter().all(|&c| c > n * 2 / 5 && c < n * 3 / 5), "{ones:?}");
    }

    /// The bloom filter's bits must not change with how they are computed: files written
    /// before are read with the same bits.
    #[test]
    fn fast_mod_is_rem() {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut ds: Vec<u64> = vec![1, 2, 3, 7, 64, 72, 1000, 8 * 1250, u32::MAX as u64, u64::MAX];
        for _ in 0..200 {
            let (a, b) = (next(), next());
            ds.push(a >> (b % 60));
        }
        for d in ds.into_iter().filter(|&d| d > 0) {
            let m = FastMod::new(d);
            for a in [0, 1, d - 1, d, d.wrapping_add(1), u64::MAX, u64::MAX - 1] {
                assert_eq!(m.rem(a), a % d, "{a} % {d}");
            }
            for _ in 0..2000 {
                let a = next();
                assert_eq!(m.rem(a), a % d, "{a} % {d}");
            }
        }
    }

    /// A filter's bits are those of the formula files were written with, wraps of the 64-bit
    /// sums included.
    #[test]
    fn bloom_bits_are_the_formulas() {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut ns: Vec<u64> = vec![64, 72, 640, 8 * 1250, 8 * 125_001, 22_600_660];
        for _ in 0..50 {
            ns.push(64 + next() % (1 << 30));
        }
        for n in ns {
            let m = FastMod::new(n);
            for i in 0..3000 {
                let h = match i {
                    0 => 0,
                    1 => u64::MAX,
                    2 => u64::MAX - 5,
                    _ => next(),
                };
                let (h1, h2) = (h, (h >> 32) | 1);
                let want: Vec<usize> =
                    (0..BLOOM_K as u64).map(|i| (h1.wrapping_add(i.wrapping_mul(h2)) % n) as usize).collect();
                assert_eq!(bloom_bits(h, m).collect::<Vec<_>>(), want, "{h} in {n}");
            }
        }
    }

    /// A block's entries (keys sharing prefixes, tombstones) as the file's format has them,
    /// `restart` 0: every entry sharing with the one before (files written before restarts).
    fn block_bytes(ents: &[(Vec<u8>, Option<Vec<u8>>)], restart: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut prev: &[u8] = &[];
        for (i, (k, v)) in ents.iter().enumerate() {
            let shared = if i == 0 || restart > 0 && i % restart == 0 {
                0
            } else {
                prev.iter().zip(k).take_while(|(a, b)| a == b).count()
            };
            put_varint(&mut out, shared as u64);
            put_varint(&mut out, (k.len() - shared) as u64);
            put_varint(&mut out, v.as_ref().map_or(0, |v| v.len() as u64 + 1));
            out.extend_from_slice(&k[shared..]);
            out.extend_from_slice(v.as_deref().unwrap_or_default());
            prev = k;
        }
        out
    }

    /// Seeks, steps and positions of a block agree with its entries, whichever way it was
    /// written, and its expanded form too.
    #[test]
    fn block_positions() {
        let plain: Vec<(Vec<u8>, Option<Vec<u8>>)> = (0..300u32)
            .map(|i| {
                let k = format!("tbl/{:05}/{}", i * 3, "x".repeat((i % 7) as usize)).into_bytes();
                let v = (i % 11 != 0).then(|| format!("v{i}").into_bytes());
                (k, v)
            })
            .collect();
        // Index entries (the compact form's case): the index's prefix, a column, the row's
        // key and a version, mostly the block's; a value of the table's prefix and the row
        // key (derived), or something else; some short keys.
        let mut index: Vec<(Vec<u8>, Option<Vec<u8>>)> = (0..300u32)
            .map(|i| {
                let row = [0x20, 0x80, 0, 0, 0, 0, 0, (i / 3) as u8, (i % 3) as u8 * 7];
                let mut k = vec![0, 0, 0x40, 0x41, 0, 0, 0x40, 0x65, 0x20, (i / 50) as u8];
                k.extend_from_slice(&row);
                let ts: u64 = if i % 13 == 5 { 7 } else { 9 };
                k.extend_from_slice(&(!ts).to_be_bytes());
                let mut v = vec![0, 0, 0x40, 0x41, 0, 0, 0, 0];
                v.extend_from_slice(&row[(i % 4) as usize..]);
                match i % 17 {
                    3 => (vec![1, (i % 256) as u8], Some(b"short".to_vec())),
                    4 => (k, None),
                    5 => (k, Some(b"other".to_vec())),
                    6 => (k, Some(v[..6].to_vec())),
                    _ => (k, Some(v)),
                }
            })
            .collect();
        index.sort();
        index.dedup_by(|a, b| a.0 == b.0);
        let raw = block_bytes(&index, RESTART).len();
        let compact = Block::decode(Bytes::from(block_bytes(&index, RESTART))).unwrap();
        assert!(compact.derived && compact.data.len() * 2 < raw, "{} of {raw}", compact.data.len());
        for (ents, restart) in [(&plain, 0), (&plain, RESTART), (&plain, 5), (&index, RESTART), (&index, 3)] {
            let b = Block::decode(Bytes::from(block_bytes(ents, restart))).unwrap();
            assert_eq!(b.len(), ents.len());
            assert_eq!(b.last_key(), Some(ents.last().unwrap().0.as_slice()));
            let mut pos = BlockPos::default();
            b.at(0, &mut pos);
            for (i, (k, v)) in ents.iter().enumerate() {
                assert_eq!((pos.index(), pos.key()), (i, k.as_slice()));
                assert_eq!(b.value_ref(&pos), v.as_deref());
                let mut at = BlockPos::default();
                b.at(i, &mut at);
                assert_eq!(at.key(), k.as_slice());
                assert_eq!(b.value(&at).as_deref(), v.as_deref());
                b.step(&mut pos);
            }
            assert_eq!(pos.index(), b.len());
            let e = b.expand();
            for (i, (k, v)) in ents.iter().enumerate() {
                assert_eq!((e.key(i), e.value_ref(i)), (k.as_slice(), v.as_deref()));
                // Keys just below, at and just above each one.
                let mut below = k.clone();
                below.pop();
                let mut above = k.clone();
                above.push(0);
                // (And one below it in its last byte: inside an index entry's version.)
                let mut less = k.clone();
                if let Some(x) = less.last_mut() {
                    *x = x.wrapping_sub(1);
                }
                for probe in [below, k.clone(), above, less] {
                    let want = ents.iter().position(|(k, _)| *k >= probe).unwrap_or(ents.len());
                    b.seek(&probe, &mut pos);
                    assert_eq!(pos.index(), want, "{restart} {probe:?}");
                    assert_eq!(e.seek(&probe), want);
                    // (`seek_on` from places before, at and after it finds the same entry.)
                    for j in [0, want.saturating_sub(5), want.saturating_sub(1), want, want + 1, want + 17] {
                        if j >= ents.len() {
                            continue;
                        }
                        let mut from = BlockPos::default();
                        b.at(j, &mut from);
                        b.seek_on(&probe, &mut from);
                        assert_eq!(from.index(), want, "seek_on from {j}: {restart} {probe:?}");
                        if want < ents.len() {
                            assert_eq!(from.key(), ents[want].0.as_slice());
                            assert_eq!(b.value_ref(&from), ents[want].1.as_deref());
                        }
                    }
                    if want < ents.len() {
                        assert_eq!(pos.key(), ents[want].0.as_slice());
                        assert_eq!(b.value_ref(&pos), ents[want].1.as_deref());
                        // (A cursor goes on from a seek's place.)
                        let mut next = pos.clone();
                        b.step(&mut next);
                        if want + 1 < ents.len() {
                            assert_eq!(next.key(), ents[want + 1].0.as_slice());
                        }
                    }
                }
            }
            b.seek(b"", &mut pos);
            assert_eq!(pos.index(), 0);
            b.seek(&[0xFF], &mut pos);
            assert_eq!(pos.index(), b.len());
        }
    }

    /// Every entry of a block, by a cursor from its start.
    fn entries_of(b: &Block) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let mut out = Vec::new();
        let mut pos = BlockPos::default();
        b.at(0, &mut pos);
        while pos.index() < b.len() {
            out.push((pos.key().to_vec(), b.value_ref(&pos).map(<[u8]>::to_vec)));
            b.step(&mut pos);
        }
        out
    }

    /// Sorted entries of every kind a file holds: a table's rows, an index's entries (derived
    /// values, other values, tombstones), the store's own keys (some too short for a version),
    /// versions not the block's.
    fn mixed_entries() -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let mut seed = 11u64;
        let mut rnd = |n: u64| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let mut ents: Vec<(Vec<u8>, Option<Vec<u8>>)> = Vec::new();
        for i in 0..20_000u32 {
            let ts: u64 = if rnd(9) == 0 { 3 + rnd(4) } else { 40 };
            let (k, v) = match i % 3 {
                // A table's rows.
                0 => {
                    let mut k = vec![0, 0, 0x40, 0x41, 0, 0, 0, 0, 0x20];
                    k.extend_from_slice(&(i as u64).to_be_bytes());
                    let v: Vec<u8> = (0..rnd(60)).map(|_| rnd(7) as u8).collect();
                    (k, Some(v))
                }
                // An index's entries, the row's key as the value (derived) or not.
                1 => {
                    let row = (i as u64 / 3).to_be_bytes();
                    let mut k = vec![0, 0, 0x40, 0x41, 0, 0, 0x40, 0x65, 0x20, (i / 500) as u8, 0x20];
                    k.extend_from_slice(&row);
                    let mut v = vec![0, 0, 0x40, 0x41, 0, 0, 0, 0, 0x20];
                    v.extend_from_slice(&row);
                    (k, if rnd(11) == 0 { None } else if rnd(13) == 0 { Some(b"other".to_vec()) } else { Some(v) })
                }
                // The store's own keys, some short.
                _ => ((0..1 + rnd(12)).map(|_| rnd(3) as u8).collect(), Some(vec![i as u8; rnd(40) as usize])),
            };
            let mut k = k;
            if i % 3 != 2 || k.len() > 6 {
                k.extend_from_slice(&(!ts).to_be_bytes());
            }
            ents.push((k, v));
        }
        ents.sort_by(|a, b| a.0.cmp(&b.0));
        ents.dedup_by(|a, b| a.0 == b.0);
        ents
    }

    /// The same entries written in format 1 and format 2 (each codec) read the same, block by
    /// block: keys, values (derived index values, tombstones, versions not the block's, keys
    /// too short for one), positions, seeks, and the blocks' cache form byte for byte.
    #[test]
    fn format2_reads_as_format1() {
        let ents = mixed_entries();
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::new(64 << 20);
        let write = |id: u64, codec: Codec, v2: bool| {
            let mut w = SstWriter::create_with(dir.path(), id, codec, v2).unwrap();
            // (Format 1's block ends: `ROW_BLOCK` and `INDEX_BLOCK` end format 2's sooner.)
            w.set_block_target(BLOCK_TARGET);
            for (k, v) in &ents {
                w.add(k, v.as_deref()).unwrap();
            }
            SstReader::open(dir.path(), w.finish().unwrap(), cache.clone()).unwrap()
        };
        let one = write(1, Codec::Zstd, false);
        assert_eq!(one.format(), 1);
        for (id, codec) in [(2, Codec::None), (3, Codec::Zstd), (4, Codec::Lz4)] {
            let two = write(id, codec, true);
            assert_eq!(two.format(), if codec == Codec::Zstd { FORMAT_DICT } else { FORMAT });
            assert_eq!(two.first_block_compressed().unwrap(), codec != Codec::None);
            let mut all = Vec::new();
            for i in 0..two.block_count() {
                all.extend(entries_of(&two.block_with(i, false).unwrap()));
            }
            assert_eq!(all, ents, "{codec:?}");
            // (The same blocks: format 2's end where format 1's do, so the cache holds what it
            // held, block for block.)
            assert_eq!(two.block_count(), one.block_count());
            for i in 0..two.block_count() {
                let (a, b) = (one.block_with(i, false).unwrap(), two.block_with(i, false).unwrap());
                assert_eq!((&a.data[..], a.restarts_vec(), a.rvals_vec(), a.mem_bytes()), (&b.data[..], b.restarts_vec(), b.rvals_vec(), b.mem_bytes()), "block {i}");
            }
            // Each block of format 2 is the cache form `Block::decode` makes of the same
            // entries in format 1's block form.
            let mut at = 0;
            for i in 0..two.block_count() {
                let b = two.block_with(i, false).unwrap();
                let mine = &ents[at..at + b.len()];
                at += b.len();
                let d = Block::decode(Bytes::from(block_bytes(mine, RESTART))).unwrap();
                assert_eq!(&b.data[..], &d.data[..], "block {i}");
                assert_eq!((b.restarts_vec(), b.rvals_vec(), b.common, b.len, b.last_key(), b.ts, b.derived), (d.restarts_vec(), d.rvals_vec(), d.common, d.len, d.last_key(), d.ts, d.derived));
                let mut pos = BlockPos::default();
                for (j, (k, v)) in mine.iter().enumerate() {
                    b.seek(k, &mut pos);
                    assert_eq!((pos.index(), b.value_ref(&pos)), (j, v.as_deref()));
                }
            }
            // (A block of another restart interval reads the same: it says which it has.)
            let mut w = SstWriter::create_with(dir.path(), 10 + id, codec, true).unwrap();
            w.set_block_target(BLOCK_TARGET);
            w.restart_log(2);
            for (k, v) in &ents {
                w.add(k, v.as_deref()).unwrap();
            }
            let four = SstReader::open(dir.path(), w.finish().unwrap(), cache.clone()).unwrap();
            let mut all = Vec::new();
            for i in 0..four.block_count() {
                let b = four.block_with(i, false).unwrap();
                assert_eq!(b.nres(), b.len().div_ceil(4));
                let mine = entries_of(&b);
                let mut pos = BlockPos::default();
                for (j, (k, v)) in mine.iter().enumerate() {
                    b.seek(k, &mut pos);
                    assert_eq!((pos.index(), b.value_ref(&pos)), (j, v.as_deref()));
                }
                all.extend(mine);
            }
            assert_eq!(all, ents, "{codec:?}, restarts every 4");
            // (Both files' block indexes find each key's block.)
            for (k, v) in ents.iter().step_by(7) {
                for f in [&one, &two] {
                    let b = f.block_with(f.find_block(k), false).unwrap();
                    let mut pos = BlockPos::default();
                    b.seek(k, &mut pos);
                    assert_eq!((pos.key(), b.value_ref(&pos)), (k.as_slice(), v.as_deref()));
                }
            }
        }
    }

    /// A file whose blocks are compressed with a dictionary it holds (format 4) reads as one
    /// without it (format 2), block for block, whether the blocks before the dictionary was
    /// made are few or all; a file with too few blocks to train one has none (format 2); a
    /// damaged dictionary is refused at open (XX001).
    #[test]
    fn dictionary_blocks_read_as_plain_ones() {
        let ents = mixed_entries();
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::new(64 << 20);
        let write = |id: u64, dict: Option<(usize, usize)>, n: usize| {
            let mut w = SstWriter::create_with(dir.path(), id, Codec::Zstd, true).unwrap();
            match dict {
                Some((size, sample)) => w.set_dict(size, sample),
                None => w.no_dict(),
            }
            for (k, v) in &ents[..n] {
                w.add(k, v.as_deref()).unwrap();
            }
            SstReader::open(dir.path(), w.finish().unwrap(), cache.clone()).unwrap()
        };
        let plain = write(1, None, ents.len());
        for (id, sample) in [(2, 256 << 10), (3, 1 << 30)] {
            let d = write(id, Some((8 << 10, sample)), ents.len());
            assert!(d.meta.dict);
            assert_eq!(d.format(), FORMAT_DICT);
            assert_eq!(d.block_count(), plain.block_count());
            for i in 0..d.block_count() {
                let (a, b) = (plain.block_with(i, false).unwrap(), d.block_with(i, false).unwrap());
                assert_eq!((&a.data[..], a.restarts_vec(), a.rvals_vec(), a.last_key()), (&b.data[..], b.restarts_vec(), b.rvals_vec(), b.last_key()), "block {i}");
            }
            for (k, v) in ents.iter().step_by(7) {
                let b = d.block_with(d.find_block(k), false).unwrap();
                let mut pos = BlockPos::default();
                b.seek(k, &mut pos);
                assert_eq!((pos.key(), b.value_ref(&pos)), (k.as_slice(), v.as_deref()));
            }
        }
        let small = write(4, Some((8 << 10, 1 << 20)), 200);
        assert!(!small.meta.dict);
        assert_eq!(small.format(), FORMAT);
        // A flipped byte of the dictionary (the bytes before the index).
        let d = write(5, Some((8 << 10, 256 << 10)), ents.len());
        let meta = d.meta.clone();
        let at = d.index.at.last().map(|(o, l)| o + *l as u64).unwrap() + 100;
        drop(d);
        let f = std::fs::OpenOptions::new().read(true).write(true).open(sst_path(dir.path(), 5)).unwrap();
        let mut b = [0u8; 1];
        f.read_exact_at(&mut b, at).unwrap();
        f.write_all_at(&[b[0] ^ 0x10], at).unwrap();
        let e = SstReader::open(dir.path(), meta, cache.clone()).err().expect("refused");
        assert!(e.message.contains("checksum mismatch") && e.message.contains("dictionary"), "{}", e.message);
    }

    /// A file read with a dead-entry set reads as one written without those entries: blocks
    /// with every entry dead, none, some; a seek of each key finds the first live entry at or
    /// past it; the set read back from its file (a damaged one refused) and deleted with the
    /// reader marked so, the file kept.
    #[test]
    fn dead_entries_read_as_a_rewrite_without_them() {
        // (Format 1 too: its blocks' entries keep their places as `Block::decode` reads them.)
        for (id, v2) in [(1, true), (2, false)] {
            dead_entries_of(id, v2);
        }
    }

    fn dead_entries_of(id: u64, v2: bool) {
        let ents = mixed_entries();
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::new(64 << 20);
        let mut w = SstWriter::create_with(dir.path(), id, Codec::Zstd, v2).unwrap();
        for (k, v) in &ents {
            w.add(k, v.as_deref()).unwrap();
        }
        let f = SstReader::open(dir.path(), w.finish().unwrap(), cache.clone()).unwrap();
        assert_eq!(f.format() >= FORMAT, v2);
        let (mut dead, mut live, mut seed) = (DeadSet::default(), Vec::new(), 5u64);
        for b in 0..f.block_count() {
            for (i, e) in entries_of(&f.raw_block(b).unwrap()).into_iter().enumerate() {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                if match b % 4 { 0 => false, 1 => true, _ => (seed >> 33) % 3 == 0 } {
                    dead.add(b, i);
                } else {
                    live.push(e);
                }
            }
        }
        let g = f.with_dead(dir.path(), &dead, 1).unwrap();
        assert_eq!((g.live_entries() as usize, g.meta.dead), (live.len(), dead.count()));
        let read = |r: &SstReader| (0..r.block_count()).flat_map(|b| entries_of(&r.block(b).unwrap())).collect::<Vec<_>>();
        assert_eq!(read(&g), live);
        for (k, _) in ents.iter().step_by(3) {
            let mut got = None;
            for b in g.find_block(k)..g.block_count() {
                let (blk, mut pos) = (g.block(b).unwrap(), BlockPos::default());
                blk.seek(k, &mut pos);
                if pos.index() < blk.len() {
                    got = Some((pos.key().to_vec(), blk.value_ref(&pos).map(<[u8]>::to_vec)));
                    break;
                }
            }
            assert_eq!(got.as_ref(), live.iter().find(|(l, _)| l >= k));
        }
        assert_eq!(read(&f), ents);
        assert_eq!(DeadSet::decode(&dead.encode()).unwrap(), dead);
        let mut bad = dead.encode();
        bad[13] ^= 1;
        assert!(DeadSet::decode(&bad).is_err());
        let h = SstReader::open(dir.path(), g.meta.clone(), cache.clone()).unwrap();
        assert_eq!(read(&h), live);
        drop(h);
        let p = dead_path(dir.path(), id, 1);
        assert!(p.exists());
        g.mark_dead_obsolete();
        drop(g);
        assert!(!p.exists() && f.path().exists());
    }

    /// A linkable run of format 2 (`create_run`: plain values, every key one version) read
    /// by its transaction gets each value's tag back (`retagged`), and once linked reads as
    /// versions at the commit's ts, as format 1's tagged runs do.
    #[test]
    fn format2_runs_read_retagged_and_relinked() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::new(8 << 20);
        let mut w = SstWriter::create_run(dir.path(), 7, true).unwrap();
        // (A deletion every 7th: a statement's deletions are a run of them alone.)
        let rows: Vec<(Vec<u8>, Option<Vec<u8>>)> =
            (0..3000u32).map(|i| (format!("k{i:05}").into_bytes(), (i % 7 != 3).then(|| format!("value {i}").into_bytes()))).collect();
        for (k, v) in &rows {
            w.add(&mvcc(k, 9), v.as_deref()).unwrap();
        }
        let meta = w.finish().unwrap();
        assert_eq!(meta.deletes, rows.iter().filter(|r| r.1.is_none()).count() as u64);
        let read = |r: &SstReader| (0..r.block_count()).flat_map(|b| entries_of(&r.block(b).unwrap())).collect::<Vec<_>>();
        let own = SstReader::open(dir.path(), meta.clone(), cache.clone()).unwrap().retagged(5);
        assert_eq!(own.format(), FORMAT);
        let want: Vec<_> = rows
            .iter()
            .map(|(k, v)| (mvcc(k, 9), Some(match v { Some(v) => [&[3, 0, 0, 0, 5][..], v].concat(), None => vec![2, 0, 0, 0, 5] })))
            .collect();
        assert_eq!(read(&own), want);
        let mut linked = meta;
        linked.commit_ts = Some(41);
        set_ts(&mut linked.first_key, 41);
        set_ts(&mut linked.last_key, 41);
        let l = SstReader::open(dir.path(), linked, cache.clone()).unwrap();
        let want: Vec<_> = rows.iter().map(|(k, v)| (mvcc(k, 41), v.clone())).collect();
        assert_eq!(read(&l), want);
        for (k, v) in rows.iter().step_by(97) {
            let (b, mut pos) = (l.block(l.find_block(&mvcc(k, 41))).unwrap(), BlockPos::default());
            b.seek(&mvcc(k, 41), &mut pos);
            assert_eq!((pos.key(), b.value_ref(&pos)), (&mvcc(k, 41)[..], v.as_deref()));
        }
    }

    /// A file of a format past this build's, and a footer of neither format, are refused
    /// with an error naming them.
    #[test]
    fn newer_formats_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::new(1 << 20);
        let mut w = SstWriter::create(dir.path(), 1, true).unwrap();
        for i in 0..100u32 {
            w.add(&mvcc(&i.to_be_bytes(), 5), Some(b"v")).unwrap();
        }
        let meta = w.finish().unwrap();
        let path = sst_path(dir.path(), 1);
        let mut b = std::fs::read(&path).unwrap();
        let n = b.len();
        b[n - 16..n - 8].copy_from_slice(&(FORMAT_COLS as u64 + 1).to_le_bytes());
        std::fs::write(&path, &b).unwrap();
        let e = SstReader::open(dir.path(), meta.clone(), cache.clone()).err().unwrap();
        assert!(e.message.contains(&format!("storage format {}, newer than this build reads ({FORMAT_COLS})", FORMAT_COLS + 1)), "{}", e.message);
        // (Format 3 is a store's, never a file's.)
        b[n - 16..n - 8].copy_from_slice(&3u64.to_le_bytes());
        std::fs::write(&path, &b).unwrap();
        let e = SstReader::open(dir.path(), meta.clone(), cache.clone()).err().unwrap();
        assert!(e.message.contains("bad footer"), "{}", e.message);
        b[n - 1] ^= 1;
        std::fs::write(&path, &b).unwrap();
        let e = SstReader::open(dir.path(), meta, cache).err().unwrap();
        assert!(e.message.contains("bad footer"), "{}", e.message);
    }

    /// A format 2 block whose bytes changed is refused by its checksum (XX001, as format 1's),
    /// raw or compressed; one whose checksum holds but whose trailer doesn't add up is refused
    /// by `Block::load`, not read.
    #[test]
    fn corrupted_format2_blocks_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        for (id, codec) in [(1, Codec::None), (2, Codec::Zstd)] {
            let mut w = SstWriter::create_with(dir.path(), id, codec, true).unwrap();
            for i in 0..2000u32 {
                w.add(&mvcc(&i.to_be_bytes(), 5), Some(format!("value {i}").as_bytes())).unwrap();
            }
            let meta = w.finish().unwrap();
            let path = sst_path(dir.path(), id);
            let good = std::fs::read(&path).unwrap();
            let open = |b: &[u8]| {
                std::fs::write(&path, b).unwrap();
                SstReader::open(dir.path(), meta.clone(), BlockCache::new(1 << 20)).unwrap()
            };
            let (off, len) = open(&good).index.at[1];
            let (off, len) = (off as usize, len as usize);
            let mut b = good.clone();
            b[off + len / 2] ^= 0x10;
            let e = open(&b).block(1).err().unwrap();
            assert_eq!((e.code, e.message.contains("checksum mismatch")), (crate::error::code::DATA_CORRUPTED, true), "{codec:?}: {}", e.message);
            if codec == Codec::None {
                // (The flags byte ends the body: a restart interval of 0, the checksum redone.)
                let mut b = good.clone();
                b[off + len - 5] &= 0x0F;
                let crc = crc32fast::hash(&b[off..off + len - 4]);
                b[off + len - 4..off + len].copy_from_slice(&crc.to_le_bytes());
                let e = open(&b).block(1).err().unwrap();
                assert!(e.message.contains("bad sst block"), "{}", e.message);
            }
            assert!(open(&good).block(1).is_ok());
        }
    }

    fn mvcc(k: &[u8], ts: u64) -> Vec<u8> {
        let mut v = k.to_vec();
        v.extend_from_slice(&(!ts).to_be_bytes());
        v
    }

    /// A linked run's block decoded in one pass reads as the relinked block does.
    #[test]
    fn linked_blocks_decode_as_relinked() {
        let ents: Vec<(Vec<u8>, Option<Vec<u8>>)> = (0..200u32)
            .map(|i| {
                let mut k = vec![0, 0, 0x40, 0x41, 0, 0, 0, 0, 0x20, (i / 40) as u8, (i % 40) as u8];
                k.extend_from_slice(&(!5u64).to_be_bytes());
                let v = match i % 5 {
                    0 => Some(vec![1, b'a', (i % 256) as u8]),
                    1 => Some([&[3u8, 0, 0, 0, 9][..], b"row", &[i as u8]].concat()),
                    2 => Some(vec![0]),
                    3 => None,
                    _ => Some(vec![3, 0, 0, 0, 1]),
                };
                (k, v)
            })
            .collect();
        for restart in [RESTART, 3] {
            let raw = block_bytes(&ents, restart);
            let one = Block::decode_linked(&raw, 77).unwrap();
            let two = Block::decode(relink(&raw, 77).unwrap()).unwrap();
            assert_eq!(one.len(), two.len());
            let (mut a, mut b) = (BlockPos::default(), BlockPos::default());
            one.at(0, &mut a);
            two.at(0, &mut b);
            while a.index() < one.len() {
                assert_eq!(a.key(), b.key());
                assert_eq!(key_ts(a.key()), 77);
                assert_eq!(one.value_ref(&a), two.value_ref(&b));
                one.step(&mut a);
                two.step(&mut b);
            }
        }
    }
}
