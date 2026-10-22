//! The WAL's on-disk format: a file header, then records, each with its own CRC.
//!
//! ```text
//! file header (32 bytes, little endian):
//!   magic "RSWAL\0\0\x01" | u64 file seq | u64 first lsn | u64 created unix ms
//! record:
//!   u32 body length | u32 crc32c(length bytes ++ body) | body
//! body:
//!   u64 unix ms | i64 seq_no | u32 shard | u16 cluster length | u16 index uuid length |
//!   cluster | index uuid | payload (opaque operation bytes from search-index)
//! ```
//!
//! Files are always freshly created and only appended to, so whatever follows the last record
//! that checks out is a torn tail from a crash in the middle of a group commit: readers stop
//! there and recovery truncates it. The CRC covers the length too, so a length damaged into a
//! plausible value can't make a reader swallow the next records as one body. Readers bound every
//! length by the bytes actually present (or by [`MAX_RECORD`] when streaming) before allocating.

use std::io::{self, Read};

pub const MAGIC: [u8; 8] = *b"RSWAL\0\0\x01";
pub const FILE_HEADER: usize = 32;
pub const RECORD_HEADER: usize = 8;
const BODY_FIXED: usize = 8 + 8 + 4 + 2 + 2;
/// Largest record body. Elasticsearch caps a request at `http.max_content_length` (100 MB);
/// an operation is at most that plus framing.
pub const MAX_RECORD: usize = 256 << 20;

/// One operation read back from the WAL. Borrowed from the reader's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalRecord<'a> {
    /// When it was appended (Unix ms). Non-decreasing in log order on a node.
    pub at_ms: u64,
    pub seq_no: i64,
    pub cluster: &'a str,
    pub index_uuid: &'a str,
    pub shard: u32,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileHeader {
    pub seq: u64,
    pub first_lsn: u64,
    pub created_ms: u64,
}

impl FileHeader {
    pub fn encode(&self) -> [u8; FILE_HEADER] {
        let mut h = [0u8; FILE_HEADER];
        h[..8].copy_from_slice(&MAGIC);
        h[8..16].copy_from_slice(&self.seq.to_le_bytes());
        h[16..24].copy_from_slice(&self.first_lsn.to_le_bytes());
        h[24..32].copy_from_slice(&self.created_ms.to_le_bytes());
        h
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < FILE_HEADER || b[..8] != MAGIC {
            return None;
        }
        let u = |r: std::ops::Range<usize>| u64::from_le_bytes(b[r].try_into().unwrap());
        Some(Self { seq: u(8..16), first_lsn: u(16..24), created_ms: u(24..32) })
    }
}

/// Size of a record with these fields on disk.
pub fn encoded_len(cluster: &str, index_uuid: &str, payload: &[u8]) -> usize {
    RECORD_HEADER + BODY_FIXED + cluster.len() + index_uuid.len() + payload.len()
}

/// Append one record to `buf`. The caller has validated the lengths (see [`check_lengths`]).
#[inline]
pub fn encode(buf: &mut Vec<u8>, at_ms: u64, seq_no: i64, cluster: &str, index_uuid: &str, shard: u32, payload: &[u8]) {
    let body = BODY_FIXED + cluster.len() + index_uuid.len() + payload.len();
    let start = buf.len();
    buf.reserve(RECORD_HEADER + body);
    buf.extend_from_slice(&(body as u32).to_le_bytes());
    buf.extend_from_slice(&[0; 4]);
    buf.extend_from_slice(&at_ms.to_le_bytes());
    buf.extend_from_slice(&seq_no.to_le_bytes());
    buf.extend_from_slice(&shard.to_le_bytes());
    buf.extend_from_slice(&(cluster.len() as u16).to_le_bytes());
    buf.extend_from_slice(&(index_uuid.len() as u16).to_le_bytes());
    buf.extend_from_slice(cluster.as_bytes());
    buf.extend_from_slice(index_uuid.as_bytes());
    buf.extend_from_slice(payload);
    let crc = crc(&buf[start..start + 4], &buf[start + RECORD_HEADER..]);
    buf[start + 4..start + 8].copy_from_slice(&crc.to_le_bytes());
}

pub fn check_lengths(cluster: &str, index_uuid: &str, payload: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(cluster.len() <= u16::MAX as usize, "cluster name too long");
    anyhow::ensure!(index_uuid.len() <= u16::MAX as usize, "index uuid too long");
    let body = BODY_FIXED + cluster.len() + index_uuid.len() + payload.len();
    anyhow::ensure!(body <= MAX_RECORD, "operation of {} bytes exceeds the WAL record limit of {MAX_RECORD} bytes", payload.len());
    Ok(())
}

#[inline]
fn crc(len: &[u8], body: &[u8]) -> u32 {
    crc32c::crc32c_append(crc32c::crc32c(len), body)
}

/// Parse a body whose CRC already matched.
fn parse_body(body: &[u8]) -> Option<WalRecord<'_>> {
    if body.len() < BODY_FIXED {
        return None;
    }
    let at_ms = u64::from_le_bytes(body[0..8].try_into().unwrap());
    let seq_no = i64::from_le_bytes(body[8..16].try_into().unwrap());
    let shard = u32::from_le_bytes(body[16..20].try_into().unwrap());
    let clen = u16::from_le_bytes(body[20..22].try_into().unwrap()) as usize;
    let ulen = u16::from_le_bytes(body[22..24].try_into().unwrap()) as usize;
    let rest = &body[BODY_FIXED..];
    if rest.len() < clen + ulen {
        return None;
    }
    let cluster = std::str::from_utf8(&rest[..clen]).ok()?;
    let index_uuid = std::str::from_utf8(&rest[clen..clen + ulen]).ok()?;
    Some(WalRecord { at_ms, seq_no, cluster, index_uuid, shard, payload: &rest[clen + ulen..] })
}

/// Iterates the records of a file held in memory (header excluded), stopping at the first one
/// that doesn't check out. [`Records::valid_len`] then says how much of the input is intact.
pub struct Records<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Records<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Bytes of intact records consumed so far.
    pub fn valid_len(&self) -> usize {
        self.pos
    }
}

impl<'a> Iterator for Records<'a> {
    type Item = WalRecord<'a>;

    fn next(&mut self) -> Option<WalRecord<'a>> {
        let rest = &self.data[self.pos..];
        if rest.len() < RECORD_HEADER {
            return None;
        }
        let len = u32::from_le_bytes(rest[0..4].try_into().unwrap()) as usize;
        let want = u32::from_le_bytes(rest[4..8].try_into().unwrap());
        if len < BODY_FIXED || len > rest.len() - RECORD_HEADER {
            return None;
        }
        let body = &rest[RECORD_HEADER..RECORD_HEADER + len];
        if crc(&rest[0..4], body) != want {
            return None;
        }
        let rec = parse_body(body)?;
        self.pos += RECORD_HEADER + len;
        Some(rec)
    }
}

/// Reads records from a stream (a zstd decoder over a downloaded object, or a file), reusing one
/// buffer. Stops at end of input or at a torn/corrupt record.
pub struct StreamRecords<R> {
    inner: R,
    buf: Vec<u8>,
    valid: u64,
    torn: bool,
}

impl<R: Read> StreamRecords<R> {
    /// Reads and checks the file header first.
    pub fn open(mut inner: R) -> io::Result<(FileHeader, Self)> {
        let mut h = [0u8; FILE_HEADER];
        inner.read_exact(&mut h)?;
        let header = FileHeader::decode(&h).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not a WAL file"))?;
        Ok((header, Self { inner, buf: Vec::new(), valid: FILE_HEADER as u64, torn: false }))
    }

    /// Bytes of the input (header included) up to the end of the last intact record.
    pub fn valid_len(&self) -> u64 {
        self.valid
    }

    /// Whether reading stopped at a damaged record rather than at a clean end.
    pub fn torn(&self) -> bool {
        self.torn
    }

    /// The next record, `None` at the end (clean or torn).
    pub fn next_record(&mut self) -> io::Result<Option<WalRecord<'_>>> {
        let mut h = [0u8; RECORD_HEADER];
        match read_full(&mut self.inner, &mut h)? {
            0 => return Ok(None),
            n if n < RECORD_HEADER => {
                self.torn = true;
                return Ok(None);
            }
            _ => {}
        }
        let len = u32::from_le_bytes(h[0..4].try_into().unwrap()) as usize;
        let want = u32::from_le_bytes(h[4..8].try_into().unwrap());
        if !(BODY_FIXED..=MAX_RECORD).contains(&len) {
            self.torn = true;
            return Ok(None);
        }
        // Grow with the data actually read, never to a length the input merely claims.
        self.buf.clear();
        let got = (&mut self.inner).take(len as u64).read_to_end(&mut self.buf)?;
        if got < len || crc(&h[0..4], &self.buf) != want {
            self.torn = true;
            return Ok(None);
        }
        match parse_body(&self.buf) {
            Some(r) => {
                self.valid += (RECORD_HEADER + len) as u64;
                Ok(Some(r))
            }
            None => {
                self.torn = true;
                Ok(None)
            }
        }
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut buf = Vec::new();
        encode(&mut buf, 10, 0, "c1", "u1", 0, b"first");
        encode(&mut buf, 11, 1, "c2", "uuid-2", 7, b"");
        encode(&mut buf, 12, -1, "c1", "u1", 0, &[0xff; 1000]);
        buf
    }

    #[test]
    fn round_trip() {
        let buf = sample();
        let recs: Vec<_> = Records::new(&buf).collect();
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0], WalRecord { at_ms: 10, seq_no: 0, cluster: "c1", index_uuid: "u1", shard: 0, payload: b"first" });
        assert_eq!(recs[1], WalRecord { at_ms: 11, seq_no: 1, cluster: "c2", index_uuid: "uuid-2", shard: 7, payload: b"" });
        assert_eq!(recs[2].payload.len(), 1000);
        assert_eq!(recs[2].seq_no, -1);
        assert_eq!(encoded_len("c1", "u1", b"first"), 8 + 24 + 4 + 5);
    }

    #[test]
    fn every_truncation_yields_a_prefix() {
        let buf = sample();
        let ends: Vec<usize> = {
            let mut it = Records::new(&buf);
            let mut v = vec![0];
            while it.next().is_some() {
                v.push(it.valid_len());
            }
            v
        };
        for cut in 0..=buf.len() {
            let mut it = Records::new(&buf[..cut]);
            let n = it.by_ref().count();
            let expect = ends.iter().rposition(|&e| e <= cut).unwrap();
            assert_eq!(n, expect, "cut at {cut}");
            assert_eq!(it.valid_len(), ends[expect]);
        }
    }

    #[test]
    fn any_flipped_byte_stops_the_reader_there() {
        let buf = sample();
        let first_len = encoded_len("c1", "u1", b"first");
        for i in 0..first_len {
            let mut b = buf.clone();
            b[i] ^= 0x40;
            assert_eq!(Records::new(&b).count(), 0, "flip at {i}");
        }
        let mut b = buf.clone();
        b[first_len + 10] ^= 1;
        assert_eq!(Records::new(&b).count(), 1);
    }

    #[test]
    fn streaming_reader_matches_and_bounds_lengths() {
        let mut file = FileHeader { seq: 3, first_lsn: 100, created_ms: 5 }.encode().to_vec();
        file.extend(sample());
        let (h, mut r) = StreamRecords::open(&file[..]).unwrap();
        assert_eq!(h, FileHeader { seq: 3, first_lsn: 100, created_ms: 5 });
        let mut n = 0;
        while let Some(rec) = r.next_record().unwrap() {
            assert!(rec.cluster.starts_with('c'));
            n += 1;
        }
        assert_eq!(n, 3);
        assert!(!r.torn());
        assert_eq!(r.valid_len(), file.len() as u64);

        // A length claiming far more than is present: no huge allocation, just a torn tail.
        let mut bad = FileHeader { seq: 1, first_lsn: 1, created_ms: 0 }.encode().to_vec();
        bad.extend_from_slice(&(MAX_RECORD as u32).to_le_bytes());
        bad.extend_from_slice(&[0; 4]);
        bad.extend_from_slice(&[1; 100]);
        let (_, mut r) = StreamRecords::open(&bad[..]).unwrap();
        assert!(r.next_record().unwrap().is_none());
        assert!(r.torn());
        assert_eq!(r.valid_len(), FILE_HEADER as u64);
        assert!(r.buf.capacity() < 1 << 20);

        assert!(StreamRecords::open(&b"RSWAL"[..]).is_err());
        assert!(StreamRecords::open(&[0u8; 40][..]).is_err());
    }

    #[test]
    fn lengths_are_checked() {
        assert!(check_lengths("c", "u", b"x").is_ok());
        assert!(check_lengths(&"c".repeat(70_000), "u", b"x").is_err());
        assert!(check_lengths("c", "u", &vec![0; MAX_RECORD]).is_err());
    }
}
