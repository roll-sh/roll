//! `aws-chunked` request bodies (`Content-Encoding: aws-chunked`), decoded incrementally:
//!
//! ```text
//! <hex size>[;chunk-signature=<sig>]\r\n<data>\r\n   … repeated …
//! 0[;chunk-signature=<sig>]\r\n
//! [<trailer name>:<value>\r\n]*                        x-amz-checksum-*, x-amz-trailer-signature
//! \r\n
//! ```
//!
//! Signed chunks chain from the request's seed signature. Payload bytes are handed out as they
//! arrive, before their chunk's signature can be checked (it covers the whole chunk), so a
//! bad signature surfaces at the end of its chunk: nothing may be committed before
//! [`Decoder::finish`] succeeds. Memory is bounded by [`MAX_LINE`] and [`MAX_TRAILERS`],
//! whatever sizes the peer claims.

use crate::error::{Code, S3Error};
use crate::sigv4::{self, StreamSigning};
use bytes::Bytes;
use sha2::{Digest, Sha256};

/// Longest chunk header or trailer line.
pub const MAX_LINE: usize = 4096;
/// Most trailers accepted.
pub const MAX_TRAILERS: usize = 16;

/// How the body is signed.
#[derive(Clone, Debug)]
pub enum Mode {
    /// `STREAMING-AWS4-HMAC-SHA256-PAYLOAD[-TRAILER]`.
    Signed { signing: StreamSigning, trailer: bool },
    /// `STREAMING-UNSIGNED-PAYLOAD-TRAILER`.
    UnsignedTrailer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Header,
    Data,
    /// The `\r\n` after a chunk's data; how many of its bytes were seen.
    DataEnd(u8),
    Trailers,
    Done,
}

/// The incremental decoder. Feed it the raw body in pieces of any size.
pub struct Decoder {
    mode: Mode,
    state: State,
    line: Vec<u8>,
    remaining: u64,
    chunk_sha: Option<Sha256>,
    chunk_sig: String,
    prev_sig: String,
    decoded: u64,
    expected: Option<u64>,
    trailers: Vec<(String, String)>,
    trailer_sig: Option<String>,
}

fn malformed(why: &str) -> S3Error {
    S3Error::msg(Code::IncompleteBody, format!("Malformed aws-chunked body: {why}"))
}

impl Decoder {
    /// `decoded_length`: `x-amz-decoded-content-length`, enforced when given.
    pub fn new(mode: Mode, decoded_length: Option<u64>) -> Self {
        let prev_sig = match &mode {
            Mode::Signed { signing, .. } => signing.seed_signature.clone(),
            Mode::UnsignedTrailer => String::new(),
        };
        Self {
            mode,
            state: State::Header,
            line: Vec::new(),
            remaining: 0,
            chunk_sha: None,
            chunk_sig: String::new(),
            prev_sig,
            decoded: 0,
            expected: decoded_length,
            trailers: Vec::new(),
            trailer_sig: None,
        }
    }

    /// Payload bytes so far.
    pub fn decoded(&self) -> u64 {
        self.decoded
    }

    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    fn signing(&self) -> Option<&StreamSigning> {
        match &self.mode {
            Mode::Signed { signing, .. } => Some(signing),
            Mode::UnsignedTrailer => None,
        }
    }

    fn trailer_allowed(&self) -> bool {
        matches!(self.mode, Mode::Signed { trailer: true, .. } | Mode::UnsignedTrailer)
    }

    /// Take one line (without `\r\n`) out of `input`, or buffer what's there. `Ok(None)`: need
    /// more input.
    fn take_line(&mut self, input: &mut Bytes) -> Result<Option<Vec<u8>>, S3Error> {
        match input.iter().position(|&b| b == b'\n') {
            Some(i) => {
                let part = input.split_to(i + 1);
                if self.line.len() + part.len() > MAX_LINE + 2 {
                    return Err(malformed("line too long"));
                }
                self.line.extend_from_slice(&part);
                let mut line = std::mem::take(&mut self.line);
                line.pop();
                if line.pop() != Some(b'\r') {
                    return Err(malformed("expected CRLF"));
                }
                Ok(Some(line))
            }
            None => {
                if self.line.len() + input.len() > MAX_LINE + 2 {
                    return Err(malformed("line too long"));
                }
                self.line.extend_from_slice(input);
                input.clear();
                Ok(None)
            }
        }
    }

    fn start_chunk(&mut self, line: &[u8]) -> Result<(), S3Error> {
        let line = std::str::from_utf8(line).map_err(|_| malformed("chunk header is not text"))?;
        let mut parts = line.split(';');
        let size_hex = parts.next().unwrap_or("").trim();
        if size_hex.is_empty() || size_hex.len() > 16 || !size_hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(malformed("bad chunk size"));
        }
        let size = u64::from_str_radix(size_hex, 16).map_err(|_| malformed("bad chunk size"))?;
        let mut sig = None;
        for ext in parts {
            if let Some(s) = ext.trim().strip_prefix("chunk-signature=") {
                sig = Some(s.trim().to_ascii_lowercase());
            }
        }
        if self.signing().is_some() {
            let s = sig.ok_or_else(|| S3Error::msg(Code::SignatureDoesNotMatch, "Chunk is missing its chunk-signature"))?;
            if s.len() != 64 {
                return Err(S3Error::msg(Code::SignatureDoesNotMatch, "Malformed chunk-signature"));
            }
            self.chunk_sig = s;
            self.chunk_sha = Some(Sha256::new());
        }
        if let Some(limit) = self.expected
            && self.decoded.saturating_add(size) > limit
        {
            return Err(S3Error::msg(Code::InvalidRequest, "The body is longer than x-amz-decoded-content-length"));
        }
        self.remaining = size;
        if size == 0 {
            self.end_chunk()?;
            self.state = State::Trailers;
        } else {
            self.state = State::Data;
        }
        Ok(())
    }

    fn end_chunk(&mut self) -> Result<(), S3Error> {
        let sha = self.chunk_sha.take();
        let Some(signing) = self.signing() else { return Ok(()) };
        let hash = hex::encode(sha.unwrap_or_default().finalize());
        let expected = sigv4::chunk_signature(&signing.signing_key, &signing.amz_date, &signing.scope, &self.prev_sig, &hash);
        if expected != self.chunk_sig {
            return Err(S3Error::msg(Code::SignatureDoesNotMatch, "The chunk signature we calculated does not match the signature you provided."));
        }
        self.prev_sig = std::mem::take(&mut self.chunk_sig);
        Ok(())
    }

    fn trailer_line(&mut self, line: &[u8]) -> Result<(), S3Error> {
        if line.is_empty() {
            return self.end_trailers();
        }
        if !self.trailer_allowed() {
            return Err(malformed("trailers without a trailer payload mode"));
        }
        let line = std::str::from_utf8(line).map_err(|_| malformed("trailer is not text"))?;
        let (k, v) = line.split_once(':').ok_or_else(|| malformed("bad trailer"))?;
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
        if k == "x-amz-trailer-signature" {
            self.trailer_sig = Some(v.to_ascii_lowercase());
            return Ok(());
        }
        if self.trailers.len() >= MAX_TRAILERS {
            return Err(malformed("too many trailers"));
        }
        self.trailers.push((k, v));
        Ok(())
    }

    fn end_trailers(&mut self) -> Result<(), S3Error> {
        if let Mode::Signed { signing, trailer: true } = &self.mode
            && !self.trailers.is_empty()
        {
            let got = self.trailer_sig.as_deref().ok_or_else(|| S3Error::msg(Code::SignatureDoesNotMatch, "Missing x-amz-trailer-signature"))?;
            let expected = sigv4::trailer_signature(&signing.signing_key, &signing.amz_date, &signing.scope, &self.prev_sig, &self.trailers);
            if expected != got {
                return Err(S3Error::msg(Code::SignatureDoesNotMatch, "The trailer signature we calculated does not match the signature you provided."));
            }
        }
        if let Some(limit) = self.expected
            && self.decoded != limit
        {
            return Err(S3Error::new(Code::IncompleteBody));
        }
        self.state = State::Done;
        Ok(())
    }

    /// Decode the next piece of the body, appending payload slices to `out` (zero-copy).
    pub fn feed(&mut self, mut input: Bytes, out: &mut Vec<Bytes>) -> Result<(), S3Error> {
        while !input.is_empty() {
            match self.state {
                State::Header => {
                    if let Some(line) = self.take_line(&mut input)? {
                        self.start_chunk(&line)?;
                    }
                }
                State::Data => {
                    let n = (self.remaining.min(input.len() as u64)) as usize;
                    let data = input.split_to(n);
                    if let Some(h) = &mut self.chunk_sha {
                        h.update(&data);
                    }
                    self.decoded += n as u64;
                    self.remaining -= n as u64;
                    out.push(data);
                    if self.remaining == 0 {
                        self.end_chunk()?;
                        self.state = State::DataEnd(0);
                    }
                }
                State::DataEnd(seen) => {
                    let want = if seen == 0 { b'\r' } else { b'\n' };
                    if input[0] != want {
                        return Err(malformed("expected CRLF after chunk data"));
                    }
                    let _ = input.split_to(1);
                    self.state = if seen == 0 { State::DataEnd(1) } else { State::Header };
                }
                State::Trailers => {
                    if let Some(line) = self.take_line(&mut input)? {
                        self.trailer_line(&line)?;
                    }
                }
                State::Done => return Err(malformed("data after the final chunk")),
            }
        }
        Ok(())
    }

    /// End of body: the trailers (lowercase names, signature removed), or `IncompleteBody` if
    /// the body stopped early.
    pub fn finish(self) -> Result<Vec<(String, String)>, S3Error> {
        if self.state != State::Done {
            return Err(S3Error::new(Code::IncompleteBody));
        }
        Ok(self.trailers)
    }
}

/// Encode a body as `aws-chunked` (tests and clients). With `signing`, chunks are signed and
/// trailers too; `chunk` is the payload size per chunk.
pub fn encode(payload: &[u8], chunk: usize, signing: Option<&StreamSigning>, trailers: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 256);
    let mut prev = signing.map(|s| s.seed_signature.clone()).unwrap_or_default();
    let mut pieces: Vec<&[u8]> = payload.chunks(chunk.max(1)).collect();
    pieces.push(&[]);
    for p in pieces {
        out.extend_from_slice(format!("{:x}", p.len()).as_bytes());
        if let Some(s) = signing {
            let sig = sigv4::chunk_signature(&s.signing_key, &s.amz_date, &s.scope, &prev, &hex::encode(Sha256::digest(p)));
            out.extend_from_slice(format!(";chunk-signature={sig}").as_bytes());
            prev = sig;
        }
        out.extend_from_slice(b"\r\n");
        if !p.is_empty() {
            out.extend_from_slice(p);
            out.extend_from_slice(b"\r\n");
        }
    }
    for (k, v) in trailers {
        out.extend_from_slice(format!("{k}:{v}\r\n").as_bytes());
    }
    if let (Some(s), false) = (signing, trailers.is_empty()) {
        let sig = sigv4::trailer_signature(&s.signing_key, &s.amz_date, &s.scope, &prev, trailers);
        out.extend_from_slice(format!("x-amz-trailer-signature:{sig}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sigv4::{Request, authenticate, signing_key};

    fn run(dec: &mut Decoder, body: &[u8], split: impl Fn(usize) -> usize) -> Result<Vec<u8>, S3Error> {
        let mut out = Vec::new();
        let mut pieces = Vec::new();
        let mut i = 0;
        let mut n = 0;
        while i < body.len() {
            let len = split(n).clamp(1, body.len() - i);
            dec.feed(Bytes::copy_from_slice(&body[i..i + len]), &mut pieces)?;
            i += len;
            n += 1;
        }
        for p in pieces {
            out.extend_from_slice(&p);
        }
        Ok(out)
    }

    fn example_signing() -> StreamSigning {
        let (headers, _) = crate::sigv4::tests::chunked_example();
        let req = Request { method: "PUT", path: "/examplebucket/chunkObject.txt", query: "", headers: &headers };
        authenticate(&req, 1_369_353_600, |_| Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into())).unwrap().unwrap().stream.unwrap()
    }

    #[test]
    fn aws_example_any_split() {
        let (_, body) = crate::sigv4::tests::chunked_example();
        let signing = example_signing();
        let splits: Vec<Box<dyn Fn(usize) -> usize>> = vec![
            Box::new(|_| 1),
            Box::new(|_| 7),
            Box::new(|_| 65_536),
            Box::new(|_| usize::MAX),
            Box::new(|n| {
                let mut x = (n as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
                x ^= x >> 29;
                (x % 5000) as usize + 1
            }),
        ];
        for split in splits {
            let mut d = Decoder::new(Mode::Signed { signing: signing.clone(), trailer: false }, Some(66_560));
            let payload = run(&mut d, &body, split).unwrap();
            assert_eq!(payload, vec![b'a'; 66_560]);
            assert!(d.is_done());
            assert_eq!(d.finish().unwrap(), vec![]);
        }
    }

    #[test]
    fn rejects_tampering() {
        let (_, body) = crate::sigv4::tests::chunked_example();
        let signing = example_signing();
        let mode = || Mode::Signed { signing: signing.clone(), trailer: false };
        // A changed payload byte.
        let mut b = body.clone();
        b[200] = b'b';
        assert_eq!(run(&mut Decoder::new(mode(), Some(66_560)), &b, |_| 1000).unwrap_err().code, Code::SignatureDoesNotMatch);
        // Truncated.
        let mut d = Decoder::new(mode(), Some(66_560));
        run(&mut d, &body[..body.len() - 3], |_| 1000).unwrap();
        assert_eq!(d.finish().unwrap_err().code, Code::IncompleteBody);
        // Wrong decoded length.
        assert_eq!(run(&mut Decoder::new(mode(), Some(1000)), &body, |_| 100_000).unwrap_err().code, Code::InvalidRequest);
        assert_eq!(run(&mut Decoder::new(mode(), Some(70_000)), &body, |_| 100_000).unwrap_err().code, Code::IncompleteBody);
        // Unsigned chunks under a signed mode.
        let unsigned = encode(b"hello", 2, None, &[]);
        assert_eq!(run(&mut Decoder::new(mode(), None), &unsigned, |_| 100).unwrap_err().code, Code::SignatureDoesNotMatch);
        // Garbage after the end.
        let mut extra = body.clone();
        extra.extend_from_slice(b"x");
        assert!(run(&mut Decoder::new(mode(), Some(66_560)), &extra, |_| 100_000).is_err());
        // An endless header line doesn't grow memory.
        let long = vec![b'1'; MAX_LINE * 3];
        assert_eq!(run(&mut Decoder::new(Mode::UnsignedTrailer, None), &long, |_| 100).unwrap_err().code, Code::IncompleteBody);
        // A huge claimed chunk size is fine until bytes run out.
        let mut d = Decoder::new(Mode::UnsignedTrailer, None);
        run(&mut d, b"ffffffffffffffff\r\nabc", |_| 100).unwrap();
        assert_eq!(d.decoded(), 3);
        assert_eq!(d.finish().unwrap_err().code, Code::IncompleteBody);
        assert!(run(&mut Decoder::new(Mode::UnsignedTrailer, None), b"10000000000000000\r\n", |_| 100).is_err());
    }

    #[test]
    fn signed_trailers() {
        let signing = StreamSigning {
            signing_key: signing_key("secret", "20240101", "auto", "s3"),
            seed_signature: "a".repeat(64),
            amz_date: "20240101T000000Z".into(),
            scope: "20240101/auto/s3/aws4_request".into(),
        };
        let payload: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let crc = crate::checksum::b64(&crate::checksum::Algorithm::Crc32c.checksum(&payload));
        let trailers = vec![("x-amz-checksum-crc32c".to_string(), crc.clone())];
        let body = encode(&payload, 8192, Some(&signing), &trailers);
        let mode = Mode::Signed { signing: signing.clone(), trailer: true };
        let mut d = Decoder::new(mode.clone(), Some(payload.len() as u64));
        assert_eq!(run(&mut d, &body, |n| n * 13 % 977 + 1).unwrap(), payload);
        assert_eq!(d.finish().unwrap(), trailers);
        // A changed trailer value fails the trailer signature.
        let mut b = body.clone();
        let pos = b.windows(crc.len()).position(|w| w == crc.as_bytes()).unwrap();
        b[pos] = if b[pos] == b'A' { b'B' } else { b'A' };
        assert_eq!(run(&mut Decoder::new(mode.clone(), None), &b, |_| 4096).unwrap_err().code, Code::SignatureDoesNotMatch);
        // Trailers in a non-trailer mode are refused.
        assert!(run(&mut Decoder::new(Mode::Signed { signing, trailer: false }, None), &body, |_| 4096).is_err());
    }

    #[test]
    fn unsigned_trailer_mode() {
        let payload = b"Hello world".repeat(1000);
        let crc = crate::checksum::b64(&crate::checksum::Algorithm::Crc32.checksum(&payload));
        let trailers = vec![("x-amz-checksum-crc32".to_string(), crc)];
        let body = encode(&payload, 4000, None, &trailers);
        let mut d = Decoder::new(Mode::UnsignedTrailer, Some(payload.len() as u64));
        assert_eq!(run(&mut d, &body, |_| 3).unwrap(), payload);
        assert_eq!(d.finish().unwrap(), trailers);
        // Empty body, no trailers.
        let mut d = Decoder::new(Mode::UnsignedTrailer, Some(0));
        assert_eq!(run(&mut d, b"0\r\n\r\n", |_| 1).unwrap(), b"");
        assert_eq!(d.finish().unwrap(), vec![]);
        // Missing CRLF after data.
        assert!(run(&mut Decoder::new(Mode::UnsignedTrailer, None), b"3\r\nabcX\n0\r\n\r\n", |_| 100).is_err());
        let many: String = (0..20).map(|i| format!("x-t{i}:v\r\n")).collect();
        let body = format!("0\r\n{many}\r\n");
        assert!(run(&mut Decoder::new(Mode::UnsignedTrailer, None), body.as_bytes(), |_| 100).is_err());
    }
}
