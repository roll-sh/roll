//! The protocol's notation (spec section 3): a bounded reader over a message body and the
//! matching writers.
//!
//! [`Reader`] hands out `Bytes` slices of the body, so values and cells are never copied.
//! Every read checks the bytes actually present before it touches them, and every collection
//! is pre-sized by `min(count, remaining / smallest_element)`, never by the count alone: a
//! peer that claims 65535 strings in a 10-byte body gets a [`DecodeError::Truncated`], not a
//! large allocation.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use bytes::{BufMut, Bytes};

use crate::consistency::Consistency;
use crate::error::{DecodeError, Field};
use crate::version::Version;

/// A bound value in QUERY / EXECUTE / BATCH (`[value]`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Value {
    /// Serialised bytes (possibly empty, which is not null).
    Set(Bytes),
    /// Length -1. Before v4 every negative length is null.
    Null,
    /// Length -2 (v4+): leave the column unchanged.
    Unset,
}

impl Value {
    pub fn new(b: impl Into<Bytes>) -> Self {
        Value::Set(b.into())
    }

    pub fn as_bytes(&self) -> Option<&Bytes> {
        match self {
            Value::Set(b) => Some(b),
            _ => None,
        }
    }
}

impl From<Option<Bytes>> for Value {
    fn from(b: Option<Bytes>) -> Self {
        b.map_or(Value::Null, Value::Set)
    }
}

/// A cursor over one message body.
#[derive(Clone, Debug)]
pub struct Reader {
    buf: Bytes,
    pos: usize,
}

impl Reader {
    pub fn new(buf: Bytes) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Bytes read so far.
    pub fn offset(&self) -> usize {
        self.pos
    }

    fn need(&self, n: usize, field: Field) -> Result<(), DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::Truncated {
                field,
                need: n,
                have: self.remaining(),
                at: self.pos,
                len: self.buf.len(),
            });
        }
        Ok(())
    }

    fn take<const N: usize>(&mut self, field: Field) -> Result<[u8; N], DecodeError> {
        self.need(N, field)?;
        let mut out = [0u8; N];
        out.copy_from_slice(&self.buf[self.pos..self.pos + N]);
        self.pos += N;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take::<1>(Field::Byte)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.take(Field::Short)?))
    }

    pub fn i16(&mut self) -> Result<i16, DecodeError> {
        Ok(i16::from_be_bytes(self.take(Field::Short)?))
    }

    pub fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_be_bytes(self.take(Field::Int)?))
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.take(Field::Int)?))
    }

    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(i64::from_be_bytes(self.take(Field::Long)?))
    }

    pub fn uuid(&mut self) -> Result<[u8; 16], DecodeError> {
        self.take(Field::Uuid)
    }

    /// `n` raw bytes, as a slice of the body.
    pub fn raw(&mut self, n: usize, field: Field) -> Result<Bytes, DecodeError> {
        self.need(n, field)?;
        let b = self.buf.slice(self.pos..self.pos + n);
        self.pos += n;
        Ok(b)
    }

    /// Everything left.
    pub fn rest(&mut self) -> Bytes {
        let b = self.buf.slice(self.pos..);
        self.pos = self.buf.len();
        b
    }

    fn utf8(b: Bytes) -> Result<String, DecodeError> {
        match std::str::from_utf8(&b) {
            Ok(s) => Ok(s.to_owned()),
            Err(e) => Err(DecodeError::Utf8 {
                valid_up_to: e.valid_up_to(),
                error_len: e.error_len().unwrap_or(b.len() - e.valid_up_to()),
                bytes: b,
            }),
        }
    }

    /// `[string]`
    pub fn string(&mut self) -> Result<String, DecodeError> {
        let n = u16::from_be_bytes(self.take(Field::String)?) as usize;
        let b = self.raw(n, Field::String)?;
        Self::utf8(b)
    }

    /// `[long string]`. A negative length can never be satisfied (Cassandra fails it the
    /// same way as a short body).
    pub fn long_string(&mut self) -> Result<String, DecodeError> {
        let n = i32::from_be_bytes(self.take(Field::LongString)?);
        let b = self.raw(n as u32 as usize, Field::LongString)?;
        Self::utf8(b)
    }

    /// `[short bytes]`
    pub fn short_bytes(&mut self) -> Result<Bytes, DecodeError> {
        let n = u16::from_be_bytes(self.take(Field::ShortBytes)?) as usize;
        self.raw(n, Field::ShortBytes)
    }

    /// `[bytes]`: a negative length is null.
    pub fn bytes(&mut self) -> Result<Option<Bytes>, DecodeError> {
        let n = self.i32()?;
        if n < 0 {
            return Ok(None);
        }
        self.raw(n as usize, Field::Bytes).map(Some)
    }

    /// `[value]` as a bound variable: -1 null, -2 unset from v4, anything else negative is an
    /// error from v4 and null before.
    pub fn value(&mut self, version: Version) -> Result<Value, DecodeError> {
        let n = self.i32()?;
        if n >= 0 {
            return self.raw(n as usize, Field::Value).map(Value::Set);
        }
        if version < Version::V4 {
            return Ok(Value::Null);
        }
        match n {
            -1 => Ok(Value::Null),
            -2 => Ok(Value::Unset),
            _ => Err(DecodeError::InvalidValueLength(n)),
        }
    }

    /// `[short] n` followed by `n` values.
    pub fn value_list(&mut self, version: Version) -> Result<Vec<Value>, DecodeError> {
        let n = self.u16()? as usize;
        let mut out = Vec::with_capacity(n.min(self.remaining() / 4));
        for _ in 0..n {
            out.push(self.value(version)?);
        }
        Ok(out)
    }

    /// `[string list]`
    pub fn string_list(&mut self) -> Result<Vec<String>, DecodeError> {
        let n = self.u16()? as usize;
        let mut out = Vec::with_capacity(n.min(self.remaining() / 2));
        for _ in 0..n {
            out.push(self.string()?);
        }
        Ok(out)
    }

    /// `[string map]`, in wire order.
    pub fn string_map(&mut self) -> Result<Vec<(String, String)>, DecodeError> {
        let n = self.u16()? as usize;
        let mut out = Vec::with_capacity(n.min(self.remaining() / 4));
        for _ in 0..n {
            let k = self.string()?;
            out.push((k, self.string()?));
        }
        Ok(out)
    }

    /// `[string multimap]`, in wire order.
    pub fn string_multimap(&mut self) -> Result<Vec<(String, Vec<String>)>, DecodeError> {
        let n = self.u16()? as usize;
        let mut out = Vec::with_capacity(n.min(self.remaining() / 4));
        for _ in 0..n {
            let k = self.string()?;
            out.push((k, self.string_list()?));
        }
        Ok(out)
    }

    /// `[bytes map]`, in wire order.
    pub fn bytes_map(&mut self) -> Result<Vec<(String, Option<Bytes>)>, DecodeError> {
        let n = self.u16()? as usize;
        let mut out = Vec::with_capacity(n.min(self.remaining() / 6));
        for _ in 0..n {
            let k = self.string()?;
            out.push((k, self.bytes()?));
        }
        Ok(out)
    }

    /// `[inetaddr]`
    pub fn inetaddr(&mut self) -> Result<IpAddr, DecodeError> {
        let n = self.u8()?;
        match n {
            4 => Ok(IpAddr::V4(Ipv4Addr::from(self.take::<4>(Field::Inet)?))),
            16 => Ok(IpAddr::V6(Ipv6Addr::from(self.take::<16>(Field::Inet)?))),
            _ => Err(DecodeError::Invalid(format!("Invalid IP address length {n} while deserializing inet address"))),
        }
    }

    /// `[inet]`
    pub fn inet(&mut self) -> Result<SocketAddr, DecodeError> {
        let ip = self.inetaddr()?;
        let port = self.i32()?;
        Ok(SocketAddr::new(ip, port as u16))
    }

    /// `[consistency]`
    pub fn consistency(&mut self) -> Result<Consistency, DecodeError> {
        let c = self.u16()?;
        Consistency::from_code(c).ok_or(DecodeError::UnknownConsistency(c))
    }
}

/// Writers for the notation. Strings longer than their length prefix allows are cut at the
/// limit (on a character boundary) so the message stays well formed.
pub trait PutExt: BufMut {
    fn put_string(&mut self, s: &str) {
        let s = clip(s, u16::MAX as usize);
        self.put_u16(s.len() as u16);
        self.put_slice(s.as_bytes());
    }

    fn put_long_string(&mut self, s: &str) {
        let s = clip(s, i32::MAX as usize);
        self.put_i32(s.len() as i32);
        self.put_slice(s.as_bytes());
    }

    fn put_short_bytes(&mut self, b: &[u8]) {
        let b = &b[..b.len().min(u16::MAX as usize)];
        self.put_u16(b.len() as u16);
        self.put_slice(b);
    }

    fn put_cql_bytes(&mut self, b: Option<&[u8]>) {
        match b {
            Some(b) => {
                self.put_i32(b.len() as i32);
                self.put_slice(b);
            }
            None => self.put_i32(-1),
        }
    }

    fn put_value(&mut self, v: &Value) {
        match v {
            Value::Set(b) => self.put_cql_bytes(Some(b)),
            Value::Null => self.put_i32(-1),
            Value::Unset => self.put_i32(-2),
        }
    }

    fn put_string_list<S: AsRef<str>>(&mut self, l: &[S]) {
        self.put_u16(l.len().min(u16::MAX as usize) as u16);
        for s in l.iter().take(u16::MAX as usize) {
            self.put_string(s.as_ref());
        }
    }

    fn put_string_map<K: AsRef<str>, V: AsRef<str>>(&mut self, m: &[(K, V)]) {
        self.put_u16(m.len().min(u16::MAX as usize) as u16);
        for (k, v) in m.iter().take(u16::MAX as usize) {
            self.put_string(k.as_ref());
            self.put_string(v.as_ref());
        }
    }

    fn put_string_multimap<K: AsRef<str>, V: AsRef<str>>(&mut self, m: &[(K, Vec<V>)]) {
        self.put_u16(m.len().min(u16::MAX as usize) as u16);
        for (k, v) in m.iter().take(u16::MAX as usize) {
            self.put_string(k.as_ref());
            self.put_string_list(v);
        }
    }

    fn put_bytes_map(&mut self, m: &[(String, Option<Bytes>)]) {
        self.put_u16(m.len().min(u16::MAX as usize) as u16);
        for (k, v) in m.iter().take(u16::MAX as usize) {
            self.put_string(k);
            self.put_cql_bytes(v.as_deref());
        }
    }

    fn put_inetaddr(&mut self, ip: IpAddr) {
        match ip {
            IpAddr::V4(a) => {
                self.put_u8(4);
                self.put_slice(&a.octets());
            }
            IpAddr::V6(a) => {
                self.put_u8(16);
                self.put_slice(&a.octets());
            }
        }
    }

    fn put_inet(&mut self, a: SocketAddr) {
        self.put_inetaddr(a.ip());
        self.put_i32(a.port() as i32);
    }

    fn put_consistency(&mut self, c: Consistency) {
        self.put_u16(c.code());
    }
}

impl<B: BufMut + ?Sized> PutExt for B {}

fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;

    fn r(b: &[u8]) -> Reader {
        Reader::new(Bytes::copy_from_slice(b))
    }

    #[test]
    fn primitives_round_trip() {
        let mut b = BytesMut::new();
        b.put_u8(7);
        b.put_i16(-2);
        b.put_i32(-70000);
        b.put_i64(1 << 40);
        b.put_string("héllo");
        b.put_long_string("long");
        b.put_short_bytes(b"\x01\x02");
        b.put_cql_bytes(None);
        b.put_cql_bytes(Some(b""));
        b.put_string_list(&["a", "b"]);
        b.put_string_map(&[("k", "v")]);
        b.put_string_multimap(&[("k", vec!["v1", "v2"])]);
        b.put_bytes_map(&[("p".into(), Some(Bytes::from_static(b"x"))), ("q".into(), None)]);
        b.put_inet("10.1.2.3:9042".parse().unwrap());
        b.put_inet("[::1]:7".parse().unwrap());
        b.put_consistency(Consistency::LocalQuorum);
        let mut rd = Reader::new(b.freeze());
        assert_eq!(rd.u8().unwrap(), 7);
        assert_eq!(rd.i16().unwrap(), -2);
        assert_eq!(rd.i32().unwrap(), -70000);
        assert_eq!(rd.i64().unwrap(), 1 << 40);
        assert_eq!(rd.string().unwrap(), "héllo");
        assert_eq!(rd.long_string().unwrap(), "long");
        assert_eq!(&rd.short_bytes().unwrap()[..], b"\x01\x02");
        assert_eq!(rd.bytes().unwrap(), None);
        assert_eq!(rd.bytes().unwrap(), Some(Bytes::new()));
        assert_eq!(rd.string_list().unwrap(), vec!["a", "b"]);
        assert_eq!(rd.string_map().unwrap(), vec![("k".to_string(), "v".to_string())]);
        assert_eq!(rd.string_multimap().unwrap(), vec![("k".to_string(), vec!["v1".to_string(), "v2".to_string()])]);
        assert_eq!(rd.bytes_map().unwrap().len(), 2);
        assert_eq!(rd.inet().unwrap(), "10.1.2.3:9042".parse().unwrap());
        assert_eq!(rd.inet().unwrap(), "[::1]:7".parse().unwrap());
        assert_eq!(rd.consistency().unwrap(), Consistency::LocalQuorum);
        assert!(rd.is_empty());
    }

    #[test]
    fn values_by_version() {
        let mut b = BytesMut::new();
        b.put_i32(-1);
        b.put_i32(-2);
        b.put_i32(-3);
        let bytes = b.freeze();
        let mut v4 = Reader::new(bytes.clone());
        assert_eq!(v4.value(Version::V4).unwrap(), Value::Null);
        assert_eq!(v4.value(Version::V4).unwrap(), Value::Unset);
        assert_eq!(v4.value(Version::V4), Err(DecodeError::InvalidValueLength(-3)));
        let mut v3 = Reader::new(bytes);
        for _ in 0..3 {
            assert_eq!(v3.value(Version::V3).unwrap(), Value::Null);
        }
    }

    #[test]
    fn slices_share_the_body() {
        let body = Bytes::from_static(b"\x00\x00\x00\x03abc");
        let mut rd = Reader::new(body.clone());
        let v = rd.bytes().unwrap().unwrap();
        assert_eq!(v.as_ptr(), body[4..].as_ptr());
    }

    #[test]
    fn truncation_is_reported_not_panicked() {
        assert!(matches!(r(b"\x00").i32(), Err(DecodeError::Truncated { need: 4, have: 1, .. })));
        assert!(matches!(r(b"\x00\x05ab").string(), Err(DecodeError::Truncated { field: Field::String, need: 5, have: 2, .. })));
        assert!(matches!(r(b"\x7f\xff\xff\xff").long_string(), Err(DecodeError::Truncated { need: 0x7fff_ffff, have: 0, .. })));
        assert!(matches!(r(b"\x00\x00\x00\x09ab").bytes(), Err(DecodeError::Truncated { field: Field::Bytes, need: 9, have: 2, .. })));
        assert!(matches!(r(b"\x05").inetaddr(), Err(DecodeError::Invalid(_))));
        assert!(matches!(r(b"\x00\x63").consistency(), Err(DecodeError::UnknownConsistency(99))));
    }

    #[test]
    fn huge_counts_do_not_allocate() {
        // 65535 claimed elements, nothing present: fails on the first element.
        for f in [
            |r: &mut Reader| r.string_list().map(drop),
            |r: &mut Reader| r.string_map().map(drop),
            |r: &mut Reader| r.string_multimap().map(drop),
            |r: &mut Reader| r.bytes_map().map(drop),
            |r: &mut Reader| r.value_list(Version::V4).map(drop),
        ] {
            assert!(matches!(f(&mut r(b"\xff\xff")), Err(DecodeError::Truncated { .. })));
        }
    }

    #[test]
    fn bad_utf8() {
        let e = r(b"\x00\x02\xff\xfe").string().unwrap_err();
        assert!(matches!(e, DecodeError::Utf8 { valid_up_to: 0, error_len: 1, .. }));
    }

    #[test]
    fn long_strings_are_clipped_on_char_boundaries() {
        let s = "é".repeat(40000); // 80000 bytes
        let mut b = BytesMut::new();
        b.put_string(&s);
        let mut rd = Reader::new(b.freeze());
        let back = rd.string().unwrap();
        assert_eq!(back.len(), 65534);
        assert!(rd.is_empty());
    }
}
