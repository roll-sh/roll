//! Reading `Data.db` sequentially: compressed chunks (`CompressedChunkReader`: a CRC32 of each
//! stored chunk follows it, big-endian) or an uncompressed file checked against `CRC.db`
//! (`ChecksumWriter`: a big-endian chunk size, then a CRC32 per chunk), and the whole stored file
//! against `Digest.crc32` (the decimal CRC32 of every byte, chunk checksums included).
//!
//! Memory is bounded by the read-ahead window plus one decompressed chunk: files are read
//! through [`RandomAccess::read_at`] in windows, never whole.

use crate::compression::{Codec, CompressionInfo};
use crate::input::In;
use anyhow::{Result, bail};
use bytes::{Bytes, BytesMut};
use std::sync::Arc;

/// A file that can be read at any offset (an object in S3, a local file, memory).
pub trait RandomAccess: Send + Sync {
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// `len` bytes at `offset` (all of them; the range is within the file).
    fn read_at(&self, offset: u64, len: usize) -> Result<Bytes>;
}

impl RandomAccess for Bytes {
    fn len(&self) -> u64 {
        Bytes::len(self) as u64
    }

    fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
        let end = offset.checked_add(len as u64).filter(|e| *e <= Bytes::len(self) as u64).ok_or_else(|| anyhow::anyhow!("read past the end of the file"))?;
        Ok(self.slice(offset as usize..end as usize))
    }
}

/// Forward-only reads of a [`RandomAccess`] file through a window, optionally checksumming
/// every byte read in order.
pub struct Readahead {
    src: Arc<dyn RandomAccess>,
    len: u64,
    /// Bytes `buf_at..buf_at + buf.len()` of the file.
    buf: Bytes,
    buf_at: u64,
    pos: u64,
    window: usize,
    crc: Option<crc32fast::Hasher>,
}

impl Readahead {
    pub fn new(src: Arc<dyn RandomAccess>, window: usize, checksum: bool) -> Self {
        let len = src.len();
        Self { src, len, buf: Bytes::new(), buf_at: 0, pos: 0, window: window.max(4096), crc: checksum.then(crc32fast::Hasher::new) }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn pos(&self) -> u64 {
        self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.len
    }

    /// The next `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<Bytes> {
        if n as u64 > self.len - self.pos {
            bail!("unexpected end of file: {n} bytes wanted at {} of {}", self.pos, self.len);
        }
        let in_buf = (self.buf_at + self.buf.len() as u64).saturating_sub(self.pos) as usize;
        let out = if self.pos >= self.buf_at && n <= in_buf {
            let at = (self.pos - self.buf_at) as usize;
            self.buf.slice(at..at + n)
        } else if n >= self.window {
            self.src.read_at(self.pos, n)?
        } else {
            let want = (self.window as u64).min(self.len - self.pos) as usize;
            let mut next = BytesMut::with_capacity(want.max(n));
            if self.pos >= self.buf_at && in_buf > 0 {
                let at = (self.pos - self.buf_at) as usize;
                next.extend_from_slice(&self.buf[at..]);
            }
            let have = next.len();
            next.extend_from_slice(&self.src.read_at(self.pos + have as u64, want.max(n) - have)?);
            self.buf = next.freeze();
            self.buf_at = self.pos;
            self.buf.slice(..n)
        };
        if out.len() != n {
            bail!("short read: {} of {n} bytes at {}", out.len(), self.pos);
        }
        self.pos += n as u64;
        if let Some(c) = &mut self.crc {
            c.update(&out);
        }
        Ok(out)
    }

    /// Skip forward to `to` (checksummed readers read the gap).
    pub fn seek_forward(&mut self, to: u64) -> Result<()> {
        if to < self.pos || to > self.len {
            bail!("bad seek to {to} from {}", self.pos);
        }
        if self.crc.is_some() {
            while self.pos < to {
                let n = ((to - self.pos) as usize).min(self.window);
                self.take(n)?;
            }
        } else {
            self.pos = to;
        }
        Ok(())
    }

    /// The CRC32 of everything read, once all of the file was read.
    pub fn checksum(&self) -> Option<u32> {
        self.crc.as_ref().filter(|_| self.pos == self.len).map(|c| c.clone().finalize())
    }

    fn be32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes(b[..].try_into().expect("4")))
    }

    fn be64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes(b[..].try_into().expect("8")))
    }
}

enum Mode {
    Compressed { info: CompressionInfo, data: Readahead, offsets: Readahead, next: u64, next_offset: Option<u64>, scratch: Vec<u8> },
    Plain { data: Readahead, crc: Option<(usize, Readahead)> },
}

/// The uncompressed bytes of a `Data.db`, in order.
pub struct DataReader {
    mode: Mode,
    chunk: Bytes,
    chunk_pos: usize,
    /// Uncompressed position of the start of `chunk`.
    chunk_at: u64,
    len: u64,
    digest: Option<u32>,
    pub(crate) path: String,
}

impl DataReader {
    /// A compressed data file. `offsets` is CompressionInfo.db (read from
    /// `info.offsets_at` on).
    pub fn compressed(path: String, data: Arc<dyn RandomAccess>, info: CompressionInfo, offsets: Arc<dyn RandomAccess>, window: usize, digest: Option<u32>) -> Result<DataReader> {
        let mut off = Readahead::new(offsets, 64 << 10, false);
        off.seek_forward(info.offsets_at)?;
        let len = info.data_length;
        let data = Readahead::new(data, window, digest.is_some());
        let scratch = Vec::with_capacity(info.chunk_length as usize);
        Ok(DataReader { mode: Mode::Compressed { info, data, offsets: off, next: 0, next_offset: None, scratch }, chunk: Bytes::new(), chunk_pos: 0, chunk_at: 0, len, digest, path })
    }

    /// An uncompressed data file, with its CRC.db when there is one.
    pub fn plain(path: String, data: Arc<dyn RandomAccess>, crc: Option<Arc<dyn RandomAccess>>, window: usize, digest: Option<u32>) -> Result<DataReader> {
        let len = data.len();
        let crc = match crc {
            Some(c) => {
                let mut r = Readahead::new(c, 64 << 10, false);
                let size = r.be32()?;
                if size == 0 || size > crate::compression::MAX_CHUNK {
                    bail!("corrupt CRC.db: chunk size {size}");
                }
                if r.len() - 4 < len.div_ceil(size as u64) * 4 {
                    bail!("corrupt CRC.db: too few checksums for {len} bytes");
                }
                Some((size as usize, r))
            }
            None => None,
        };
        let data = Readahead::new(data, window, digest.is_some());
        Ok(DataReader { mode: Mode::Plain { data, crc }, chunk: Bytes::new(), chunk_pos: 0, chunk_at: 0, len, digest, path })
    }

    /// Uncompressed length.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn pos(&self) -> u64 {
        self.chunk_at + self.chunk_pos as u64
    }

    pub fn is_empty(&self) -> bool {
        self.pos() >= self.len
    }

    fn next_chunk(&mut self) -> Result<()> {
        self.chunk_at += self.chunk.len() as u64;
        self.chunk_pos = 0;
        match &mut self.mode {
            Mode::Compressed { info, data, offsets, next, next_offset, scratch } => {
                if *next >= info.chunks {
                    bail!("{}: unexpected end of data", self.path);
                }
                let off = match next_offset.take() {
                    Some(o) => o,
                    None => offsets.be64()?,
                };
                let end = if *next + 1 < info.chunks {
                    let e = offsets.be64()?;
                    *next_offset = Some(e);
                    e
                } else {
                    data.len()
                };
                if off != data.pos() || end < off + 4 || end > data.len() {
                    bail!("{}: corrupt chunk offsets ({off}..{end}, at {})", self.path, data.pos());
                }
                let stored = data.take((end - off - 4) as usize)?;
                let crc = data.be32()?;
                if crc32fast::hash(&stored) != crc {
                    bail!("{}: chunk {} checksum mismatch (corrupt or truncated)", self.path, next);
                }
                let expect = (info.data_length - *next * info.chunk_length as u64).min(info.chunk_length as u64) as usize;
                if stored.len() as u64 >= info.max_compressed_length as u64 || info.codec == Codec::Noop {
                    self.chunk = stored;
                } else {
                    info.codec.decompress(&stored, info.chunk_length as usize, scratch).map_err(|e| anyhow::anyhow!("{}: chunk {next}: {e}", self.path))?;
                    self.chunk = Bytes::copy_from_slice(scratch);
                }
                if self.chunk.len() != expect {
                    bail!("{}: chunk {next} has {} bytes, expected {expect}", self.path, self.chunk.len());
                }
                *next += 1;
            }
            Mode::Plain { data, crc } => {
                let size = crc.as_ref().map_or(1 << 20, |c| c.0);
                let n = (size as u64).min(data.len() - data.pos()) as usize;
                if n == 0 {
                    bail!("{}: unexpected end of data", self.path);
                }
                self.chunk = data.take(n)?;
                if let Some((_, sums)) = crc {
                    let want = sums.be32()?;
                    if crc32fast::hash(&self.chunk) != want {
                        bail!("{}: checksum mismatch at {} (CRC.db)", self.path, self.chunk_at);
                    }
                }
            }
        }
        Ok(())
    }

    /// Check `Digest.crc32` once everything was read.
    pub fn finish(&mut self) -> Result<()> {
        if self.pos() != self.len {
            bail!("{}: {} bytes left unread", self.path, self.len - self.pos());
        }
        let data = match &mut self.mode {
            Mode::Compressed { data, .. } | Mode::Plain { data, .. } => data,
        };
        if let Some(want) = self.digest {
            match data.checksum() {
                Some(got) if got == want => {}
                Some(got) => bail!("{}: Digest.crc32 mismatch (file {got}, digest {want})", self.path),
                None => bail!("{}: {} trailing bytes after the data", self.path, data.len() - data.pos()),
            }
        }
        Ok(())
    }
}

impl In for DataReader {
    fn remaining(&self) -> u64 {
        self.len - self.pos()
    }

    fn bytes(&mut self, n: usize) -> Result<Bytes> {
        if n as u64 > self.remaining() {
            bail!("{}: corrupt data: {n} bytes wanted, {} left", self.path, self.remaining());
        }
        if self.chunk.len() - self.chunk_pos >= n {
            let b = self.chunk.slice(self.chunk_pos..self.chunk_pos + n);
            self.chunk_pos += n;
            return Ok(b);
        }
        let mut out = BytesMut::with_capacity(n);
        while out.len() < n {
            if self.chunk_pos == self.chunk.len() {
                self.next_chunk()?;
            }
            let take = (n - out.len()).min(self.chunk.len() - self.chunk_pos);
            out.extend_from_slice(&self.chunk[self.chunk_pos..self.chunk_pos + take]);
            self.chunk_pos += take;
        }
        Ok(out.freeze())
    }

    fn skip(&mut self, n: u64) -> Result<()> {
        if n > self.remaining() {
            bail!("{}: corrupt data: skipping {n} bytes, {} left", self.path, self.remaining());
        }
        let mut left = n;
        while left > 0 {
            if self.chunk_pos == self.chunk.len() {
                self.next_chunk()?;
            }
            let take = left.min((self.chunk.len() - self.chunk_pos) as u64);
            self.chunk_pos += take as usize;
            left -= take;
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8> {
        if self.chunk_pos == self.chunk.len() {
            if self.is_empty() {
                bail!("{}: corrupt data: unexpected end", self.path);
            }
            self.next_chunk()?;
        }
        let b = self.chunk[self.chunk_pos];
        self.chunk_pos += 1;
        Ok(b)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Compress `raw` into Cassandra's layout: (Data.db, CompressionInfo.db).
    pub fn compressed_file(raw: &[u8], chunk: usize, lz4: bool) -> (Vec<u8>, Vec<u8>) {
        let mut data = Vec::new();
        let mut offsets = Vec::new();
        for c in raw.chunks(chunk) {
            offsets.push(data.len() as u64);
            let stored = if lz4 {
                let mut v = (c.len() as u32).to_le_bytes().to_vec();
                v.extend_from_slice(&lz4_flex::block::compress(c));
                v
            } else {
                c.to_vec()
            };
            data.extend_from_slice(&stored);
            data.extend_from_slice(&crc32fast::hash(&stored).to_be_bytes());
        }
        let class = if lz4 { "LZ4Compressor" } else { "NoopCompressor" };
        let mut info = Vec::new();
        info.extend_from_slice(&(class.len() as u16).to_be_bytes());
        info.extend_from_slice(class.as_bytes());
        info.extend_from_slice(&0u32.to_be_bytes());
        info.extend_from_slice(&(chunk as u32).to_be_bytes());
        info.extend_from_slice(&(i32::MAX as u32).to_be_bytes());
        info.extend_from_slice(&(raw.len() as u64).to_be_bytes());
        info.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for o in offsets {
            info.extend_from_slice(&o.to_be_bytes());
        }
        (data, info)
    }

    pub fn reader(data: Vec<u8>, info: Vec<u8>, window: usize, digest: Option<u32>) -> Result<DataReader> {
        let ci = CompressionInfo::parse(Bytes::from(info.clone()), info.len() as u64, true)?;
        DataReader::compressed("test".into(), Arc::new(Bytes::from(data)), ci, Arc::new(Bytes::from(info)), window, digest)
    }

    fn sample() -> Vec<u8> {
        (0..100_000u32).flat_map(|i| (i.wrapping_mul(2654435761) % 1000).to_be_bytes()).collect()
    }

    #[test]
    fn reads_across_chunks_and_checks_the_digest() {
        let raw = sample();
        let (data, info) = compressed_file(&raw, 4096, true);
        let digest = crc32fast::hash(&data);
        for window in [4096, 10_000, 1 << 20] {
            let mut r = reader(data.clone(), info.clone(), window, Some(digest)).unwrap();
            let mut got = Vec::new();
            got.push(r.u8().unwrap());
            got.extend_from_slice(&r.bytes(9000).unwrap());
            r.skip(3).unwrap();
            got.extend_from_slice(&raw[9001..9004]);
            while !r.is_empty() {
                let n = (r.remaining() as usize).min(777);
                got.extend_from_slice(&r.bytes(n).unwrap());
            }
            assert_eq!(got, raw);
            r.finish().unwrap();
        }
        let mut r = reader(data.clone(), info.clone(), 8192, Some(digest ^ 1)).unwrap();
        r.skip(raw.len() as u64).unwrap();
        assert!(r.finish().unwrap_err().to_string().contains("Digest"));
    }

    #[test]
    fn corruption_and_truncation_are_errors() {
        let raw = sample();
        let (data, info) = compressed_file(&raw, 4096, true);
        let mut bad = data.clone();
        bad[5000] ^= 0x10;
        let mut r = reader(bad, info.clone(), 8192, None).unwrap();
        let e = r.skip(raw.len() as u64).unwrap_err().to_string();
        assert!(e.contains("checksum mismatch"), "{e}");
        let mut r = reader(data[..data.len() - 100].to_vec(), info.clone(), 8192, None).unwrap();
        assert!(r.skip(raw.len() as u64).is_err(), "truncated last chunk");
        let mut r = reader(data.clone(), info, 8192, None).unwrap();
        assert!(r.bytes(raw.len() + 1).is_err(), "a length past the end allocates nothing");
    }

    #[test]
    fn plain_files_check_crc_db() {
        let raw = sample();
        let mut crc = (65536u32).to_be_bytes().to_vec();
        for c in raw.chunks(65536) {
            crc.extend_from_slice(&crc32fast::hash(c).to_be_bytes());
        }
        let digest = crc32fast::hash(&raw);
        let mut r = DataReader::plain("t".into(), Arc::new(Bytes::from(raw.clone())), Some(Arc::new(Bytes::from(crc.clone()))), 1 << 16, Some(digest)).unwrap();
        assert_eq!(&r.bytes(raw.len()).unwrap()[..], &raw[..]);
        r.finish().unwrap();
        let mut bad = raw.clone();
        bad[70_000] ^= 1;
        let mut r = DataReader::plain("t".into(), Arc::new(Bytes::from(bad)), Some(Arc::new(Bytes::from(crc))), 1 << 16, None).unwrap();
        assert!(r.skip(raw.len() as u64).unwrap_err().to_string().contains("CRC.db"));
    }
}
