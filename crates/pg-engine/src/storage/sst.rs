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
const BLOCK_TARGET: usize = 16 * 1024;
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

pub fn get_varint(b: &[u8], pos: &mut usize) -> Result<u64> {
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
}

impl SstWriter {
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
        let shared = if self.block_entries == 0 {
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
            self.hashes.push(h);
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

        let nbits = (self.hashes.len() * BLOOM_BITS_PER_KEY).max(64);
        let mut bloom = vec![0u8; nbits.div_ceil(8)];
        let nbits = bloom.len() * 8;
        for h in &self.hashes {
            let (h1, h2) = (*h, (h >> 32) | 1);
            for i in 0..BLOOM_K as u64 {
                let bit = (h1.wrapping_add(i.wrapping_mul(h2)) % nbits as u64) as usize;
                bloom[bit / 8] |= 1 << (bit % 8);
            }
        }
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
        file.sync_all()?;
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
    cache: Arc<BlockCache>,
    cache_id: u64,
    obsolete: AtomicBool,
}

pub type Block = Vec<(Vec<u8>, Option<Bytes>)>;

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
            let k = index_buf[pos..pos + kl].to_vec();
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
            bloom,
            cache,
            cache_id,
            obsolete: AtomicBool::new(false),
        })
    }

    /// Delete the file once the last reader drops it.
    pub fn mark_obsolete(&self) {
        self.obsolete.store(true, Ordering::Release);
    }

    pub fn may_contain(&self, user_key: &[u8]) -> bool {
        let nbits = self.bloom.len() * 8;
        if nbits == 0 {
            return true;
        }
        let h = hash64(user_key);
        let (h1, h2) = (h, (h >> 32) | 1);
        (0..BLOOM_K as u64).all(|i| {
            let bit = (h1.wrapping_add(i.wrapping_mul(h2)) % nbits as u64) as usize;
            self.bloom[bit / 8] & (1 << (bit % 8)) != 0
        })
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
        let raw: Vec<u8> = match buf[0] {
            0 => buf[1..crc_pos].to_vec(),
            1 => {
                let n = u32::from_le_bytes(buf[1..5].try_into().unwrap()) as usize;
                zstd::bulk::decompress(&buf[5..crc_pos], n)
                    .map_err(|e| internal(format!("zstd: {e}")))?
            }
            _ => return Err(internal("unknown block compression")),
        };
        let raw = Bytes::from(raw);
        let mut entries: Block = Vec::new();
        let mut pos = 0;
        let mut prev: Vec<u8> = Vec::new();
        while pos < raw.len() {
            let shared = get_varint(&raw, &mut pos)? as usize;
            let unshared = get_varint(&raw, &mut pos)? as usize;
            let vl = get_varint(&raw, &mut pos)? as usize;
            let mut k = Vec::with_capacity(shared + unshared);
            k.extend_from_slice(&prev[..shared]);
            k.extend_from_slice(&raw[pos..pos + unshared]);
            pos += unshared;
            let v = if vl == 0 {
                None
            } else {
                let v = raw.slice(pos..pos + vl - 1);
                pos += vl - 1;
                Some(v)
            };
            prev.clear();
            prev.extend_from_slice(&k);
            entries.push((k, v));
        }
        let block = Arc::new(entries);
        self.cache
            .insert(self.cache_id, i as u32, block.clone(), raw.len());
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
