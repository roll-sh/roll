//! Reply decoding (RESP2 and RESP3), for clients, replication links and tests.

use crate::Reply;
use bytes::Bytes;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid reply: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

type R<T> = Result<Option<T>, DecodeError>;

/// Decode one reply from the front of `buf`. Returns the reply and the bytes consumed, or
/// `Ok(None)` when the buffer holds only part of it. RESP3 attributes are skipped.
pub fn decode(buf: &[u8]) -> R<(Reply, usize)> {
    let mut p = 0;
    Ok(value(buf, &mut p)?.map(|r| (r, p)))
}

fn line<'a>(buf: &'a [u8], p: &mut usize) -> R<&'a [u8]> {
    match memchr::memchr(b'\r', &buf[*p..]) {
        Some(i) if *p + i + 1 < buf.len() => {
            let s = &buf[*p..*p + i];
            *p += i + 2;
            Ok(Some(s))
        }
        _ => Ok(None),
    }
}

fn int(s: &[u8]) -> Result<i64, DecodeError> {
    std::str::from_utf8(s)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| DecodeError(format!("bad integer {:?}", String::from_utf8_lossy(s))))
}

fn text(s: &[u8]) -> String {
    String::from_utf8_lossy(s).into_owned()
}

fn blob(buf: &[u8], p: &mut usize, len: i64) -> R<Bytes> {
    let len = len as usize;
    if buf.len() < *p + len + 2 {
        return Ok(None);
    }
    let b = Bytes::copy_from_slice(&buf[*p..*p + len]);
    *p += len + 2;
    Ok(Some(b))
}

fn items(buf: &[u8], p: &mut usize, n: i64) -> R<Vec<Reply>> {
    let mut out = Vec::with_capacity((n as usize).min(4096));
    for _ in 0..n {
        match value(buf, p)? {
            Some(v) => out.push(v),
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

macro_rules! need {
    ($e:expr) => {
        match $e? {
            Some(v) => v,
            None => return Ok(None),
        }
    };
}

fn value(buf: &[u8], p: &mut usize) -> R<Reply> {
    if *p >= buf.len() {
        return Ok(None);
    }
    let kind = buf[*p];
    *p += 1;
    let l = need!(line(buf, p));
    Ok(Some(match kind {
        b'+' => {
            if l == b"OK" {
                Reply::Ok
            } else {
                Reply::Status(text(l))
            }
        }
        b'-' => Reply::Error(text(l)),
        b':' => Reply::Int(int(l)?),
        b'$' => match int(l)? {
            -1 => Reply::Nil,
            n if n < 0 => return Err(DecodeError("negative bulk length".into())),
            n => Reply::Bulk(need!(blob(buf, p, n))),
        },
        b'!' => {
            let n = int(l)?;
            Reply::Error(text(&need!(blob(buf, p, n))))
        }
        b'=' => {
            let n = int(l)?;
            let b = need!(blob(buf, p, n));
            if b.len() < 4 {
                return Err(DecodeError("short verbatim string".into()));
            }
            let fmt = match &b[..3] {
                b"mkd" => "mkd",
                _ => "txt",
            };
            Reply::Verbatim(fmt, b.slice(4..))
        }
        b'*' => match int(l)? {
            -1 => Reply::NilArray,
            n => Reply::Array(need!(items(buf, p, n))),
        },
        b'~' => Reply::Set(need!(items(buf, p, int(l)?))),
        b'>' => Reply::Push(need!(items(buf, p, int(l)?))),
        b'%' => {
            let flat = need!(items(buf, p, 2 * int(l)?));
            let mut it = flat.into_iter();
            let mut pairs = Vec::new();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                pairs.push((k, v));
            }
            Reply::Map(pairs)
        }
        b'|' => {
            // An attribute map precedes the real reply; drop it.
            need!(items(buf, p, 2 * int(l)?));
            return value(buf, p);
        }
        b'_' => Reply::Nil,
        b'#' => Reply::Bool(l == b"t"),
        b',' => Reply::Double(match l {
            b"inf" => f64::INFINITY,
            b"-inf" => f64::NEG_INFINITY,
            b"nan" => f64::NAN,
            _ => text(l).parse().map_err(|_| DecodeError("bad double".into()))?,
        }),
        b'(' => Reply::BigNum(text(l)),
        other => return Err(DecodeError(format!("unknown type byte {:?}", other as char))),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_and_nested() {
        let b = b"*2\r\n$3\r\nfoo\r\n*1\r\n:5\r\n";
        for cut in 0..b.len() {
            assert_eq!(decode(&b[..cut]).unwrap(), None);
        }
        let (r, n) = decode(b).unwrap().unwrap();
        assert_eq!(n, b.len());
        assert_eq!(r, Reply::Array(vec![Reply::bulk("foo"), Reply::Array(vec![Reply::Int(5)])]));
    }

    #[test]
    fn resp2_nulls_and_attributes() {
        assert_eq!(decode(b"$-1\r\n").unwrap().unwrap().0, Reply::Nil);
        assert_eq!(decode(b"*-1\r\n").unwrap().unwrap().0, Reply::NilArray);
        assert_eq!(decode(b"|1\r\n+a\r\n:1\r\n:2\r\n").unwrap().unwrap().0, Reply::Int(2));
    }
}
