//! Client request parsing: multibulk (`*N\r\n$len\r\n…`) and inline (`SET a b\n`) commands.

use bytes::{Bytes, BytesMut};

/// Longest inline command or multibulk/bulk header line.
const INLINE_MAX: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// `proto-max-bulk-len`.
    pub max_bulk: usize,
    /// Unauthenticated clients get tiny limits so they can't make the server buffer much.
    pub authenticated: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_bulk: 512 * 1024 * 1024, authenticated: true }
    }
}

/// A malformed request. The connection is closed after the error is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtoError(pub String);

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Protocol error: {}", self.0)
    }
}

impl std::error::Error for ProtoError {}

/// Arguments from this size on are read into a buffer of their own size (`PROTO_MBULK_BIG_ARG`).
pub const BIG_ARG: usize = 32 * 1024;

/// How Redis holds an incomplete multibulk command at the front of `buf`: the header lines and
/// arguments it has parsed already leave the query buffer (`skip` bytes), the arguments going
/// to argv (`argv`, their total length). Waiting on a big argument (`big`, its length), the
/// query buffer keeps only that argument, sized for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Partial {
    pub skip: usize,
    pub argv: usize,
    pub big: Option<usize>,
}

pub fn partial(buf: &[u8]) -> Partial {
    let mut p = Partial::default();
    if buf.first() != Some(&b'*') {
        return p;
    }
    let line = |from: usize| -> Option<(i64, usize)> {
        let end = from + memchr::memchr(b'\r', buf.get(from..)?)?;
        let n = std::str::from_utf8(&buf[from..end]).ok()?.parse().ok()?;
        (buf.get(end + 1) == Some(&b'\n')).then_some((n, end + 2))
    };
    let Some((count, mut pos)) = line(1) else { return p };
    if count <= 0 {
        return p;
    }
    p.skip = pos;
    for _ in 0..count {
        if buf.get(pos) != Some(&b'$') {
            return p;
        }
        let Some((len, data)) = line(pos + 1) else { return p };
        let Ok(len) = usize::try_from(len) else { return p };
        p.skip = data;
        if buf.len() < data + len + 2 {
            p.big = (len >= BIG_ARG).then_some(len);
            return p;
        }
        p.argv += len;
        pos = data + len + 2;
        p.skip = pos;
    }
    // A complete command.
    Partial::default()
}

/// Take one complete command off the front of `buf`.
///
/// `Ok(None)` means more bytes are needed. An empty vector is a no-op request (an empty inline
/// line or `*0`) that the caller skips. Arguments are slices of the original buffer (no copy).
pub fn parse_request(buf: &mut BytesMut, limits: Limits) -> Result<Option<Vec<Bytes>>, ProtoError> {
    let mut args = Vec::new();
    Ok(parse_request_into(buf, limits, &mut args)?.then_some(args))
}

/// [`parse_request`] into `args` (cleared first; a connection reuses its argument vectors):
/// true when a command was taken (`args` empty for a no-op request), false when more bytes are
/// needed.
pub fn parse_request_into(buf: &mut BytesMut, limits: Limits, args: &mut Vec<Bytes>) -> Result<bool, ProtoError> {
    args.clear();
    if buf.is_empty() {
        return Ok(false);
    }
    if buf[0] != b'*' {
        return Ok(match parse_inline(buf)? {
            Some(v) => {
                *args = v;
                true
            }
            None => false,
        });
    }
    let Some((count, mut pos)) = header(buf, 1, || ProtoError("too big mbulk count string".into()))? else {
        return Ok(false);
    };
    let count = parse_len(count).ok_or_else(|| ProtoError("invalid multibulk length".into()))?;
    if count > i32::MAX as i64 {
        return Err(ProtoError("invalid multibulk length".into()));
    }
    if !limits.authenticated && count > 10 {
        return Err(ProtoError("unauthenticated multibulk length".into()));
    }
    if count <= 0 {
        let _ = buf.split_to(pos);
        return Ok(true);
    }
    let count = count as usize;
    // First pass: find where every argument is, without consuming anything (on the stack for
    // the usual few arguments).
    let mut few = [(0usize, 0usize); 16];
    let mut many = Vec::new();
    if count > few.len() {
        many.reserve(count.min(1024));
    }
    for i in 0..count {
        if pos >= buf.len() {
            return Ok(false);
        }
        if buf[pos] != b'$' {
            return Err(ProtoError(expected_dollar(buf[pos])));
        }
        let Some((len, next)) = header(buf, pos + 1, || ProtoError("too big bulk count string".into()))? else {
            return Ok(false);
        };
        let len = parse_len(len).ok_or_else(|| ProtoError("invalid bulk length".into()))?;
        if len < 0 || len as usize > limits.max_bulk {
            return Err(ProtoError("invalid bulk length".into()));
        }
        if !limits.authenticated && len > 16384 {
            return Err(ProtoError("unauthenticated bulk length".into()));
        }
        let len = len as usize;
        if buf.len() < next + len + 2 {
            return Ok(false);
        }
        if count <= few.len() {
            few[i] = (next, len);
        } else {
            many.push((next, len));
        }
        pos = next + len + 2;
    }
    let frame = buf.split_to(pos).freeze();
    let spans = if count <= few.len() { &few[..count] } else { &many[..] };
    args.extend(spans.iter().map(|&(at, len)| frame.slice(at..at + len)));
    Ok(true)
}

fn expected_dollar(got: u8) -> String {
    format!("expected '$', got '{}'", got as char)
}

/// Find `\r\n` after `from`; returns the header text and the offset after the line.
fn header(
    buf: &[u8],
    from: usize,
    too_big: impl Fn() -> ProtoError,
) -> Result<Option<(&[u8], usize)>, ProtoError> {
    match memchr::memchr(b'\r', &buf[from..]) {
        Some(i) => {
            let end = from + i;
            if end + 1 >= buf.len() {
                return Ok(None);
            }
            Ok(Some((&buf[from..end], end + 2)))
        }
        None if buf.len() - from > INLINE_MAX => Err(too_big()),
        None => Ok(None),
    }
}

fn parse_len(s: &[u8]) -> Option<i64> {
    if s.is_empty() || s.len() > 20 {
        return None;
    }
    // Plain digits (every well-formed header): no UTF-8 check or general parse.
    if s.len() <= 18 && s.iter().all(u8::is_ascii_digit) {
        return Some(s.iter().fold(0i64, |n, &c| n * 10 + (c - b'0') as i64));
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

fn parse_inline(buf: &mut BytesMut) -> Result<Option<Vec<Bytes>>, ProtoError> {
    let Some(nl) = memchr::memchr(b'\n', buf) else {
        if buf.len() > INLINE_MAX {
            return Err(ProtoError("too big inline request".into()));
        }
        return Ok(None);
    };
    let line = buf.split_to(nl + 1);
    let mut text = &line[..nl];
    if text.last() == Some(&b'\r') {
        text = &text[..text.len() - 1];
    }
    let args = split_args(text).ok_or_else(|| ProtoError("unbalanced quotes in request".into()))?;
    Ok(Some(args.into_iter().map(Bytes::from).collect()))
}

/// Split a line into arguments the way `redis-cli` and inline commands do (`sdssplitargs`):
/// whitespace separated, with `"double"` (C escapes, `\xHH`) and `'single'` quotes.
/// `None` on unbalanced quotes or a closing quote not followed by a space.
pub fn split_args(line: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    let mut i = 0;
    let n = line.len();
    loop {
        while i < n && line[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            return Some(out);
        }
        let mut cur = Vec::new();
        let (mut inq, mut insq) = (false, false);
        loop {
            if inq {
                if i >= n {
                    return None;
                }
                let c = line[i];
                if c == b'\\' && i + 3 < n && line[i + 1] == b'x' && hex(line[i + 2]).is_some() && hex(line[i + 3]).is_some() {
                    cur.push(hex(line[i + 2]).unwrap() * 16 + hex(line[i + 3]).unwrap());
                    i += 3;
                } else if c == b'\\' && i + 1 < n {
                    i += 1;
                    cur.push(match line[i] {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'b' => 8,
                        b'a' => 7,
                        other => other,
                    });
                } else if c == b'"' {
                    if i + 1 < n && !line[i + 1].is_ascii_whitespace() {
                        return None;
                    }
                    i += 1;
                    break;
                } else {
                    cur.push(c);
                }
            } else if insq {
                if i >= n {
                    return None;
                }
                let c = line[i];
                if c == b'\\' && i + 1 < n && line[i + 1] == b'\'' {
                    i += 1;
                    cur.push(b'\'');
                } else if c == b'\'' {
                    if i + 1 < n && !line[i + 1].is_ascii_whitespace() {
                        return None;
                    }
                    i += 1;
                    break;
                } else {
                    cur.push(c);
                }
            } else {
                if i >= n {
                    break;
                }
                match line[i] {
                    b' ' | b'\n' | b'\r' | b'\t' | 0 => {
                        i += 1;
                        break;
                    }
                    b'"' => inq = true,
                    b'\'' => insq = true,
                    c => cur.push(c),
                }
            }
            i += 1;
        }
        out.push(cur);
    }
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &[u8]) -> Result<Option<Vec<Bytes>>, ProtoError> {
        parse_request(&mut BytesMut::from(s), Limits::default())
    }

    #[test]
    fn multibulk_and_partial() {
        let full = b"*3\r\n$3\r\nSET\r\n$1\r\na\r\n$3\r\nbcd\r\n";
        for cut in 0..full.len() {
            assert_eq!(parse(&full[..cut]).unwrap(), None, "cut at {cut}");
        }
        let mut buf = BytesMut::from(&full[..]);
        buf.extend_from_slice(b"*1\r\n$4\r\nPING\r\n");
        let a = parse_request(&mut buf, Limits::default()).unwrap().unwrap();
        assert_eq!(a, vec![Bytes::from("SET"), Bytes::from("a"), Bytes::from("bcd")]);
        let b = parse_request(&mut buf, Limits::default()).unwrap().unwrap();
        assert_eq!(b, vec![Bytes::from("PING")]);
        assert!(buf.is_empty());
    }

    #[test]
    fn binary_safe_bulk() {
        let a = parse(b"*1\r\n$4\r\na\r\nb\r\n").unwrap().unwrap();
        assert_eq!(&a[0][..], b"a\r\nb");
    }

    #[test]
    fn errors() {
        assert_eq!(parse(b"*x\r\n"), Err(ProtoError("invalid multibulk length".into())));
        assert_eq!(parse(b"*1\r\n+a\r\n"), Err(ProtoError("expected '$', got '+'".into())));
        assert_eq!(parse(b"*1\r\n$-5\r\n"), Err(ProtoError("invalid bulk length".into())));
        let unauth = Limits { authenticated: false, ..Limits::default() };
        assert_eq!(
            parse_request(&mut BytesMut::from(&b"*11\r\n"[..]), unauth),
            Err(ProtoError("unauthenticated multibulk length".into()))
        );
        assert_eq!(
            parse_request(&mut BytesMut::from(&b"*1\r\n$20000\r\n"[..]), unauth),
            Err(ProtoError("unauthenticated bulk length".into()))
        );
        assert_eq!(parse(b"set \"a\n"), Err(ProtoError("unbalanced quotes in request".into())));
    }

    #[test]
    fn inline() {
        assert_eq!(parse(b"PING\r\n").unwrap().unwrap(), vec![Bytes::from("PING")]);
        assert_eq!(parse(b"\r\n").unwrap().unwrap(), Vec::<Bytes>::new());
        assert_eq!(parse(b"set k \"a b\\x41\"\n").unwrap().unwrap(), vec![Bytes::from("set"), Bytes::from("k"), Bytes::from("a bA")]);
        assert_eq!(parse(b"*0\r\n").unwrap().unwrap(), Vec::<Bytes>::new());
        assert_eq!(parse(b"PI").unwrap(), None);
    }

    #[test]
    fn split() {
        assert_eq!(split_args(b"a 'b c' \"d\\ne\"").unwrap(), vec![b"a".to_vec(), b"b c".to_vec(), b"d\ne".to_vec()]);
        assert_eq!(split_args(b"'it\\'s'").unwrap(), vec![b"it's".to_vec()]);
        assert!(split_args(b"\"a\"b").is_none());
        assert!(split_args(b"'a").is_none());
        assert_eq!(split_args(b"  ").unwrap(), Vec::<Vec<u8>>::new());
        assert_eq!(split_args(b"\"\"").unwrap(), vec![Vec::<u8>::new()]);
    }

    #[test]
    fn partial_commands() {
        use super::{Partial, partial};
        let p = |skip, argv, big| Partial { skip, argv, big };
        let mut req = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$40000\r\n".to_vec();
        let data = req.len();
        req.extend_from_slice(&[b'x'; 100]);
        assert_eq!(partial(&req), p(data, 4, Some(40000)));
        assert_eq!(partial(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$4000\r\nxx"), p(27, 4, None));
        assert_eq!(partial(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5"), p(20, 4, None));
        assert_eq!(partial(b"*3\r\n$3\r\nSE"), p(8, 0, None));
        assert_eq!(partial(b"*3\r\n"), p(4, 0, None));
        assert_eq!(partial(b"*3"), p(0, 0, None));
        assert_eq!(partial(b"*1\r\n$4\r\nPING\r\n"), p(0, 0, None));
        assert_eq!(partial(b"PING"), p(0, 0, None));
    }
}
