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

use super::cache::{BlockCache, CachedBlock};
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
pub(crate) const BLOCK_TARGET: usize = 16 * 1024;
const FOOTER_LEN: usize = 48;
const FOOTER2_LEN: usize = 56;
/// The newest file format this build reads, and what it writes the store's files in (a
/// transaction's runs are format 1: `SstWriter::create_run`).
pub const FORMAT: u32 = 2;
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
        let mut t = [0u8; 8];
        t[..r.len()].copy_from_slice(r);
        h = (h ^ u64::from_le_bytes(t)).wrapping_mul(K).rotate_left(31);
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
}

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
}

impl Block2 {
    fn clear(&mut self) {
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
    /// as it is when its commit links it, so its entries are written once. (Format 1: its
    /// values carry the run's tags, which `Block::decode_linked` takes off as it reads it.)
    pub fn create_run(dir: &Path, id: u64, compress: bool) -> Result<Self> {
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
        })
    }

    pub fn entries(&self) -> u64 {
        self.entries
    }

    pub fn approx_size(&self) -> u64 {
        self.offset + (self.block.len() + self.b2.vals.len()) as u64
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
        let ts = key_ts(key);
        self.min_ts = self.min_ts.min(ts);
        self.max_ts = self.max_ts.max(ts);
        let h = if self.v2 { hash64_2(user_key(key)) } else { hash64(user_key(key)) };
        self.rows |= !index_entry(user_key(key));
        if self.last_user_hash != Some(h) {
            match &mut self.bloom {
                Some((b, m)) => bloom_set(b, *m, h),
                None => self.hashes.push(h),
            }
            self.last_user_hash = Some(h);
        }
        if self.block_bytes1() >= BLOCK_TARGET {
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
        }
    }

    /// Format 2's block body: the entries, their values, and what `Block::load` reads from
    /// its end (the restarts as `Block::restarts` holds them).
    fn body2(&mut self) -> Vec<u8> {
        let b = &mut self.b2;
        let mut raw = std::mem::take(&mut self.block);
        let vbase = raw.len() as u32;
        raw.extend_from_slice(&b.vals);
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
        raw.push(b.derived as u8 | self.rlog << 4);
        b.clear();
        raw
    }

    fn finish_block(&mut self) -> Result<()> {
        if self.block_entries == 0 {
            return Ok(());
        }
        let raw = if self.v2 { self.body2() } else { std::mem::take(&mut self.block) };
        let mut data = Vec::with_capacity(raw.len() + 9);
        let zstd_err = |e: std::io::Error| internal(format!("zstd: {e}"));
        let packed = match self.codec {
            Codec::None => None,
            Codec::Zstd => {
                if self.zc.is_none() {
                    self.zc = Some(zstd::bulk::Compressor::new(if self.v2 { ZSTD_LEVEL } else { 1 }).map_err(zstd_err)?);
                }
                let zc = self.zc.as_mut().expect("a zstd context");
                Some((1, zc.compress(&raw).map_err(zstd_err)?))
            }
            Codec::Lz4 => Some((2, lz4_flex::block::compress(&raw))),
        };
        match packed {
            Some((tag, c)) if c.len() < raw.len() * 7 / 8 => {
                data.push(tag);
                data.extend_from_slice(&(raw.len() as u32).to_le_bytes());
                data.extend_from_slice(&c);
            }
            _ => {
                data.push(0);
                data.extend_from_slice(&raw);
            }
        }
        let crc = crc32fast::hash(&data);
        data.extend_from_slice(&crc.to_le_bytes());
        self.out.write_all(&data)?;
        self.index
            .push((self.prev_key.clone(), self.offset, data.len() as u32, self.block_entries as u32));
        self.offset += data.len() as u64;
        self.block = raw;
        self.block.clear();
        self.block_entries = 0;
        self.raw1 = 0;
        Ok(())
    }

    pub fn finish(mut self) -> Result<SstMeta> {
        self.finish_block()?;
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
            _ if !self.rows => Vec::new(),
            Some((b, _)) => b,
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

        let mut footer = Vec::with_capacity(FOOTER2_LEN);
        let tail: &[u64] = if self.v2 { &[FORMAT as u64, MAGIC2] } else { &[MAGIC] };
        for v in [index_off, index.len() as u64, bloom_off, bloom.len() as u64, self.entries].iter().chain(tail) {
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
        })
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
}

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
    data: Bytes,
    /// Every `RESTART`th entry: its key as a number in the high 32 bits (as
    /// `BlockIndex::words`, of 4 bytes: those after the `common` bytes all restarts' keys
    /// start with), its offset in the low ones. A seek searches the numbers, not the
    /// entries: a point read's block is seldom in the CPU's cache, and every restart it
    /// compared was a miss (three quarters of `seek`).
    restarts: Vec<u64>,
    /// Where each restart's entry's value is (each value follows the one before's).
    rvals: Vec<u32>,
    common: usize,
    len: usize,
    last_key: Box<[u8]>,
    /// The version (a key's last 8 bytes) entries flagged `F_TS` end with.
    ts: [u8; 8],
    /// Whether some entry's value is derived (`expand` then copies the values).
    derived: bool,
    /// Entries between restarts: 1 << `rlog` (`RLOG` but in a format 2 block that says
    /// otherwise).
    rlog: u8,
}

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
#[inline(always)]
fn head(b: &[u8], pos: &mut usize) -> (usize, usize, usize) {
    #[inline(always)]
    fn v(b: &[u8], pos: &mut usize) -> usize {
        match b[*pos] {
            x if x < 0x80 => {
                *pos += 1;
                x as usize
            }
            _ => get_varint_long(b, pos).expect("a checked block") as usize,
        }
    }
    let shared = v(b, pos);
    let unshared = v(b, pos);
    (shared, unshared, v(b, pos))
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
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn last_key(&self) -> Option<&[u8]> {
        (self.len > 0).then_some(&*self.last_key)
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
        pos.next = self.restarts[r] as u32;
        pos.vnext = self.rvals[r];
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

    /// The whole key of restart `r` compared with `key`.
    fn restart_cmp(&self, r: usize, key: &[u8]) -> std::cmp::Ordering {
        let mut p = self.restarts[r] as u32 as usize;
        let (_, us, _) = head(&self.data, &mut p);
        let stored = &self.data[p..p + (us >> 2)];
        cmp_split(stored, if us & F_TS != 0 { &self.ts } else { &[] }, key)
    }

    /// The first restart whose key is >= `key` (`restarts.len()`: none).
    fn first_restart(&self, key: &[u8]) -> usize {
        let n = self.restarts.len();
        if n == 0 {
            return 0;
        }
        let c = self.common;
        let m = c.min(key.len());
        // (Restart 0 stores its key's first `common` bytes whole.)
        let mut p = self.restarts[0] as u32 as usize;
        head(&self.data, &mut p);
        match key[..m].cmp(&self.data[p..p + m]) {
            std::cmp::Ordering::Less => return 0,
            std::cmp::Ordering::Greater => return n,
            std::cmp::Ordering::Equal if key.len() < c => return 0,
            std::cmp::Ordering::Equal => {}
        }
        let w = (word(&key[c..]) >> 32) as u32;
        let word_of = |r: u64| (r >> 32) as u32;
        let mut lo = self.restarts.partition_point(|&x| word_of(x) < w);
        while lo < n && word_of(self.restarts[lo]) == w && self.restart_cmp(lo, key).is_lt() {
            lo += 1;
        }
        lo
    }

    /// `pos` at the first entry whose key is >= `key` (`len`: none).
    pub fn seek(&self, key: &[u8], pos: &mut BlockPos) {
        // The first restart whose key is >= `key`; the entries from the one before it.
        let lo = self.first_restart(key);
        let r = lo.saturating_sub(1);
        if r >= self.restarts.len() {
            pos.i = self.len;
            return;
        }
        // The keys from the restart's on, the entries' values left alone until the one
        // found.
        pos.next = self.restarts[r] as u32;
        pos.vnext = self.rvals[r];
        pos.key.clear();
        pos.i = r << self.rlog;
        self.read(pos);
        if pos.key.as_slice() >= key {
            return;
        }
        let b = &self.data[..];
        let end = ((r + 1) << self.rlog).min(self.len);
        let (mut p, mut v) = (pos.next as usize, pos.vnext);
        for i in pos.i + 1..end {
            let at = (p, v);
            let (shared, us, vf) = head(b, &mut p);
            let unshared = us >> 2;
            pos.key.truncate(shared);
            pos.key.extend_from_slice(&b[p..p + unshared]);
            if us & F_TS != 0 {
                pos.key.extend_from_slice(&self.ts);
            }
            // (`read` makes this key again from the one it has: their first `shared` bytes
            // are the same. A whole comparison: a byte loop from the shared prefix on took more
            // instructions than memcmp saved.)
            if pos.key.as_slice() >= key {
                (pos.next, pos.vnext) = (at.0 as u32, at.1);
                pos.i = i;
                self.read(pos);
                return;
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
            n => Some(self.data.slice(pos.val_off as usize..(pos.val_off + n) as usize)),
        }
    }

    /// The value at `pos`, borrowed.
    pub fn value_ref<'a>(&'a self, pos: &'a BlockPos) -> Option<&'a [u8]> {
        match pos.val_len {
            TOMBSTONE => None,
            DERIVED => Some(&pos.dval),
            n => Some(&self.data[pos.val_off as usize..(pos.val_off + n) as usize]),
        }
    }

    /// The entry at `pos` (not a tombstone) into `batch` under `key`: its value referenced in
    /// the block's bytes, or copied when derived.
    pub fn push_value(&self, pos: &BlockPos, key: &[u8], batch: &mut super::batch::RowBatch) {
        match pos.val_len {
            TOMBSTONE => debug_assert!(false, "a tombstone pushed"),
            DERIVED => batch.push_copy(key, &pos.dval),
            n => batch.push_in(key, &self.data, pos.val_off as usize, n as usize),
        }
    }

    /// The bytes it holds (what the block cache counts).
    pub fn mem_bytes(&self) -> usize {
        self.data.len() + self.restarts.capacity() * 8 + self.rvals.capacity() * 4 + self.last_key.len() + std::mem::size_of::<Block>()
    }

    /// Every entry with its key whole, for readers that go backwards or read a block once.
    pub fn expand(&self) -> Expanded {
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

    /// A format 2 block as it was decompressed (`SstWriter::body2`): its entries and values
    /// are the compact form already, the restarts read from its end, the last key from the
    /// file's block index.
    fn load(raw: Bytes, last_key: &[u8]) -> Result<Block> {
        let bad = || internal("bad sst block");
        let n = raw.len();
        let t = raw.get(n.checked_sub(TRAILER).ok_or_else(bad)?..).ok_or_else(bad)?;
        let u32_at = |i: usize| u32::from_le_bytes(t[i..i + 4].try_into().unwrap()) as usize;
        let ts: [u8; 8] = t[..8].try_into().unwrap();
        let (len, nr) = (u32_at(8), u32_at(12));
        let common = u16::from_le_bytes([t[16], t[17]]) as usize;
        // (Flags: bit 0 a value is derived; bits 4-7 the restart interval's log.)
        let derived = t[18] & 1 != 0;
        let rlog = t[18] >> 4;
        let data_len = n.checked_sub(TRAILER + nr * 12).ok_or_else(bad)?;
        if !(1..=8).contains(&rlog) || nr != len.div_ceil(1 << rlog) {
            return Err(bad());
        }
        let r = &raw[data_len..];
        let restarts: Vec<u64> = r[..nr * 8].as_chunks::<8>().0.iter().map(|c| u64::from_le_bytes(*c)).collect();
        let rvals: Vec<u32> = r[nr * 8..nr * 12].as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect();
        if restarts.iter().any(|&x| x as u32 as usize >= data_len) || rvals.iter().any(|&v| v as usize > data_len) {
            return Err(bad());
        }
        Ok(Block {
            // (Copied into an allocation of its own size, as `decode_with`'s is: a slice would
            // keep the restart arrays, copied above, and the file buffer's frame with it, a few
            // hundred bytes a block: TPC-H's peak +12 to +20 MB.)
            data: Bytes::copy_from_slice(&raw[..data_len]),
            restarts,
            rvals,
            common,
            len,
            last_key: last_key.into(),
            ts,
            derived,
            rlog,
        })
    }

    /// Check a decompressed block of the file's format and write it in the compact form,
    /// with a whole key at every `RESTART`th entry (files written before restarts were
    /// share a prefix there).
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
        let mut data = Vec::with_capacity(out.len() + vals.len());
        data.extend_from_slice(&out);
        data.extend_from_slice(&vals);
        restarts.shrink_to_fit();
        rvals.iter_mut().for_each(|v| *v += vbase);
        rvals.shrink_to_fit();
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
        Ok(Block {
            data: Bytes::from(data),
            restarts,
            rvals,
            common,
            len: n,
            last_key: key.as_slice().into(),
            ts: ts.unwrap_or([0xFF; 8]),
            derived,
            rlog,
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

impl SstReader {
    pub fn open(dir: &Path, meta: SstMeta, cache: Arc<BlockCache>) -> Result<SstReader> {
        let path = sst_path(dir, meta.id);
        let file = File::open(&path)?;
        let len = file.metadata()?.len();
        if len < FOOTER_LEN as u64 {
            return Err(internal(format!("sst {} is truncated", path.display())));
        }
        let mut footer = [0u8; FOOTER2_LEN];
        let at = len.saturating_sub(FOOTER2_LEN as u64);
        let got = (len - at) as usize;
        file.read_exact_at(&mut footer[FOOTER2_LEN - got..], at)?;
        let word_at = |i: usize| u64::from_le_bytes(footer[i * 8..i * 8 + 8].try_into().unwrap());
        // (Format 1's six words are the last six; format 2's seven end with its format and magic.)
        let v2 = match word_at(6) {
            MAGIC => false,
            MAGIC2 if got == FOOTER2_LEN => {
                let format = word_at(5);
                if format > FORMAT as u64 {
                    return Err(internal(format!(
                        "sst {} is of storage format {format}, newer than this build reads ({FORMAT})",
                        path.display()
                    )));
                }
                if meta.commit_ts.is_some() {
                    return Err(internal(format!("sst {}: a linked run of format 2", path.display())));
                }
                true
            }
            _ => return Err(internal(format!("sst {} has a bad footer", path.display()))),
        };
        let f = |i: usize| word_at(i + if v2 { 0 } else { 1 });
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
        })
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
        if self.v2 { FORMAT } else { 1 }
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
        let (off, len) = &self.index.at[i];
        let mut buf = vec![0u8; *len as usize];
        self.file.read_exact_at(&mut buf, *off)?;
        if buf.len() < 5 {
            return Err(internal("sst block too short"));
        }
        let crc_pos = buf.len() - 4;
        let crc = u32::from_le_bytes(buf[crc_pos..].try_into().unwrap());
        if crc32fast::hash(&buf[..crc_pos]) != crc {
            return Err(crate::error::Error::new(
                crate::error::code::DATA_CORRUPTED,
                format!("checksum mismatch in {} block {i}", self.path.display()),
            ));
        }
        // (A compressed block's raw length; a raw block's first bytes, unused.)
        let n = u32::from_le_bytes(buf[1..5].try_into().unwrap()) as usize;
        let raw: Bytes = match buf[0] {
            0 => Bytes::from(buf).slice(1..crc_pos),
            1 => Bytes::from(zstd::bulk::decompress(&buf[5..crc_pos], n).map_err(|e| internal(format!("zstd: {e}")))?),
            2 if self.v2 => Bytes::from(lz4_flex::block::decompress(&buf[5..crc_pos], n).map_err(|e| internal(format!("lz4: {e}")))?),
            _ => return Err(internal("unknown block compression")),
        };
        let block = Arc::new(match self.meta.commit_ts {
            _ if self.v2 => Block::load(raw, self.index.key(i))?,
            Some(ts) => Block::decode_linked(&raw, ts)?,
            None => Block::decode(raw)?,
        });
        if fill && !super::in_read_once() {
            let size = block.mem_bytes();
            // (A bulk read's block takes only room the cache has free: it evicts nothing.)
            if super::in_bulk_read() {
                self.cache.insert_if_room(self.cache_id, i as u32, block.clone(), size);
            } else {
                self.cache.insert(self.cache_id, i as u32, block.clone(), size);
            }
        }
        Ok(block)
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
        if self.obsolete.load(Ordering::Acquire) {
            let _ = super::durable::remove_file(&self.path);
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
                for probe in [below, k.clone(), above] {
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

    /// The same entries written in format 1 and format 2 (each codec) read the same, block by
    /// block: keys, values (derived index values, tombstones, versions not the block's, keys
    /// too short for one), positions, seeks, and the blocks' cache form byte for byte.
    #[test]
    fn format2_reads_as_format1() {
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
        let dir = tempfile::tempdir().unwrap();
        let cache = BlockCache::new(64 << 20);
        let write = |id: u64, codec: Codec, v2: bool| {
            let mut w = SstWriter::create_with(dir.path(), id, codec, v2).unwrap();
            for (k, v) in &ents {
                w.add(k, v.as_deref()).unwrap();
            }
            SstReader::open(dir.path(), w.finish().unwrap(), cache.clone()).unwrap()
        };
        let one = write(1, Codec::Zstd, false);
        assert_eq!(one.format(), 1);
        for (id, codec) in [(2, Codec::None), (3, Codec::Zstd), (4, Codec::Lz4)] {
            let two = write(id, codec, true);
            assert_eq!(two.format(), FORMAT);
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
                assert_eq!((&a.data[..], &a.restarts, &a.rvals, a.mem_bytes()), (&b.data[..], &b.restarts, &b.rvals, b.mem_bytes()), "block {i}");
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
                assert_eq!((&b.restarts, &b.rvals, b.common, b.len, &b.last_key, b.ts, b.derived), (&d.restarts, &d.rvals, d.common, d.len, &d.last_key, d.ts, d.derived));
                let mut pos = BlockPos::default();
                for (j, (k, v)) in mine.iter().enumerate() {
                    b.seek(k, &mut pos);
                    assert_eq!((pos.index(), b.value_ref(&pos)), (j, v.as_deref()));
                }
            }
            // (A block of another restart interval reads the same: it says which it has.)
            let mut w = SstWriter::create_with(dir.path(), 10 + id, codec, true).unwrap();
            w.restart_log(2);
            for (k, v) in &ents {
                w.add(k, v.as_deref()).unwrap();
            }
            let four = SstReader::open(dir.path(), w.finish().unwrap(), cache.clone()).unwrap();
            let mut all = Vec::new();
            for i in 0..four.block_count() {
                let b = four.block_with(i, false).unwrap();
                assert_eq!(b.restarts.len(), b.len().div_ceil(4));
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
        b[n - 16..n - 8].copy_from_slice(&(FORMAT as u64 + 1).to_le_bytes());
        std::fs::write(&path, &b).unwrap();
        let e = SstReader::open(dir.path(), meta.clone(), cache.clone()).err().unwrap();
        assert!(e.message.contains(&format!("storage format {}, newer than this build reads ({FORMAT})", FORMAT + 1)), "{}", e.message);
        b[n - 1] ^= 1;
        std::fs::write(&path, &b).unwrap();
        let e = SstReader::open(dir.path(), meta, cache).err().unwrap();
        assert!(e.message.contains("bad footer"), "{}", e.message);
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
