//! Amazon's event stream framing (`application/vnd.amazon.eventstream`), as Bedrock's
//! `InvokeModelWithResponseStream` answers: a stream of binary messages, each
//!
//! ```text
//! total length u32 | headers length u32 | prelude CRC32 u32 | headers | payload | message CRC32 u32
//! ```
//!
//! (big-endian; both CRCs are CRC-32/ISO-HDLC, the prelude's over the first 8 bytes and the
//! message's over everything before it). A message is at most [`MAX_MESSAGE`]; its length is
//! checked before anything waits for it, and the buffer only ever holds bytes that arrived.

use bytes::{Buf, BytesMut};

/// The largest message accepted.
pub const MAX_MESSAGE: usize = 16 << 20;
const PRELUDE: usize = 12;
const MIN_MESSAGE: usize = PRELUDE + 4;

/// One message: its string headers (`:message-type`, `:event-type`, `:exception-type`,
/// `:content-type`) and its payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub headers: Vec<(String, String)>,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("event stream message of {0} bytes (at most {MAX_MESSAGE})")]
    TooLarge(usize),
    #[error("event stream: {0}")]
    Invalid(&'static str),
}

fn crc_table() -> &'static [u32; 256] {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

/// CRC-32 (ISO-HDLC, the zlib one).
pub fn crc32(data: &[u8]) -> u32 {
    let t = crc_table();
    !data.iter().fold(!0u32, |c, &b| t[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8))
}

/// Splits bytes into messages.
#[derive(Default)]
pub struct Decoder {
    buf: BytesMut,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// The messages `chunk` completes.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Message>, FrameError> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < PRELUDE {
                break;
            }
            let total = u32::from_be_bytes(self.buf[0..4].try_into().unwrap()) as usize;
            let headers_len = u32::from_be_bytes(self.buf[4..8].try_into().unwrap()) as usize;
            let prelude_crc = u32::from_be_bytes(self.buf[8..12].try_into().unwrap());
            if crc32(&self.buf[0..8]) != prelude_crc {
                return Err(FrameError::Invalid("prelude CRC mismatch"));
            }
            if total > MAX_MESSAGE {
                return Err(FrameError::TooLarge(total));
            }
            if total < MIN_MESSAGE || headers_len > total - MIN_MESSAGE {
                return Err(FrameError::Invalid("bad lengths"));
            }
            if self.buf.len() < total {
                break;
            }
            let msg = self.buf.split_to(total);
            let crc = u32::from_be_bytes(msg[total - 4..].try_into().unwrap());
            if crc32(&msg[..total - 4]) != crc {
                return Err(FrameError::Invalid("message CRC mismatch"));
            }
            let headers = parse_headers(&msg[PRELUDE..PRELUDE + headers_len])?;
            out.push(Message { headers, payload: msg[PRELUDE + headers_len..total - 4].to_vec() });
        }
        Ok(out)
    }

    /// Bytes of a message that never finished.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

/// Headers: name length u8, name, type u8, value. String values are kept; the rest skipped.
fn parse_headers(mut b: &[u8]) -> Result<Vec<(String, String)>, FrameError> {
    let short = FrameError::Invalid("truncated header");
    let mut out = Vec::new();
    while b.has_remaining() {
        let n = b.get_u8() as usize;
        if b.remaining() < n + 1 {
            return Err(short);
        }
        let name = String::from_utf8_lossy(&b[..n]).into_owned();
        b.advance(n);
        let fixed = match b.get_u8() {
            0 | 1 => 0,
            2 => 1,
            3 => 2,
            4 => 4,
            5 | 8 => 8,
            9 => 16,
            6 | 7 => {
                if b.remaining() < 2 {
                    return Err(short);
                }
                let len = b.get_u16() as usize;
                if b.remaining() < len {
                    return Err(short);
                }
                out.push((name, String::from_utf8_lossy(&b[..len]).into_owned()));
                b.advance(len);
                continue;
            }
            _ => return Err(FrameError::Invalid("unknown header type")),
        };
        if b.remaining() < fixed {
            return Err(short);
        }
        b.advance(fixed);
    }
    Ok(out)
}

/// Encodes a message with string headers (mock upstreams in tests).
pub fn encode(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (k, v) in headers {
        h.push(k.len() as u8);
        h.extend_from_slice(k.as_bytes());
        h.push(7);
        h.extend_from_slice(&(v.len() as u16).to_be_bytes());
        h.extend_from_slice(v.as_bytes());
    }
    let total = PRELUDE + h.len() + payload.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(h.len() as u32).to_be_bytes());
    out.extend_from_slice(&crc32(&out[0..8]).to_be_bytes());
    out.extend_from_slice(&h);
    out.extend_from_slice(payload);
    out.extend_from_slice(&crc32(&out).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn messages_split_anywhere() {
        let a = encode(&[(":message-type", "event"), (":event-type", "chunk")], br#"{"bytes":"e30="}"#);
        let b = encode(&[(":message-type", "exception"), (":exception-type", "throttlingException")], br#"{"message":"slow down"}"#);
        let all = [a.clone(), b].concat();
        for cut in 0..all.len() {
            let mut d = Decoder::new();
            let mut got = d.feed(&all[..cut]).unwrap();
            got.extend(d.feed(&all[cut..]).unwrap());
            assert_eq!(got.len(), 2, "cut at {cut}");
            assert_eq!(got[0].header(":event-type"), Some("chunk"));
            assert_eq!(got[1].header(":exception-type"), Some("throttlingException"));
            assert_eq!(d.pending(), 0);
        }
        let mut bad = a.clone();
        let n = bad.len();
        bad[n - 6] ^= 1;
        assert!(Decoder::new().feed(&bad).is_err());
    }

    #[test]
    fn a_huge_length_is_refused_before_waiting_for_it() {
        let mut prelude = Vec::new();
        prelude.extend_from_slice(&(u32::MAX).to_be_bytes());
        prelude.extend_from_slice(&0u32.to_be_bytes());
        let crc = crc32(&prelude);
        prelude.extend_from_slice(&crc.to_be_bytes());
        assert!(matches!(Decoder::new().feed(&prelude), Err(FrameError::TooLarge(_))));
    }
}
