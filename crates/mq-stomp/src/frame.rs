//! STOMP frames: an incremental parser (EOL heart-beats between frames, CRLF line endings,
//! `content-length` bodies, 1.1/1.2 header escaping, size limits) and the encoder.

use bytes::{BufMut, Bytes, BytesMut};

/// Negotiated protocol version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
    V10,
    V11,
    V12,
}

impl Version {
    pub fn as_str(self) -> &'static str {
        match self {
            Version::V10 => "1.0",
            Version::V11 => "1.1",
            Version::V12 => "1.2",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "1.0" => Some(Version::V10),
            "1.1" => Some(Version::V11),
            "1.2" => Some(Version::V12),
            _ => None,
        }
    }

    /// The highest version both sides speak (`accept-version` absent means 1.0).
    pub fn negotiate(accept: Option<&str>) -> Option<Self> {
        match accept {
            None => Some(Version::V10),
            Some(a) => a.split(',').filter_map(Version::parse).max(),
        }
    }
}

pub const SUPPORTED_VERSIONS: &str = "1.0,1.1,1.2";

/// A parsed client frame. Repeated headers keep their first value, as the spec requires.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub command: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Frame {
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn has(&self, key: &str) -> bool {
        self.header(key).is_some()
    }

    /// `true`/`True`/`false`/`False`; anything else counts as absent (RabbitMQ).
    pub fn bool_header(&self, key: &str) -> Option<bool> {
        match self.header(key)? {
            "true" | "True" => Some(true),
            "false" | "False" => Some(false),
            _ => None,
        }
    }

    pub fn int_header(&self, key: &str) -> Option<i64> {
        self.header(key)?.trim().parse().ok()
    }
}

/// Parser limits (RabbitMQ: 100 headers of up to 100 KiB, 4 MiB frames once authenticated).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_frame: usize,
    pub max_headers: usize,
    pub max_header_len: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The frame exceeds `max_frame` bytes.
    TooLarge(usize),
    Malformed(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::TooLarge(max) => write!(f, "frame is larger than the maximum frame size of {max} bytes"),
            ParseError::Malformed(m) => f.write_str(m),
        }
    }
}

const MAX_COMMAND_LEN: usize = 32;

/// Command and headers of a frame whose body hasn't fully arrived yet.
struct Head {
    command: String,
    headers: Vec<(String, String)>,
    body_start: usize,
    content_length: Option<usize>,
}

/// Incremental frame parser over a growing byte buffer.
pub struct Parser {
    pub buf: BytesMut,
    pub limits: Limits,
    head: Option<Head>,
    /// How far the buffer has been searched (header terminator or body NUL).
    scan: usize,
}

fn malformed(m: impl Into<String>) -> ParseError {
    ParseError::Malformed(m.into())
}

fn find(hay: &[u8], from: usize, b: u8) -> Option<usize> {
    hay.get(from..)?.iter().position(|x| *x == b).map(|p| p + from)
}

/// Undo 1.1/1.2 header escaping (`\r`, `\n`, `\c`, `\\`); any other escape is an error.
pub fn unescape(s: &[u8]) -> Result<Vec<u8>, ParseError> {
    let mut out = Vec::with_capacity(s.len());
    let mut it = s.iter();
    while let Some(&c) = it.next() {
        if c != b'\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b'c') => out.push(b':'),
            Some(b'\\') => out.push(b'\\'),
            Some(&x) => return Err(malformed(format!("bad header escape sequence '\\{}'", x as char))),
            None => return Err(malformed("bad header escape sequence at end of header")),
        }
    }
    Ok(out)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

impl Parser {
    pub fn new(limits: Limits) -> Self {
        Parser { buf: BytesMut::with_capacity(16 * 1024), limits, head: None, scan: 0 }
    }

    /// The next complete frame, if any. `version` decides header escaping (none before CONNECT
    /// and in 1.0, and never on CONNECT/STOMP frames) and 1.0's leading-whitespace trimming.
    pub fn next(&mut self, version: Option<Version>) -> Result<Option<Frame>, ParseError> {
        if self.head.is_none() {
            if !self.skip_noise()? {
                return Ok(None);
            }
            match self.parse_head(version)? {
                Some(h) => {
                    self.scan = h.body_start;
                    self.head = Some(h);
                }
                None => return Ok(None),
            }
        }
        let Some(h) = &self.head else { return Ok(None) };
        let max = self.limits.max_frame;
        let end = match h.content_length {
            Some(n) => {
                let end = h.body_start + n;
                if end + 1 > max {
                    return Err(ParseError::TooLarge(max));
                }
                if self.buf.len() <= end {
                    return Ok(None);
                }
                if self.buf[end] != 0 {
                    return Err(malformed("frame body is not terminated by NUL"));
                }
                end
            }
            None => match find(&self.buf, self.scan, 0) {
                Some(end) => end,
                None => {
                    if self.buf.len() > max {
                        return Err(ParseError::TooLarge(max));
                    }
                    self.scan = self.buf.len();
                    return Ok(None);
                }
            },
        };
        if end + 1 > max {
            return Err(ParseError::TooLarge(max));
        }
        let Some(h) = self.head.take() else { return Ok(None) };
        let mut raw = self.buf.split_to(end + 1).freeze();
        let body = raw.split_off(h.body_start).slice(..end - h.body_start);
        self.scan = 0;
        Ok(Some(Frame { command: h.command, headers: h.headers, body }))
    }

    /// Drop EOLs (heart-beats) and stray NULs between frames. False when the buffer is empty
    /// or ends in a lone CR.
    fn skip_noise(&mut self) -> Result<bool, ParseError> {
        let mut i = 0;
        let b = &self.buf;
        while i < b.len() {
            match b[i] {
                b'\n' | 0 => i += 1,
                b'\r' => match b.get(i + 1) {
                    Some(b'\n') => i += 2,
                    Some(_) => return Err(malformed("unexpected characters between frames")),
                    None => break,
                },
                _ => break,
            }
        }
        let _ = self.buf.split_to(i);
        Ok(!self.buf.is_empty() && self.buf[0] != b'\r')
    }

    fn parse_head(&mut self, version: Option<Version>) -> Result<Option<Head>, ParseError> {
        let limits = self.limits;
        let head_max = limits.max_frame.min(limits.max_header_len.saturating_mul(limits.max_headers + 1));
        // Find the blank line ending the headers, resuming the search where it stopped.
        let mut from = self.scan.saturating_sub(2);
        let body_start = loop {
            let Some(nl) = find(&self.buf, from, b'\n') else {
                if self.buf.len() > head_max {
                    return Err(ParseError::TooLarge(head_max));
                }
                self.scan = self.buf.len();
                return Ok(None);
            };
            let next = nl + 1;
            let blank = match self.buf.get(next) {
                Some(b'\n') => Some(next + 1),
                Some(b'\r') => match self.buf.get(next + 1) {
                    Some(b'\n') => Some(next + 2),
                    Some(_) => None,
                    None => {
                        self.scan = next;
                        return Ok(None);
                    }
                },
                Some(_) => None,
                None => {
                    self.scan = nl;
                    return Ok(None);
                }
            };
            if let Some(start) = blank {
                break start;
            }
            if nl + 1 > head_max {
                return Err(ParseError::TooLarge(head_max));
            }
            from = next;
        };
        let head = &self.buf[..body_start];
        let mut lines = head.split(|b| *b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
        let command_line = lines.next().unwrap_or_default();
        if command_line.len() > MAX_COMMAND_LEN {
            return Err(malformed("command too long"));
        }
        if command_line.contains(&b'\r') {
            return Err(malformed("unexpected characters in command"));
        }
        let command = text(command_line);
        let connect = matches!(command.as_str(), "CONNECT" | "STOMP");
        let escapes = !connect && version.is_some_and(|v| v >= Version::V11);
        let trim = !connect && version == Some(Version::V10);
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            let Some(colon) = line.iter().position(|b| *b == b':') else {
                return Err(malformed(format!("header '{}' has no value", text(line))));
            };
            let (name, value) = (&line[..colon], &line[colon + 1..]);
            if name.len() > limits.max_header_len || value.len() > limits.max_header_len {
                return Err(malformed(format!("header is longer than {} bytes", limits.max_header_len)));
            }
            let (name, mut value) = if escapes { (text(&unescape(name)?), text(&unescape(value)?)) } else { (text(name), text(value)) };
            if trim {
                value = value.trim_start().to_string();
            }
            if headers.iter().any(|(k, _)| *k == name) {
                continue;
            }
            if headers.len() >= limits.max_headers {
                return Err(malformed(format!("too many headers (limit {})", limits.max_headers)));
            }
            headers.push((name, value));
        }
        let content_length = match headers.iter().find(|(k, _)| k == "content-length") {
            Some((_, v)) => match v.trim().parse::<i64>() {
                Ok(n) if n < 0 => return Err(malformed(format!("invalid content-length {n}"))),
                Ok(n) => Some(usize::try_from(n).map_err(|_| ParseError::TooLarge(limits.max_frame))?),
                Err(_) => None,
            },
            None => None,
        };
        Ok(Some(Head { command, headers, body_start, content_length }))
    }
}

/// Escape a header name or value for 1.1/1.2.
fn put_escaped(out: &mut BytesMut, s: &str) {
    for &b in s.as_bytes() {
        match b {
            b'\n' => out.put_slice(b"\\n"),
            b'\r' => out.put_slice(b"\\r"),
            b':' => out.put_slice(b"\\c"),
            b'\\' => out.put_slice(b"\\\\"),
            _ => out.put_u8(b),
        }
    }
}

/// Builds outgoing frames.
pub struct Encoder<'a> {
    out: &'a mut BytesMut,
    escape: bool,
}

impl<'a> Encoder<'a> {
    /// Start a frame. CONNECTED headers are never escaped; other frames always are, like
    /// RabbitMQ.
    pub fn new(out: &'a mut BytesMut, command: &str) -> Self {
        out.put_slice(command.as_bytes());
        out.put_u8(b'\n');
        Encoder { out, escape: command != "CONNECTED" }
    }

    pub fn header(&mut self, name: &str, value: &str) -> &mut Self {
        if self.escape {
            put_escaped(self.out, name);
            self.out.put_u8(b':');
            put_escaped(self.out, value);
        } else {
            self.out.put_slice(name.as_bytes());
            self.out.put_u8(b':');
            self.out.put_slice(value.as_bytes());
        }
        self.out.put_u8(b'\n');
        self
    }

    /// Finish with the body (`content-length` is added when it isn't empty) and the NUL, plus a
    /// trailing EOL if configured (RabbitMQ's `trailing_lf`).
    pub fn body(self, body: &[u8], trailing_lf: bool) {
        if !body.is_empty() {
            self.out.put_slice(b"content-length:");
            self.out.put_slice(body.len().to_string().as_bytes());
            self.out.put_u8(b'\n');
        }
        self.out.put_u8(b'\n');
        self.out.put_slice(body);
        self.out.put_u8(0);
        if trailing_lf {
            self.out.put_u8(b'\n');
        }
    }
}

/// A whole frame as bytes.
pub fn encode(command: &str, headers: &[(&str, &str)], body: &[u8], trailing_lf: bool) -> Bytes {
    let mut out = BytesMut::with_capacity(64 + body.len());
    let mut e = Encoder::new(&mut out, command);
    for (k, v) in headers {
        e.header(k, v);
    }
    e.body(body, trailing_lf);
    out.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits { max_frame: 4096, max_headers: 8, max_header_len: 256 };

    fn parse_all(input: &[u8], version: Option<Version>) -> Result<Vec<Frame>, ParseError> {
        let mut p = Parser::new(LIMITS);
        p.buf.extend_from_slice(input);
        let mut v = Vec::new();
        while let Some(f) = p.next(version)? {
            v.push(f);
        }
        Ok(v)
    }

    #[test]
    fn heartbeats_crlf_and_trailing_lf() {
        let frames = parse_all(b"\n\r\n\nSEND\r\ndestination:/queue/a\r\n\r\nhi\0\n\nACK\nid:1\n\n\0", Some(Version::V12)).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].command, "SEND");
        assert_eq!(frames[0].header("destination"), Some("/queue/a"));
        assert_eq!(&frames[0].body[..], b"hi");
        assert_eq!(frames[1].header("id"), Some("1"));
        assert!(frames[1].body.is_empty());
    }

    #[test]
    fn content_length_allows_nul_in_body() {
        let f = parse_all(b"SEND\ncontent-length:3\n\na\0b\0", None).unwrap();
        assert_eq!(&f[0].body[..], b"a\0b");
        assert!(matches!(parse_all(b"SEND\ncontent-length:1\n\nab\0", None), Err(ParseError::Malformed(_))));
    }

    #[test]
    fn incremental_byte_by_byte() {
        let input = b"SEND\ndestination:/queue/a\nreceipt:r1\n\nhello\0";
        let mut p = Parser::new(LIMITS);
        let mut got = None;
        for b in input {
            p.buf.put_u8(*b);
            if let Some(f) = p.next(Some(Version::V12)).unwrap() {
                got = Some(f);
            }
        }
        let f = got.expect("frame");
        assert_eq!(&f.body[..], b"hello");
        assert_eq!(f.header("receipt"), Some("r1"));
    }

    #[test]
    fn escaping_depends_on_version_and_command() {
        let f = parse_all(b"SEND\nk\\cx:a\\nb\\\\c\\r\nk\\cx:dup\n\n\0", Some(Version::V12)).unwrap();
        assert_eq!(f[0].header("k:x"), Some("a\nb\\c\r"));
        assert_eq!(f[0].headers.len(), 1, "repeated headers keep the first value");
        let f = parse_all(b"SEND\nk:a\\nb\n\n\0", Some(Version::V10)).unwrap();
        assert_eq!(f[0].header("k"), Some("a\\nb"));
        let f = parse_all(b"CONNECT\npasscode:p\\w\n\n\0", Some(Version::V12)).unwrap();
        assert_eq!(f[0].header("passcode"), Some("p\\w"));
        assert!(parse_all(b"SEND\nk:a\\tb\n\n\0", Some(Version::V12)).is_err());
        let f = parse_all(b"SEND\nk:   v\n\n\0", Some(Version::V10)).unwrap();
        assert_eq!(f[0].header("k"), Some("v"));
    }

    #[test]
    fn limits() {
        let big = format!("SEND\n\n{}\0", "x".repeat(5000));
        assert_eq!(parse_all(big.as_bytes(), None).unwrap_err(), ParseError::TooLarge(4096));
        assert_eq!(parse_all(b"SEND\ncontent-length:100000\n\n", None).unwrap_err(), ParseError::TooLarge(4096));
        let many: String = (0..9).map(|i| format!("h{i}:v\n")).collect();
        assert!(parse_all(format!("SEND\n{many}\n\0").as_bytes(), None).is_err());
        assert!(parse_all(b"SEND\nnovalue\n\n\0", None).is_err());
        assert!(parse_all(b"\rX", None).is_err());
    }

    #[test]
    fn encode_escapes_except_connected() {
        let b = encode("MESSAGE", &[("a:b", "x\ny")], b"body", true);
        assert_eq!(&b[..], b"MESSAGE\na\\cb:x\\ny\ncontent-length:4\n\nbody\0\n");
        let b = encode("CONNECTED", &[("version", "1.2")], b"", false);
        assert_eq!(&b[..], b"CONNECTED\nversion:1.2\n\n\0");
    }

    #[test]
    fn version_negotiation() {
        assert_eq!(Version::negotiate(None), Some(Version::V10));
        assert_eq!(Version::negotiate(Some("1.0,1.1,1.2")), Some(Version::V12));
        assert_eq!(Version::negotiate(Some("1.1, 2.0")), Some(Version::V11));
        assert_eq!(Version::negotiate(Some("2.0")), None);
    }
}
