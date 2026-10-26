//! The WAL record of one shard operation (Elasticsearch's translog `Operation`).
//!
//! search-es logs every operation a shard applied ([`LoggedOp`]: index, delete, or the no-op of
//! a seq_no a failed write used up) with the `_seq_no`, `_primary_term` and `_version` it got.
//! An index operation keeps the document as it was indexed (after ingest) and its routing:
//! replay parses it again against the index's mapping, which is durable before any operation
//! that needed it, so recovery rebuilds the shard's buffer exactly as acknowledged.
//!
//! ```text
//! magic 0xD1 | version 1 | kind u8 (1 index, 2 delete, 3 no-op) |
//! seq_no i64 | primary_term i64 | version i64 |
//! [index, delete] id_len u32 | id
//! [index]         routing_len u32 (u32::MAX: none) | routing | source (the rest)
//! ```
//!
//! All integers are little-endian. The source is the record's tail, so encoding hands it to the
//! WAL as a separate part ([`Header`]) and never copies a document into a scratch buffer.
//! Decoding borrows from the record and checks every length against the bytes actually present
//! before using it.

use anyhow::{bail, ensure};
use search_es::engine::LoggedOp;

const MAGIC: u8 = 0xD1;
const VERSION: u8 = 1;

const K_INDEX: u8 = 1;
const K_DELETE: u8 = 2;
const K_NOOP: u8 = 3;
const NO_ROUTING: u32 = u32::MAX;

/// The fixed part every record starts with.
const FIXED: usize = 3 + 8 * 3;

/// A record's framing (everything but an index operation's source), built on the stack.
pub struct Header {
    buf: [u8; FIXED + 4 + MAX_ID + 4 + MAX_ROUTING],
    len: usize,
}

/// Longest `_id` Elasticsearch accepts is 512 bytes; routing values are not limited by
/// Elasticsearch, so longer ones go to the heap ([`encode`]).
const MAX_ID: usize = 512;
const MAX_ROUTING: usize = 512;

impl Header {
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

fn fixed(kind: u8, seq_no: i64, primary_term: i64, version: i64, out: &mut [u8]) {
    out[0] = MAGIC;
    out[1] = VERSION;
    out[2] = kind;
    out[3..11].copy_from_slice(&seq_no.to_le_bytes());
    out[11..19].copy_from_slice(&primary_term.to_le_bytes());
    out[19..27].copy_from_slice(&version.to_le_bytes());
}

/// The framing of `op` and, for an index operation, its source (the record's tail). `None`
/// when the id or routing is too long for the stack header: use [`encode`].
pub fn header<'a>(op: &LoggedOp<'a>) -> Option<(Header, &'a [u8])> {
    let mut h = Header { buf: [0; FIXED + 4 + MAX_ID + 4 + MAX_ROUTING], len: FIXED };
    let tail: &[u8] = match *op {
        LoggedOp::Index { id, routing, source, seq_no, primary_term, version } => {
            if id.len() > MAX_ID || routing.is_some_and(|r| r.len() > MAX_ROUTING) {
                return None;
            }
            fixed(K_INDEX, seq_no, primary_term, version, &mut h.buf);
            h.push_str(id);
            match routing {
                Some(r) => h.push_str(r),
                None => {
                    h.buf[h.len..h.len + 4].copy_from_slice(&NO_ROUTING.to_le_bytes());
                    h.len += 4;
                }
            }
            source
        }
        LoggedOp::Delete { id, seq_no, primary_term, version } => {
            if id.len() > MAX_ID {
                return None;
            }
            fixed(K_DELETE, seq_no, primary_term, version, &mut h.buf);
            h.push_str(id);
            &[]
        }
        LoggedOp::NoOp { seq_no, primary_term } => {
            fixed(K_NOOP, seq_no, primary_term, 0, &mut h.buf);
            &[]
        }
    };
    Some((h, tail))
}

impl Header {
    fn push_str(&mut self, s: &str) {
        self.buf[self.len..self.len + 4].copy_from_slice(&(s.len() as u32).to_le_bytes());
        self.len += 4;
        self.buf[self.len..self.len + s.len()].copy_from_slice(s.as_bytes());
        self.len += s.len();
    }
}

/// The whole record in one buffer (tests, and ids or routings too long for [`header`]).
pub fn encode(op: &LoggedOp<'_>) -> Vec<u8> {
    let mut out = vec![0u8; FIXED];
    let put = |out: &mut Vec<u8>, s: &str| {
        out.extend_from_slice(&(s.len() as u32).to_le_bytes());
        out.extend_from_slice(s.as_bytes());
    };
    match *op {
        LoggedOp::Index { id, routing, source, seq_no, primary_term, version } => {
            fixed(K_INDEX, seq_no, primary_term, version, &mut out);
            put(&mut out, id);
            match routing {
                Some(r) => put(&mut out, r),
                None => out.extend_from_slice(&NO_ROUTING.to_le_bytes()),
            }
            out.extend_from_slice(source);
        }
        LoggedOp::Delete { id, seq_no, primary_term, version } => {
            fixed(K_DELETE, seq_no, primary_term, version, &mut out);
            put(&mut out, id);
        }
        LoggedOp::NoOp { seq_no, primary_term } => fixed(K_NOOP, seq_no, primary_term, 0, &mut out),
    }
    out
}

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> anyhow::Result<&'a [u8]> {
        ensure!(n <= self.b.len(), "truncated operation record");
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }

    fn u32(&mut self) -> anyhow::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }

    fn i64(&mut self) -> anyhow::Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn str(&mut self) -> anyhow::Result<&'a str> {
        let n = self.u32()? as usize;
        Ok(std::str::from_utf8(self.take(n)?)?)
    }
}

/// Decodes a record (borrowing from it).
pub fn decode(b: &[u8]) -> anyhow::Result<LoggedOp<'_>> {
    let mut r = Reader { b };
    let head = r.take(3)?;
    ensure!(head[0] == MAGIC, "not an operation record (magic {:#04x})", head[0]);
    ensure!(head[1] == VERSION, "operation record version {} is not supported", head[1]);
    let (seq_no, primary_term, version) = (r.i64()?, r.i64()?, r.i64()?);
    Ok(match head[2] {
        K_INDEX => {
            let id = r.str()?;
            let n = r.u32()?;
            let routing = if n == NO_ROUTING { None } else { Some(std::str::from_utf8(r.take(n as usize)?)?) };
            LoggedOp::Index { id, routing, source: r.b, seq_no, primary_term, version }
        }
        K_DELETE => {
            let id = r.str()?;
            ensure!(r.b.is_empty(), "trailing bytes after a delete record");
            LoggedOp::Delete { id, seq_no, primary_term, version }
        }
        K_NOOP => {
            ensure!(r.b.is_empty(), "trailing bytes after a no-op record");
            LoggedOp::NoOp { seq_no, primary_term }
        }
        k => bail!("unknown operation kind {k}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops<'a>(long: &'a str) -> Vec<LoggedOp<'a>> {
        vec![
            LoggedOp::Index { id: "1", routing: None, source: br#"{"a":1}"#, seq_no: 0, primary_term: 1, version: 1 },
            LoggedOp::Index { id: "é", routing: Some("r"), source: b"", seq_no: i64::MAX, primary_term: 7, version: -3 },
            LoggedOp::Index { id: long, routing: Some(long), source: b"{}", seq_no: 5, primary_term: 2, version: 9 },
            LoggedOp::Delete { id: "x", seq_no: 3, primary_term: 1, version: 2 },
            LoggedOp::Delete { id: long, seq_no: 4, primary_term: 1, version: 2 },
            LoggedOp::NoOp { seq_no: 12, primary_term: 3 },
        ]
    }

    #[test]
    fn round_trips() {
        let long = "z".repeat(2000);
        for op in ops(&long) {
            let whole = encode(&op);
            assert_eq!(decode(&whole).unwrap(), op);
            match header(&op) {
                Some((h, tail)) => {
                    let mut parts = h.as_bytes().to_vec();
                    parts.extend_from_slice(tail);
                    assert_eq!(parts, whole, "{op:?}");
                }
                None => assert!(whole.len() > 1000, "only long ids or routings need the heap"),
            }
        }
    }

    #[test]
    fn rejects_damage_without_overallocating() {
        let op = LoggedOp::Index { id: "abc", routing: Some("rr"), source: b"{}", seq_no: 1, primary_term: 1, version: 1 };
        let whole = encode(&op);
        for n in 0..FIXED + 4 {
            assert!(decode(&whole[..n]).is_err(), "prefix of {n} bytes");
        }
        // A length far past the record.
        let mut bad = whole.clone();
        bad[FIXED..FIXED + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode(&bad).is_err());
        let mut bad = whole.clone();
        bad[0] = 0xC7;
        assert!(decode(&bad).unwrap_err().to_string().contains("not an operation record"));
        let mut bad = whole.clone();
        bad[2] = 9;
        assert!(decode(&bad).is_err());
        let mut del = encode(&LoggedOp::Delete { id: "a", seq_no: 1, primary_term: 1, version: 1 });
        del.push(0);
        assert!(decode(&del).is_err());
    }
}
