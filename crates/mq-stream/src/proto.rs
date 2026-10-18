//! Stream protocol framing: command keys, response codes, and big-endian readers/writers.
//!
//! A frame is `size u32` followed by `key u16, version u16` and the command body. Strings are
//! `i16` length-prefixed, byte arrays `i32` length-prefixed; `-1` means null.

use bytes::{BufMut, BytesMut};

pub const DECLARE_PUBLISHER: u16 = 0x01;
pub const PUBLISH: u16 = 0x02;
pub const PUBLISH_CONFIRM: u16 = 0x03;
pub const PUBLISH_ERROR: u16 = 0x04;
pub const QUERY_PUBLISHER_SEQUENCE: u16 = 0x05;
pub const DELETE_PUBLISHER: u16 = 0x06;
pub const SUBSCRIBE: u16 = 0x07;
pub const DELIVER: u16 = 0x08;
pub const CREDIT: u16 = 0x09;
pub const STORE_OFFSET: u16 = 0x0a;
pub const QUERY_OFFSET: u16 = 0x0b;
pub const UNSUBSCRIBE: u16 = 0x0c;
pub const CREATE_STREAM: u16 = 0x0d;
pub const DELETE_STREAM: u16 = 0x0e;
pub const METADATA: u16 = 0x0f;
pub const METADATA_UPDATE: u16 = 0x10;
pub const PEER_PROPERTIES: u16 = 0x11;
pub const SASL_HANDSHAKE: u16 = 0x12;
pub const SASL_AUTHENTICATE: u16 = 0x13;
pub const TUNE: u16 = 0x14;
pub const OPEN: u16 = 0x15;
pub const CLOSE: u16 = 0x16;
pub const HEARTBEAT: u16 = 0x17;
pub const ROUTE: u16 = 0x18;
pub const PARTITIONS: u16 = 0x19;
pub const CONSUMER_UPDATE: u16 = 0x1a;
pub const EXCHANGE_COMMAND_VERSIONS: u16 = 0x1b;
pub const STREAM_STATS: u16 = 0x1c;
pub const CREATE_SUPER_STREAM: u16 = 0x1d;
pub const DELETE_SUPER_STREAM: u16 = 0x1e;
pub const RESOLVE_OFFSET_SPEC: u16 = 0x1f;
/// Set on the key of a response.
pub const RESPONSE: u16 = 0x8000;

pub const OK: u16 = 0x01;
pub const STREAM_DOES_NOT_EXIST: u16 = 0x02;
pub const SUBSCRIPTION_ID_ALREADY_EXISTS: u16 = 0x03;
pub const SUBSCRIPTION_ID_DOES_NOT_EXIST: u16 = 0x04;
pub const STREAM_ALREADY_EXISTS: u16 = 0x05;
pub const STREAM_NOT_AVAILABLE: u16 = 0x06;
pub const SASL_MECHANISM_NOT_SUPPORTED: u16 = 0x07;
pub const AUTHENTICATION_FAILURE: u16 = 0x08;
pub const SASL_ERROR: u16 = 0x09;
pub const AUTHENTICATION_FAILURE_LOOPBACK: u16 = 0x0b;
pub const VHOST_ACCESS_FAILURE: u16 = 0x0c;
pub const UNKNOWN_FRAME: u16 = 0x0d;
pub const FRAME_TOO_LARGE: u16 = 0x0e;
pub const INTERNAL_ERROR: u16 = 0x0f;
pub const ACCESS_REFUSED: u16 = 0x10;
pub const PRECONDITION_FAILED: u16 = 0x11;
pub const PUBLISHER_DOES_NOT_EXIST: u16 = 0x12;
pub const NO_OFFSET: u16 = 0x13;

pub const OFFSET_NONE: u16 = 0;
pub const OFFSET_FIRST: u16 = 1;
pub const OFFSET_LAST: u16 = 2;
pub const OFFSET_NEXT: u16 = 3;
pub const OFFSET_OFFSET: u16 = 4;
pub const OFFSET_TIMESTAMP: u16 = 5;

/// The commands and versions the server supports (RabbitMQ 4.x's list).
pub const COMMAND_VERSIONS: &[(u16, u16, u16)] = &[
    (DECLARE_PUBLISHER, 1, 1),
    (PUBLISH, 1, 2),
    (QUERY_PUBLISHER_SEQUENCE, 1, 1),
    (DELETE_PUBLISHER, 1, 1),
    (SUBSCRIBE, 1, 1),
    (CREDIT, 1, 1),
    (STORE_OFFSET, 1, 1),
    (QUERY_OFFSET, 1, 1),
    (UNSUBSCRIBE, 1, 1),
    (CREATE_STREAM, 1, 1),
    (DELETE_STREAM, 1, 1),
    (METADATA, 1, 1),
    (CLOSE, 1, 1),
    (HEARTBEAT, 1, 1),
    (ROUTE, 1, 1),
    (PARTITIONS, 1, 1),
    (STREAM_STATS, 1, 1),
    (CREATE_SUPER_STREAM, 1, 1),
    (DELETE_SUPER_STREAM, 1, 1),
    (RESOLVE_OFFSET_SPEC, 1, 1),
];

/// A frame body that ended early or carried an impossible length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("malformed stream protocol frame")
    }
}

impl std::error::Error for Malformed {}

pub type R<T> = Result<T, Malformed>;

/// Big-endian reader over a frame body. Never panics on short input.
pub struct Rd<'a> {
    pub buf: &'a [u8],
}

impl<'a> Rd<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Rd { buf }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    pub fn take(&mut self, n: usize) -> R<&'a [u8]> {
        if self.buf.len() < n {
            return Err(Malformed);
        }
        let (a, b) = self.buf.split_at(n);
        self.buf = b;
        Ok(a)
    }

    fn arr<const N: usize>(&mut self) -> R<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Malformed)
    }

    pub fn u8(&mut self) -> R<u8> {
        Ok(self.arr::<1>()?[0])
    }
    pub fn u16(&mut self) -> R<u16> {
        self.arr().map(u16::from_be_bytes)
    }
    pub fn i16(&mut self) -> R<i16> {
        self.arr().map(i16::from_be_bytes)
    }
    pub fn u32(&mut self) -> R<u32> {
        self.arr().map(u32::from_be_bytes)
    }
    pub fn i32(&mut self) -> R<i32> {
        self.arr().map(i32::from_be_bytes)
    }
    pub fn u64(&mut self) -> R<u64> {
        self.arr().map(u64::from_be_bytes)
    }
    pub fn i64(&mut self) -> R<i64> {
        self.arr().map(i64::from_be_bytes)
    }

    pub fn peek_u32(&self) -> R<u32> {
        let b: [u8; 4] = self.buf.get(..4).ok_or(Malformed)?.try_into().map_err(|_| Malformed)?;
        Ok(u32::from_be_bytes(b))
    }

    /// A nullable string's bytes.
    pub fn opt_str(&mut self) -> R<Option<&'a [u8]>> {
        match self.i16()? {
            -1 => Ok(None),
            n if n < 0 => Err(Malformed),
            n => self.take(n as usize).map(Some),
        }
    }

    /// A string (null reads as empty), lossily decoded.
    pub fn string(&mut self) -> R<String> {
        Ok(self.opt_str()?.map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default())
    }

    /// Nullable byte array.
    pub fn bytes(&mut self) -> R<Option<&'a [u8]>> {
        match self.i32()? {
            -1 => Ok(None),
            n if n < 0 => Err(Malformed),
            n => self.take(n as usize).map(Some),
        }
    }

    /// `[string]`.
    pub fn strings(&mut self) -> R<Vec<String>> {
        let n = self.count()?;
        (0..n).map(|_| self.string()).collect()
    }

    /// `[key value]` string map, in wire order.
    pub fn map(&mut self) -> R<Vec<(String, String)>> {
        let n = self.count()?;
        (0..n).map(|_| Ok((self.string()?, self.string()?))).collect()
    }

    /// An optional trailing map (absent when the client sent no properties).
    pub fn opt_map(&mut self) -> R<Vec<(String, String)>> {
        if self.remaining() < 4 { Ok(Vec::new()) } else { self.map() }
    }

    /// An array count, bounded by what's left so a hostile count can't drive allocations.
    pub fn count(&mut self) -> R<usize> {
        let n = self.i32()?;
        if n < 0 || n as usize > self.buf.len() {
            return Err(Malformed);
        }
        Ok(n as usize)
    }
}

/// Start a frame in `out`; returns the position to pass to [`finish`].
pub fn begin(out: &mut BytesMut, key: u16, version: u16) -> usize {
    let at = out.len();
    out.put_u32(0);
    out.put_u16(key);
    out.put_u16(version);
    at
}

/// Patch the frame size of a frame started at `at`.
pub fn finish(out: &mut BytesMut, at: usize) {
    let size = (out.len() - at - 4) as u32;
    out[at..at + 4].copy_from_slice(&size.to_be_bytes());
}

/// A response header: `key | 0x8000, version 1, correlation id, code`.
pub fn response(out: &mut BytesMut, key: u16, corr: u32, code: u16) -> usize {
    let at = begin(out, key | RESPONSE, 1);
    out.put_u32(corr);
    out.put_u16(code);
    at
}

pub fn put_str(out: &mut BytesMut, s: &[u8]) {
    let n = s.len().min(i16::MAX as usize);
    out.put_i16(n as i16);
    out.put_slice(&s[..n]);
}

pub fn put_map<'a>(out: &mut BytesMut, kv: impl ExactSizeIterator<Item = (&'a str, &'a str)>) {
    out.put_u32(kv.len() as u32);
    for (k, v) in kv {
        put_str(out, k.as_bytes());
        put_str(out, v.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_is_total() {
        let mut r = Rd::new(&[0, 3, b'a', b'b']);
        assert_eq!(r.opt_str(), Err(Malformed));
        let mut r = Rd::new(&[0xff, 0xff, 0, 0, 0, 1]);
        assert_eq!(r.opt_str(), Ok(None));
        assert_eq!(r.count(), Err(Malformed));
        let mut r = Rd::new(&[0, 0, 0, 1, 0, 1, b'k', 0, 1, b'v']);
        assert_eq!(r.map().unwrap(), vec![("k".to_string(), "v".to_string())]);
    }

    #[test]
    fn frames() {
        let mut b = BytesMut::new();
        let at = response(&mut b, OPEN, 7, OK);
        finish(&mut b, at);
        assert_eq!(&b[..], &[0, 0, 0, 10, 0x80, 0x15, 0, 1, 0, 0, 0, 7, 0, 1]);
    }
}
