//! Sorted string tables: immutable files of (storage key, value) in key order.
//!
//! ```text
//! [block 0] [block 1] … [index] [bloom] [footer: index off/len, bloom off/len, entries, magic]
//! block  = compression byte (0 raw, 1 zstd) + entries
//! entry  = shared varint | unshared varint | value_len+1 varint (0 = tombstone) | key suffix | value
//! index  = per block: last key, offset, length
//! bloom  = over user keys (storage key minus the 8-byte version)
//! ```

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
pub(crate) const BLOCK_TARGET: usize = 16 * 1024;
const FOOTER_LEN: usize = 48;
const BLOOM_BITS_PER_KEY: usize = 10;
const BLOOM_K: u32 = 7;

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
/// seven per bloom probe).
#[derive(Clone, Copy)]
pub(crate) struct FastMod {
    m: u128,
    d: u64,
}

impl FastMod {
    pub(crate) fn new(d: u64) -> FastMod {
        FastMod {
            m: (u128::MAX / d.max(1) as u128).wrapping_add(1),
            d,
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

/// The bloom filter bits of a key's hash `h` (`nbits` = the filter's bits).
#[inline]
fn bloom_bits(h: u64, nbits: FastMod) -> impl Iterator<Item = usize> {
    let (h1, h2) = (h, (h >> 32) | 1);
    (0..BLOOM_K as u64).map(move |i| nbits.rem(h1.wrapping_add(i.wrapping_mul(h2))) as usize)
}

fn bloom_set(bloom: &mut [u8], nbits: FastMod, h: u64) {
    for bit in bloom_bits(h, nbits) {
        bloom[bit / 8] |= 1 << (bit % 8);
    }
}

pub fn sst_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id:016x}.sst"))
}

pub struct SstWriter {
    path: PathBuf,
    id: u64,
    out: BufWriter<File>,
    block: Vec<u8>,
    block_entries: usize,
    prev_key: Vec<u8>,
    index: Vec<(Vec<u8>, u64, u32)>,
    hashes: Vec<u64>,
    last_user_hash: Option<u64>,
    offset: u64,
    entries: u64,
    first_key: Option<Vec<u8>>,
    min_ts: u64,
    max_ts: u64,
    compress: bool,
    /// fsync the file when it is finished (not for a transaction's spilled writes, which
    /// a crash discards).
    sync: bool,
    /// The bloom filter, when sized up front (`expect_keys`): its bits are set as keys come
    /// instead of keeping every key's hash until `finish`.
    bloom: Option<(Vec<u8>, FastMod)>,
}

impl SstWriter {
    /// A file of a transaction's spilled writes (`txn::spill`): uncompressed, not fsynced.
    pub fn create_temp(dir: &Path, id: u64) -> Result<Self> {
        let mut w = Self::create(dir, id, false)?;
        w.sync = false;
        Ok(w)
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

    pub fn create(dir: &Path, id: u64, compress: bool) -> Result<Self> {
        let path = sst_path(dir, id);
        let tmp = path.with_extension("tmp");
        let out = BufWriter::with_capacity(256 * 1024, File::create(&tmp)?);
        Ok(SstWriter {
            path,
            id,
            out,
            block: Vec::with_capacity(BLOCK_TARGET + 1024),
            block_entries: 0,
            prev_key: Vec::new(),
            index: Vec::new(),
            hashes: Vec::new(),
            last_user_hash: None,
            offset: 0,
            entries: 0,
            first_key: None,
            min_ts: u64::MAX,
            max_ts: 0,
            compress,
            sync: true,
            bloom: None,
        })
    }

    pub fn entries(&self) -> u64 {
        self.entries
    }

    pub fn approx_size(&self) -> u64 {
        self.offset + self.block.len() as u64
    }

    /// Keys must arrive in strictly increasing order.
    pub fn add(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<()> {
        if self.first_key.is_none() {
            self.first_key = Some(key.to_vec());
        }
        // (A whole key at every restart: a cached block keeps the file's entries as they are.)
        let shared = if self.block_entries.is_multiple_of(RESTART) {
            0
        } else {
            self.prev_key
                .iter()
                .zip(key)
                .take_while(|(a, b)| a == b)
                .count()
        };
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
        self.prev_key.clear();
        self.prev_key.extend_from_slice(key);
        self.block_entries += 1;
        self.entries += 1;
        let ts = key_ts(key);
        self.min_ts = self.min_ts.min(ts);
        self.max_ts = self.max_ts.max(ts);
        let h = hash64(user_key(key));
        if self.last_user_hash != Some(h) {
            match &mut self.bloom {
                Some((b, m)) => bloom_set(b, *m, h),
                None => self.hashes.push(h),
            }
            self.last_user_hash = Some(h);
        }
        if self.block.len() >= BLOCK_TARGET {
            self.finish_block()?;
        }
        Ok(())
    }

    fn finish_block(&mut self) -> Result<()> {
        if self.block_entries == 0 {
            return Ok(());
        }
        let raw = std::mem::take(&mut self.block);
        let mut data = Vec::with_capacity(raw.len() + 1);
        if self.compress {
            let c = zstd::bulk::compress(&raw, 1).map_err(|e| internal(format!("zstd: {e}")))?;
            if c.len() < raw.len() * 7 / 8 {
                data.push(1);
                data.extend_from_slice(&(raw.len() as u32).to_le_bytes());
                data.extend_from_slice(&c);
            } else {
                data.push(0);
                data.extend_from_slice(&raw);
            }
        } else {
            data.push(0);
            data.extend_from_slice(&raw);
        }
        let crc = crc32fast::hash(&data);
        data.extend_from_slice(&crc.to_le_bytes());
        self.out.write_all(&data)?;
        self.index
            .push((self.prev_key.clone(), self.offset, data.len() as u32));
        self.offset += data.len() as u64;
        self.block = raw;
        self.block.clear();
        self.block_entries = 0;
        Ok(())
    }

    pub fn finish(mut self) -> Result<SstMeta> {
        self.finish_block()?;
        let mut index = Vec::new();
        put_varint(&mut index, self.index.len() as u64);
        for (k, off, len) in &self.index {
            put_varint(&mut index, k.len() as u64);
            index.extend_from_slice(k);
            put_varint(&mut index, *off);
            put_varint(&mut index, *len as u64);
        }
        let index_off = self.offset;
        self.out.write_all(&index)?;
        self.offset += index.len() as u64;

        let bloom = match self.bloom.take() {
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

        let mut footer = Vec::with_capacity(FOOTER_LEN);
        for v in [
            index_off,
            index.len() as u64,
            bloom_off,
            bloom.len() as u64,
            self.entries,
            MAGIC,
        ] {
            footer.extend_from_slice(&v.to_le_bytes());
        }
        self.out.write_all(&footer)?;
        self.offset += footer.len() as u64;
        let file = self
            .out
            .into_inner()
            .map_err(|e| internal(format!("flush sst: {e}")))?;
        if self.sync {
            file.sync_all()?;
        }
        std::fs::rename(self.path.with_extension("tmp"), &self.path)?;
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
        })
    }

    pub fn abandon(self) {
        let _ = std::fs::remove_file(self.path.with_extension("tmp"));
    }
}

pub struct SstReader {
    pub meta: SstMeta,
    file: File,
    path: PathBuf,
    /// (last key, offset, len)
    index: Vec<(Vec<u8>, u64, u32)>,
    bloom: Vec<u8>,
    /// Remainders by the bloom filter's bit count.
    bloom_mod: FastMod,
    cache: Arc<BlockCache>,
    cache_id: u64,
    obsolete: AtomicBool,
}

/// Entries between restarts in a cached block: every `RESTART`th entry stores its whole key
/// (shared = 0), so a seek searches the restarts and decodes at most `RESTART - 1` entries.
const RESTART: usize = 16;

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
/// suffix | value`: flag `F_TS` puts the block's version after the suffix, `F_DERIVED`
/// makes the value field the length of the key's tail the value ends with.
pub struct Block {
    data: Bytes,
    /// The offset of every `RESTART`th entry.
    restarts: Vec<u32>,
    len: usize,
    last_key: Box<[u8]>,
    /// The version (a key's last 8 bytes) entries flagged `F_TS` end with.
    ts: [u8; 8],
    /// Whether some entry's value is derived (`expand` then copies the values).
    derived: bool,
}

const F_TS: usize = 1;
const F_DERIVED: usize = 2;

/// A position in a block: entry `i`, its whole key and value, and where the next one starts.
#[derive(Default, Clone)]
pub struct BlockPos {
    i: usize,
    next: u32,
    key: Vec<u8>,
    val_off: u32,
    /// `TOMBSTONE`: a deletion; `DERIVED`: the value is `dval`.
    val_len: u32,
    /// A derived value, made from the key.
    dval: Vec<u8>,
}

const TOMBSTONE: u32 = u32::MAX;
const DERIVED: u32 = u32::MAX - 1;

impl BlockPos {
    pub fn index(&self) -> usize {
        self.i
    }

    pub fn key(&self) -> &[u8] {
        &self.key
    }
}

/// The header of the entry at `pos`: (shared, unshared, value field), `pos` past it.
fn entry_head(b: &[u8], pos: &mut usize) -> Result<(usize, usize, usize)> {
    let shared = get_varint(b, pos)? as usize;
    let unshared = get_varint(b, pos)? as usize;
    let vl = get_varint(b, pos)? as usize;
    Ok((shared, unshared, vl))
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
        // (`decode` wrote every entry of the block.)
        let (shared, us, vf) = entry_head(b, &mut p).expect("a checked block");
        let unshared = us >> 2;
        pos.key.truncate(shared);
        pos.key.extend_from_slice(&b[p..p + unshared]);
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
            pos.val_off = p as u32;
            pos.val_len = (vf - 1) as u32;
            p += vf - 1;
        }
        pos.next = p as u32;
    }

    /// `pos` at entry `i` (`len`: past the end).
    pub fn at(&self, i: usize, pos: &mut BlockPos) {
        if i >= self.len {
            pos.i = self.len;
            return;
        }
        let r = i / RESTART;
        pos.next = self.restarts[r];
        pos.key.clear();
        pos.i = r * RESTART;
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
        let mut p = self.restarts[r] as usize;
        let (_, us, _) = entry_head(&self.data, &mut p).expect("a checked block");
        let stored = &self.data[p..p + (us >> 2)];
        cmp_split(stored, if us & F_TS != 0 { &self.ts } else { &[] }, key)
    }

    /// `pos` at the first entry whose key is >= `key` (`len`: none).
    pub fn seek(&self, key: &[u8], pos: &mut BlockPos) {
        // The first restart whose key is >= `key`; the entries from the one before it.
        let (mut lo, mut hi) = (0, self.restarts.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.restart_cmp(mid, key).is_lt() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        self.at(lo.saturating_sub(1) * RESTART, pos);
        while pos.i < self.len && pos.key.as_slice() < key {
            self.step(pos);
        }
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

    /// The bytes it holds (what the block cache counts).
    pub fn mem_bytes(&self) -> usize {
        self.data.len() + self.restarts.capacity() * 4 + self.last_key.len() + std::mem::size_of::<Block>()
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

    /// Check a decompressed block of the file's format and write it in the compact form,
    /// with a whole key at every `RESTART`th entry (files written before restarts were
    /// share a prefix there).
    fn decode(raw: Bytes) -> Result<Block> {
        let bad = || internal("bad sst block");
        let mut restarts = Vec::with_capacity(raw.len() / 512 + 1);
        let mut out = Vec::with_capacity(raw.len());
        let mut key: Vec<u8> = Vec::new();
        let mut ts = None;
        let mut derived = false;
        let (mut pos, mut n) = (0, 0usize);
        while pos < raw.len() {
            let (shared, unshared, vl) = entry_head(&raw, &mut pos)?;
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
            let value = (vl > 0).then(|| &raw[pos + unshared..end]);
            pos = end;
            let klen = key.len();
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
            let share = if n.is_multiple_of(RESTART) {
                restarts.push(out.len() as u32);
                0
            } else {
                common.min(ukey)
            };
            put_varint(&mut out, share as u64);
            put_varint(&mut out, ((ukey - share) << 2 | flags) as u64);
            put_varint(&mut out, tail.unwrap_or(vl) as u64);
            out.extend_from_slice(&key[share..ukey]);
            if let (Some(v), None) = (value, tail) {
                out.extend_from_slice(v);
            }
            n += 1;
        }
        out.shrink_to_fit();
        restarts.shrink_to_fit();
        Ok(Block {
            data: Bytes::from(out),
            restarts,
            len: n,
            last_key: key.as_slice().into(),
            ts: ts.unwrap_or([0xFF; 8]),
            derived,
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

/// A block of a linked run (`SstMeta::commit_ts`) in the store's format: every key's version
/// is the commit's, and the run's values (a tag, the command id, the value: `txn::spill`)
/// are plain ones, a delete a tombstone. (A linked run has one version of each key.)
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
        let mut footer = [0u8; FOOTER_LEN];
        file.read_exact_at(&mut footer, len - FOOTER_LEN as u64)?;
        let f = |i: usize| u64::from_le_bytes(footer[i * 8..i * 8 + 8].try_into().unwrap());
        if f(5) != MAGIC {
            return Err(internal(format!("sst {} has a bad footer", path.display())));
        }
        let mut index_buf = vec![0u8; f(1) as usize];
        file.read_exact_at(&mut index_buf, f(0))?;
        let mut bloom = vec![0u8; f(3) as usize];
        file.read_exact_at(&mut bloom, f(2))?;
        let mut pos = 0;
        let n = get_varint(&index_buf, &mut pos)? as usize;
        let mut index = Vec::with_capacity(n);
        for _ in 0..n {
            let kl = get_varint(&index_buf, &mut pos)? as usize;
            let mut k = index_buf[pos..pos + kl].to_vec();
            if let Some(ts) = meta.commit_ts {
                set_ts(&mut k, ts);
            }
            pos += kl;
            let off = get_varint(&index_buf, &mut pos)?;
            let l = get_varint(&index_buf, &mut pos)? as u32;
            index.push((k, off, l));
        }
        let cache_id = cache.next_file_id();
        Ok(SstReader {
            meta,
            file,
            path,
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

    pub fn may_contain(&self, user_key: &[u8]) -> bool {
        if self.bloom.is_empty() {
            return true;
        }
        let h = hash64(user_key);
        bloom_bits(h, self.bloom_mod).all(|bit| self.bloom[bit / 8] & (1 << (bit % 8)) != 0)
    }

    pub fn block_count(&self) -> usize {
        self.index.len()
    }

    /// Index of the first block whose last key is >= `key`.
    pub fn find_block(&self, key: &[u8]) -> usize {
        self.index
            .partition_point(|(last, _, _)| last.as_slice() < key)
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
        let (_, off, len) = &self.index[i];
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
        let raw: Bytes = match buf[0] {
            0 => Bytes::from(buf).slice(1..crc_pos),
            1 => {
                let n = u32::from_le_bytes(buf[1..5].try_into().unwrap()) as usize;
                Bytes::from(
                    zstd::bulk::decompress(&buf[5..crc_pos], n)
                        .map_err(|e| internal(format!("zstd: {e}")))?,
                )
            }
            _ => return Err(internal("unknown block compression")),
        };
        let raw = match self.meta.commit_ts {
            Some(ts) => relink(&raw, ts)?,
            None => raw,
        };
        let block = Arc::new(Block::decode(raw)?);
        if fill {
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

impl Drop for SstReader {
    fn drop(&mut self) {
        self.cache.forget_file(self.cache_id);
        if self.obsolete.load(Ordering::Acquire) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    if want < ents.len() {
                        assert_eq!(pos.key(), ents[want].0.as_slice());
                    }
                }
            }
            b.seek(b"", &mut pos);
            assert_eq!(pos.index(), 0);
            b.seek(&[0xFF], &mut pos);
            assert_eq!(pos.index(), b.len());
        }
    }
}
