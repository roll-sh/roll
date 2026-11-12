//! The AWS event-stream binary encoding (`application/vnd.amazon.eventstream`), as
//! SelectObjectContent streams its results.
//!
//! A message is:
//!
//! ```text
//! total length (u32 BE) | headers length (u32 BE) | prelude CRC32 (of the 8 bytes before)
//! headers | payload | message CRC32 (of everything before it)
//! ```
//!
//! Each header is `name length (u8) | name | type (u8, 7 = string) | value length (u16 BE) |
//! value`. Only string headers are needed. CRCs are CRC-32 (ISO-HDLC, as zlib's).

use crc_fast::CrcAlgorithm;

/// A header value's type: a UTF-8 string.
const STRING: u8 = 7;

fn crc32(data: &[u8]) -> u32 {
    crc_fast::checksum(CrcAlgorithm::Crc32IsoHdlc, data) as u32
}

/// One message with string headers.
pub fn message(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (name, value) in headers {
        debug_assert!(name.len() <= u8::MAX as usize && value.len() <= u16::MAX as usize);
        h.push(name.len() as u8);
        h.extend_from_slice(name.as_bytes());
        h.push(STRING);
        h.extend_from_slice(&(value.len() as u16).to_be_bytes());
        h.extend_from_slice(value.as_bytes());
    }
    let total = 12 + h.len() + payload.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(h.len() as u32).to_be_bytes());
    let prelude = crc32(&out);
    out.extend_from_slice(&prelude.to_be_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(payload);
    let crc = crc32(&out);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn event(kind: &str, content_type: Option<&str>, payload: &[u8]) -> Vec<u8> {
    match content_type {
        Some(ct) => message(&[(":event-type", kind), (":content-type", ct), (":message-type", "event")], payload),
        None => message(&[(":event-type", kind), (":message-type", "event")], payload),
    }
}

/// A `Records` event: a chunk of the query's output.
pub fn records(payload: &[u8]) -> Vec<u8> {
    event("Records", Some("application/octet-stream"), payload)
}

fn counts(root: &str, scanned: u64, processed: u64, returned: u64) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><{root} xmlns=\"\"><BytesScanned>{scanned}</BytesScanned><BytesProcessed>{processed}</BytesProcessed><BytesReturned>{returned}</BytesReturned></{root}>"
    )
}

/// A `Stats` event (sent once, before `End`).
pub fn stats(scanned: u64, processed: u64, returned: u64) -> Vec<u8> {
    event("Stats", Some("text/xml"), counts("Stats", scanned, processed, returned).as_bytes())
}

/// A `Progress` event (when the request asked for progress).
pub fn progress(scanned: u64, processed: u64, returned: u64) -> Vec<u8> {
    event("Progress", Some("text/xml"), counts("Progress", scanned, processed, returned).as_bytes())
}

/// A `Cont` event: keeps the connection alive while nothing else is sent.
pub fn cont() -> Vec<u8> {
    event("Cont", None, b"")
}

/// The `End` event: the request completed.
pub fn end() -> Vec<u8> {
    event("End", None, b"")
}

/// An error, in the stream (after the response's headers went out).
pub fn error(code: &str, message: &str) -> Vec<u8> {
    // Header values are at most 64 KiB; messages are short, but never send a broken frame.
    let message = truncate(message, 4096);
    self::message(&[(":error-code", code), (":error-message", message), (":message-type", "error")], b"")
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A decoded message (tests and tools).
#[derive(Debug, PartialEq, Eq)]
pub struct Decoded {
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl Decoded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Decode one message from the start of `buf` (string headers only): the message and how many
/// bytes it took. `None` if `buf` is incomplete, malformed, or a CRC doesn't match.
pub fn decode(buf: &[u8]) -> Option<(Decoded, usize)> {
    if buf.len() < 16 {
        return None;
    }
    let total = u32::from_be_bytes(buf[0..4].try_into().ok()?) as usize;
    let hlen = u32::from_be_bytes(buf[4..8].try_into().ok()?) as usize;
    if total < 16 || buf.len() < total || hlen > total - 16 {
        return None;
    }
    if crc32(&buf[..8]) != u32::from_be_bytes(buf[8..12].try_into().ok()?) || crc32(&buf[..total - 4]) != u32::from_be_bytes(buf[total - 4..total].try_into().ok()?) {
        return None;
    }
    let mut h = &buf[12..12 + hlen];
    let mut headers = Vec::new();
    while !h.is_empty() {
        let n = *h.first()? as usize;
        let name = std::str::from_utf8(h.get(1..1 + n)?).ok()?.to_string();
        h = &h[1 + n..];
        if *h.first()? != STRING {
            return None;
        }
        let vlen = u16::from_be_bytes(h.get(1..3)?.try_into().ok()?) as usize;
        let value = std::str::from_utf8(h.get(3..3 + vlen)?).ok()?.to_string();
        h = &h[3 + vlen..];
        headers.push((name, value));
    }
    Some((Decoded { headers, payload: buf[12 + hlen..total - 4].to_vec() }, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_is_zlibs() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// AWS's published vector: an empty message with no headers is 16 bytes,
    /// `00000010 00000000 05c248eb 7d98c8ff`.
    #[test]
    fn empty_message_vector() {
        assert_eq!(hex::encode(message(&[], b"")), "000000100000000005c248eb7d98c8ff");
    }

    #[test]
    fn round_trips() {
        let mut stream = Vec::new();
        stream.extend(records(b"a,b\n"));
        stream.extend(cont());
        stream.extend(progress(1, 2, 3));
        stream.extend(stats(10, 20, 4));
        stream.extend(end());
        let mut at = 0;
        let mut kinds = Vec::new();
        while at < stream.len() {
            let (m, used) = decode(&stream[at..]).expect("a whole message");
            at += used;
            kinds.push(m.header(":event-type").unwrap().to_string());
            if m.header(":event-type") == Some("Records") {
                assert_eq!(m.payload, b"a,b\n");
                assert_eq!(m.header(":content-type"), Some("application/octet-stream"));
            }
            if m.header(":event-type") == Some("Stats") {
                let s = String::from_utf8(m.payload).unwrap();
                assert!(s.contains("<BytesScanned>10</BytesScanned><BytesProcessed>20</BytesProcessed><BytesReturned>4</BytesReturned>"), "{s}");
            }
        }
        assert_eq!(kinds, ["Records", "Cont", "Progress", "Stats", "End"]);
        let e = decode(&error("CastFailed", "nope")).unwrap().0;
        assert_eq!((e.header(":message-type"), e.header(":error-code"), e.header(":error-message")), (Some("error"), Some("CastFailed"), Some("nope")));
        assert!(error("X", &"é".repeat(5000)).len() < 5000, "long messages are cut on a char boundary");
    }

    #[test]
    fn corruption_is_detected() {
        let mut m = records(b"payload");
        assert!(decode(&m[..m.len() - 1]).is_none(), "incomplete");
        let n = m.len();
        m[n - 6] ^= 1;
        assert!(decode(&m).is_none(), "message CRC");
        let mut m = records(b"payload");
        m[5] ^= 1;
        assert!(decode(&m).is_none(), "prelude CRC");
    }
}
