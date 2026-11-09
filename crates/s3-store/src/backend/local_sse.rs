//! At-rest encryption for the local directory backend (SSE-C in dev and tests; on S3, S3 does
//! the crypto). It behaves like S3: a read needs the key that wrote the blob, a wrong or
//! missing key is an error, and ranges stay cheap.
//!
//! An encrypted blob is a sequence of *segments* (one for a PUT, one per part for a multipart
//! upload, so completing an upload never needs the key). A segment is AES-256-GCM in chunks of
//! [`CHUNK`] plaintext bytes, each followed by its 16-byte tag:
//!
//! - each segment has its own random 16-byte salt and key, `HMAC-SHA256(customer key,
//!   "roll-local-sse-c/v1/segment" ‖ salt)`, so nonces never repeat across blobs;
//! - a chunk's nonce is its index; its associated data binds the index, whether it's the
//!   segment's last chunk and the segment's length, so chunks can't be reordered, dropped or
//!   cut off unnoticed;
//! - even an empty segment has one (empty) chunk, so it's authenticated too.
//!
//! The salts, lengths and a key check value live in a sidecar (`.s3-sse/<key>.json`), written
//! before the blob and removed after it. Listings report the plaintext size from it, like S3.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::CustomerKey;

/// Plaintext bytes per GCM chunk.
pub(super) const CHUNK: usize = 64 * 1024;
pub(super) const TAG: usize = 16;

/// One encrypted run of plaintext.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Segment {
    /// 16 random bytes, hex.
    pub salt: String,
    /// Plaintext length.
    pub len: u64,
}

/// What an encrypted blob (or part) needs to be read back, minus the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Sidecar {
    pub v: u32,
    /// `HMAC-SHA256(key, "roll-local-sse-c/v1/check")`, hex: tells a wrong key from damage.
    pub check: String,
    pub segments: Vec<Segment>,
}

impl Sidecar {
    pub fn plain_len(&self) -> u64 {
        self.segments.iter().map(|s| s.len).sum()
    }

    pub fn encrypted_len(&self) -> u64 {
        self.segments.iter().map(|s| encrypted_len(s.len)).sum()
    }

    /// The key is the one the blob was written with.
    pub fn verify_key(&self, key: &CustomerKey) -> Result<()> {
        if self.check != check_value(key) {
            bail!("the blob is encrypted with another customer key");
        }
        Ok(())
    }
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

pub(super) fn check_value(key: &CustomerKey) -> String {
    hex::encode(hmac(key.bytes(), &[b"roll-local-sse-c/v1/check"]))
}

fn chunks_of(len: u64) -> u64 {
    len.div_ceil(CHUNK as u64).max(1)
}

/// The stored size of a segment of `len` plaintext bytes.
pub(super) fn encrypted_len(len: u64) -> u64 {
    len + TAG as u64 * chunks_of(len)
}

fn cipher(key: &CustomerKey, salt_hex: &str) -> Result<Aes256Gcm> {
    let salt = hex::decode(salt_hex).context("bad segment salt")?;
    anyhow::ensure!(salt.len() == 16, "bad segment salt");
    let k = zeroize::Zeroizing::new(hmac(key.bytes(), &[b"roll-local-sse-c/v1/segment", &salt]));
    Ok(Aes256Gcm::new_from_slice(k.as_ref()).expect("32-byte key"))
}

fn nonce(index: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&index.to_be_bytes());
    n
}

fn aad(index: u64, last: bool, seg_len: u64) -> [u8; 17] {
    let mut a = [0u8; 17];
    a[..8].copy_from_slice(&index.to_be_bytes());
    a[8] = u8::from(last);
    a[9..].copy_from_slice(&seg_len.to_be_bytes());
    a
}

pub(super) fn new_salt() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("os randomness");
    hex::encode(b)
}

/// Encrypts one segment of a known length as its plaintext streams in.
pub(super) struct Encryptor {
    cipher: Aes256Gcm,
    seg_len: u64,
    index: u64,
    buf: Vec<u8>,
    seen: u64,
}

impl Encryptor {
    pub fn new(key: &CustomerKey, salt: &str, seg_len: u64) -> Result<Self> {
        Ok(Self { cipher: cipher(key, salt)?, seg_len, index: 0, buf: Vec::with_capacity(CHUNK), seen: 0 })
    }

    fn seal(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let last = self.index + 1 == chunks_of(self.seg_len);
        let ct = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce(self.index)), Payload { msg: &self.buf, aad: &aad(self.index, last, self.seg_len) })
            .map_err(|_| anyhow::anyhow!("encrypting a chunk failed"))?;
        out.extend_from_slice(&ct);
        self.buf.clear();
        self.index += 1;
        Ok(())
    }

    /// Encrypt what `data` completes; ciphertext goes to `out`.
    pub fn update(&mut self, mut data: &[u8], out: &mut Vec<u8>) -> Result<()> {
        self.seen += data.len() as u64;
        anyhow::ensure!(self.seen <= self.seg_len, "more plaintext than the segment's length");
        while !data.is_empty() {
            let n = (CHUNK - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..n]);
            data = &data[n..];
            // A full chunk is sealed once more data follows, or right away if it's the last.
            if self.buf.len() == CHUNK && (self.index + 1 < chunks_of(self.seg_len)) {
                self.seal(out)?;
            }
        }
        Ok(())
    }

    /// The final chunk.
    pub fn finish(mut self, out: &mut Vec<u8>) -> Result<()> {
        anyhow::ensure!(self.seen == self.seg_len, "the segment ended early");
        while self.index < chunks_of(self.seg_len) {
            self.seal(out)?;
        }
        Ok(())
    }
}

/// Where plaintext `[start, end)` of a sidecar's blob lies: per touched chunk, `(file offset,
/// stored chunk length, segment index, chunk index, bytes to skip, bytes to keep)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ChunkRead {
    pub offset: u64,
    pub stored: usize,
    pub segment: usize,
    pub index: u64,
    pub skip: usize,
    pub keep: usize,
}

/// The chunks covering plaintext `[start, end)` (already clamped to the blob), in order.
pub(super) fn plan(side: &Sidecar, start: u64, end: u64) -> Vec<ChunkRead> {
    let mut out = Vec::new();
    let (mut seg_plain, mut seg_file) = (0u64, 0u64);
    for (si, seg) in side.segments.iter().enumerate() {
        let (a, b) = (start.max(seg_plain), end.min(seg_plain + seg.len));
        if a < b {
            let first = (a - seg_plain) / CHUNK as u64;
            let last = (b - seg_plain - 1) / CHUNK as u64;
            for ci in first..=last {
                let c_start = ci * CHUNK as u64;
                let c_len = (seg.len - c_start).min(CHUNK as u64);
                let lo = (a - seg_plain).max(c_start) - c_start;
                let hi = (b - seg_plain).min(c_start + c_len) - c_start;
                out.push(ChunkRead { offset: seg_file + ci * (CHUNK + TAG) as u64, stored: c_len as usize + TAG, segment: si, index: ci, skip: lo as usize, keep: (hi - lo) as usize });
            }
        }
        seg_plain += seg.len;
        seg_file += encrypted_len(seg.len);
    }
    out
}

/// Decrypt one stored chunk.
pub(super) fn open(key: &CustomerKey, side: &Sidecar, c: &ChunkRead, stored: &[u8]) -> Result<Vec<u8>> {
    let seg = &side.segments[c.segment];
    let last = c.index + 1 == chunks_of(seg.len);
    cipher(key, &seg.salt)?
        .decrypt(Nonce::from_slice(&nonce(c.index)), Payload { msg: stored, aad: &aad(c.index, last, seg.len) })
        .map_err(|_| anyhow::anyhow!("an encrypted chunk failed authentication (damaged blob or wrong key)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encrypt(key: &CustomerKey, salt: &str, data: &[u8], step: usize) -> Vec<u8> {
        let mut e = Encryptor::new(key, salt, data.len() as u64).unwrap();
        let mut out = Vec::new();
        for c in data.chunks(step.max(1)) {
            e.update(c, &mut out).unwrap();
        }
        e.finish(&mut out).unwrap();
        out
    }

    fn read(key: &CustomerKey, side: &Sidecar, file: &[u8], start: u64, end: u64) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for c in plan(side, start, end) {
            let plain = open(key, side, &c, &file[c.offset as usize..c.offset as usize + c.stored])?;
            out.extend_from_slice(&plain[c.skip..c.skip + c.keep]);
        }
        Ok(out)
    }

    #[test]
    fn round_trips_with_ranges_across_chunks_and_segments() {
        let key = CustomerKey::new([3u8; 32]);
        let a: Vec<u8> = (0..(CHUNK * 2 + 100)).map(|i| (i % 251) as u8).collect();
        let b: Vec<u8> = (0..CHUNK).map(|i| (i % 13) as u8).collect();
        let (sa, sb) = (new_salt(), new_salt());
        let mut file = encrypt(&key, &sa, &a, 1000);
        assert_eq!(file.len() as u64, encrypted_len(a.len() as u64));
        let fb = encrypt(&key, &sb, &b, CHUNK);
        assert_eq!(fb.len() as u64, encrypted_len(b.len() as u64));
        file.extend_from_slice(&fb);
        let side = Sidecar { v: 1, check: check_value(&key), segments: vec![Segment { salt: sa, len: a.len() as u64 }, Segment { salt: sb, len: b.len() as u64 }] };
        assert_eq!(side.encrypted_len(), file.len() as u64);
        let all: Vec<u8> = a.iter().chain(&b).copied().collect();
        assert_eq!(side.plain_len(), all.len() as u64);
        for (s, e) in [(0, all.len()), (0, 1), (CHUNK - 1, CHUNK + 1), (CHUNK * 2 + 99, CHUNK * 2 + 101), (5, all.len() - 5), (all.len() - 1, all.len())] {
            assert_eq!(read(&key, &side, &file, s as u64, e as u64).unwrap(), all[s..e], "{s}..{e}");
        }
        // The plaintext is nowhere in the file.
        assert!(!file.windows(64).any(|w| w == &a[1000..1064]));
        // Another key fails, before and during decryption.
        let other = CustomerKey::new([4u8; 32]);
        assert!(side.verify_key(&other).is_err());
        side.verify_key(&key).unwrap();
        assert!(read(&other, &side, &file, 0, 10).is_err());
        // Any damage fails its chunk; swapped chunks and a cut-off last chunk too.
        let mut bad = file.clone();
        bad[CHUNK + TAG + 7] ^= 1;
        assert!(read(&key, &side, &bad, 0, all.len() as u64).is_err());
        assert!(read(&key, &side, &bad, 0, 10).is_ok(), "other chunks stay readable");
        let mut swapped = file.clone();
        let (c0, c1) = (file[..CHUNK + TAG].to_vec(), file[CHUNK + TAG..2 * (CHUNK + TAG)].to_vec());
        swapped[..CHUNK + TAG].copy_from_slice(&c1);
        swapped[CHUNK + TAG..2 * (CHUNK + TAG)].copy_from_slice(&c0);
        assert!(read(&key, &side, &swapped, 0, 10).is_err());
        let short = Sidecar { segments: vec![Segment { len: (CHUNK * 2) as u64, ..side.segments[0].clone() }], ..side.clone() };
        assert!(read(&key, &short, &file, 0, CHUNK as u64 * 2).is_err(), "a segment can't be truncated to a chunk boundary");
    }

    #[test]
    fn empty_and_exact_segments() {
        let key = CustomerKey::new([9u8; 32]);
        let salt = new_salt();
        let empty = encrypt(&key, &salt, b"", 1);
        assert_eq!(empty.len(), TAG, "an empty segment is still authenticated");
        let side = Sidecar { v: 1, check: check_value(&key), segments: vec![Segment { salt: salt.clone(), len: 0 }] };
        assert!(plan(&side, 0, 0).is_empty());
        let exact: Vec<u8> = vec![5u8; CHUNK * 2];
        let f = encrypt(&key, &salt, &exact, 777);
        assert_eq!(f.len(), CHUNK * 2 + 2 * TAG);
        let side = Sidecar { v: 1, check: check_value(&key), segments: vec![Segment { salt, len: exact.len() as u64 }] };
        assert_eq!(read(&key, &side, &f, 0, exact.len() as u64).unwrap(), exact);
        // The encryptor refuses more or less than it was told.
        let mut e = Encryptor::new(&key, &new_salt(), 3).unwrap();
        let mut out = Vec::new();
        assert!(e.update(b"abcd", &mut out).is_err());
        let e = Encryptor::new(&key, &new_salt(), 3).unwrap();
        assert!(e.finish(&mut out).is_err());
    }
}
