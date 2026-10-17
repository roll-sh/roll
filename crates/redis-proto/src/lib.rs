//! RESP2 / RESP3, the Redis wire protocol.
//!
//! - [`parse_request`] reads client commands (multibulk or inline) out of a read buffer without
//!   copying the arguments.
//! - [`Reply`] is everything a server can answer; [`Reply::encode`] writes it for either protocol
//!   version, applying the RESP2 fallbacks (maps become flat arrays, doubles become bulk strings…).
//! - [`decode`] parses replies, for clients, replication and tests.

mod decode;
mod double;
mod request;

pub use decode::{DecodeError, decode};
pub use double::{fmt_double, fmt_long_double, parse_double};
pub use request::{BIG_ARG, Limits, ProtoError, parse_request, pending_big_arg, split_args};

use bytes::{BufMut, Bytes, BytesMut};

/// A reply. Errors carry their full text including the code (`"ERR …"`, `"WRONGTYPE …"`).
#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    /// `+OK`
    Ok,
    /// A status line, e.g. `+PONG` or `+QUEUED`.
    Status(String),
    Error(String),
    Int(i64),
    Bulk(Bytes),
    /// Null bulk string (`$-1` in RESP2, `_` in RESP3).
    Nil,
    /// Null array (`*-1` in RESP2, `_` in RESP3).
    NilArray,
    Array(Vec<Reply>),
    /// Encoded as a flat array in RESP2.
    Map(Vec<(Reply, Reply)>),
    /// Encoded as an array in RESP2.
    Set(Vec<Reply>),
    /// A bulk string in RESP2.
    Double(f64),
    /// `:1` / `:0` in RESP2.
    Bool(bool),
    /// A bulk string in RESP2.
    BigNum(String),
    /// Verbatim string with a three-letter format (`txt`, `mkd`); a bulk string in RESP2.
    Verbatim(&'static str, Bytes),
    /// Out-of-band push (RESP3); an array in RESP2 (pub/sub messages).
    Push(Vec<Reply>),
    /// Already-encoded bytes, written as is.
    Raw(Bytes),
    /// Several replies written back to back (SUBSCRIBE a b answers once per channel).
    Seq(Vec<Reply>),
}

impl Reply {
    pub fn error(msg: impl Into<String>) -> Self {
        Reply::Error(msg.into())
    }
    pub fn status(s: impl Into<String>) -> Self {
        Reply::Status(s.into())
    }
    pub fn bulk(b: impl AsRef<[u8]>) -> Self {
        Reply::Bulk(Bytes::copy_from_slice(b.as_ref()))
    }
    pub fn bulk_str(s: impl Into<String>) -> Self {
        Reply::Bulk(Bytes::from(s.into()))
    }
    pub fn opt_bulk(b: Option<impl AsRef<[u8]>>) -> Self {
        b.map_or(Reply::Nil, Reply::bulk)
    }
    pub fn bulks<I: IntoIterator<Item = T>, T: AsRef<[u8]>>(items: I) -> Self {
        Reply::Array(items.into_iter().map(Reply::bulk).collect())
    }
    pub fn is_error(&self) -> bool {
        matches!(self, Reply::Error(_))
    }

    /// Encode for a client speaking `resp` (2 or 3).
    pub fn encode(&self, resp: u8, out: &mut BytesMut) {
        let v3 = resp >= 3;
        match self {
            Reply::Ok => out.put_slice(b"+OK\r\n"),
            Reply::Status(s) => line(out, b'+', s.as_bytes()),
            Reply::Error(e) => line(out, b'-', e.as_bytes()),
            Reply::Int(i) => int(out, b':', *i),
            Reply::Bulk(b) => bulk(out, b),
            Reply::Nil if v3 => out.put_slice(b"_\r\n"),
            Reply::Nil => out.put_slice(b"$-1\r\n"),
            Reply::NilArray if v3 => out.put_slice(b"_\r\n"),
            Reply::NilArray => out.put_slice(b"*-1\r\n"),
            Reply::Array(items) => {
                int(out, b'*', items.len() as i64);
                for r in items {
                    r.encode(resp, out);
                }
            }
            Reply::Map(pairs) => {
                if v3 {
                    int(out, b'%', pairs.len() as i64);
                } else {
                    int(out, b'*', 2 * pairs.len() as i64);
                }
                for (k, v) in pairs {
                    k.encode(resp, out);
                    v.encode(resp, out);
                }
            }
            Reply::Set(items) => {
                int(out, if v3 { b'~' } else { b'*' }, items.len() as i64);
                for r in items {
                    r.encode(resp, out);
                }
            }
            Reply::Push(items) => {
                int(out, if v3 { b'>' } else { b'*' }, items.len() as i64);
                for r in items {
                    r.encode(resp, out);
                }
            }
            Reply::Double(d) => {
                let s = fmt_double(*d);
                if v3 {
                    // RESP3 spells infinities `inf` / `-inf` like RESP2 does.
                    line(out, b',', s.as_bytes());
                } else {
                    bulk(out, s.as_bytes());
                }
            }
            Reply::Bool(b) if v3 => out.put_slice(if *b { b"#t\r\n" } else { b"#f\r\n" }),
            Reply::Bool(b) => out.put_slice(if *b { b":1\r\n" } else { b":0\r\n" }),
            Reply::BigNum(n) if v3 => line(out, b'(', n.as_bytes()),
            Reply::BigNum(n) => bulk(out, n.as_bytes()),
            Reply::Verbatim(fmt, b) if v3 => {
                int(out, b'=', b.len() as i64 + 4);
                out.put_slice(fmt.as_bytes());
                out.put_u8(b':');
                out.put_slice(b);
                out.put_slice(b"\r\n");
            }
            Reply::Verbatim(_, b) => bulk(out, b),
            Reply::Raw(b) => out.put_slice(b),
            Reply::Seq(items) => {
                for r in items {
                    r.encode(resp, out);
                }
            }
        }
    }

    pub fn to_bytes(&self, resp: u8) -> Bytes {
        let mut out = BytesMut::new();
        self.encode(resp, &mut out);
        out.freeze()
    }
}

impl From<i64> for Reply {
    fn from(i: i64) -> Self {
        Reply::Int(i)
    }
}
impl From<usize> for Reply {
    fn from(i: usize) -> Self {
        Reply::Int(i as i64)
    }
}
impl From<bool> for Reply {
    fn from(b: bool) -> Self {
        Reply::Int(b as i64)
    }
}

fn line(out: &mut BytesMut, prefix: u8, s: &[u8]) {
    out.reserve(s.len() + 3);
    out.put_u8(prefix);
    // Status and error lines can't contain CR/LF; replace them like Redis does.
    if memchr::memchr2(b'\r', b'\n', s).is_some() {
        out.extend(s.iter().map(|&c| if c == b'\r' || c == b'\n' { b' ' } else { c }));
    } else {
        out.put_slice(s);
    }
    out.put_slice(b"\r\n");
}

fn int(out: &mut BytesMut, prefix: u8, i: i64) {
    let mut buf = itoa::Buffer::new();
    let s = buf.format(i);
    out.reserve(s.len() + 3);
    out.put_u8(prefix);
    out.put_slice(s.as_bytes());
    out.put_slice(b"\r\n");
}

/// Write a bulk string.
pub fn bulk(out: &mut BytesMut, b: &[u8]) {
    int(out, b'$', b.len() as i64);
    out.reserve(b.len() + 2);
    out.put_slice(b);
    out.put_slice(b"\r\n");
}

/// Encode a command as a multibulk request (`*N` of bulk strings).
/// Length of `args` encoded as a RESP command (what [`encode_command`] writes).
pub fn command_len<T: AsRef<[u8]>>(args: &[T]) -> usize {
    let digits = |n: usize| n.checked_ilog10().unwrap_or(0) as usize + 1;
    3 + digits(args.len()) + args.iter().map(|a| a.as_ref().len()).map(|n| 5 + digits(n) + n).sum::<usize>()
}

pub fn encode_command<I, T>(args: I, out: &mut BytesMut)
where
    I: IntoIterator<Item = T>,
    I::IntoIter: ExactSizeIterator,
    T: AsRef<[u8]>,
{
    let it = args.into_iter();
    int(out, b'*', it.len() as i64);
    for a in it {
        bulk(out, a.as_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(r: &Reply, resp: u8) -> String {
        String::from_utf8(r.to_bytes(resp).to_vec()).unwrap()
    }

    #[test]
    fn resp2_fallbacks() {
        assert_eq!(enc(&Reply::Nil, 2), "$-1\r\n");
        assert_eq!(enc(&Reply::NilArray, 2), "*-1\r\n");
        assert_eq!(enc(&Reply::Nil, 3), "_\r\n");
        assert_eq!(enc(&Reply::Bool(true), 2), ":1\r\n");
        assert_eq!(enc(&Reply::Bool(false), 3), "#f\r\n");
        assert_eq!(enc(&Reply::Double(1.5), 2), "$3\r\n1.5\r\n");
        assert_eq!(enc(&Reply::Double(1.5), 3), ",1.5\r\n");
        let m = Reply::Map(vec![(Reply::bulk("a"), Reply::Int(1))]);
        assert_eq!(enc(&m, 2), "*2\r\n$1\r\na\r\n:1\r\n");
        assert_eq!(enc(&m, 3), "%1\r\n$1\r\na\r\n:1\r\n");
        assert_eq!(enc(&Reply::Verbatim("txt", Bytes::from_static(b"hi")), 3), "=6\r\ntxt:hi\r\n");
        assert_eq!(enc(&Reply::Error("ERR a\r\nb".into()), 2), "-ERR a  b\r\n");
    }

    #[test]
    fn roundtrip_through_decoder() {
        let r = Reply::Array(vec![
            Reply::Ok,
            Reply::Int(-7),
            Reply::bulk("x"),
            Reply::Nil,
            Reply::Map(vec![(Reply::bulk("k"), Reply::Double(0.25))]),
            Reply::Set(vec![Reply::Bool(true)]),
            Reply::BigNum("123456789012345678901234567890".into()),
            Reply::Push(vec![Reply::bulk("message")]),
        ]);
        let bytes = r.to_bytes(3);
        let (back, n) = decode(&bytes).unwrap().unwrap();
        assert_eq!(n, bytes.len());
        assert_eq!(back, r);
    }
}

#[cfg(test)]
mod command_len_tests {
    #[test]
    fn matches_encoding() {
        for args in [vec![], vec!["get"], vec!["set", "k", "0123456789x"], vec!["a"; 12]] {
            let mut b = bytes::BytesMut::new();
            super::encode_command(&args, &mut b);
            assert_eq!(super::command_len(&args), b.len());
        }
    }
}
