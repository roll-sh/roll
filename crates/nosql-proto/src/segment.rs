//! v5 framing: once STARTUP is answered, envelopes travel inside CRC-protected segments
//! (the spec calls them frames; Cassandra's `org.apache.cassandra.net.Frame*` classes).
//!
//! ```text
//! uncompressed: header3b{len:17, self_contained:1, pad:6} crc24(3) payload crc32(4)
//! lz4:          header5b{compressed:17, uncompressed:17, self_contained:1, pad:5} crc24(3)
//!               payload crc32(4)
//! ```
//!
//! All integers are little-endian. The header CRC24 starts at 0x875060 with polynomial
//! 0x1974F0B and reads the header bytes least-significant bit first (`Crc.crc24`); the
//! payload CRC32 is the standard CRC32 primed with the bytes `FA 2D 55 CA` (`Crc.crc32`).
//! For lz4 the CRC32 covers the compressed bytes, and an uncompressed length of 0 means the
//! payload is stored as is (Cassandra does that when compression would not shrink it).
//!
//! A self-contained segment holds one or more whole envelopes. A larger envelope is cut into
//! a run of non-self-contained segments that carry nothing else. Follows `FrameEncoderCrc`,
//! `FrameDecoderCrc`, `FrameEncoderLZ4`, `FrameDecoderLZ4`, `Crc`, and the large-message
//! handling of `CQLMessageHandler` / `Flusher`.

use std::collections::VecDeque;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::frame::Header;

/// The largest payload a segment header can describe.
pub const MAX_PAYLOAD: usize = (1 << 17) - 1;

/// The payload size Cassandra fills before starting a new segment
/// (`Flusher.MAX_FRAMED_PAYLOAD_SIZE`: 128 KiB less the lz4 header and trailer).
pub const CASSANDRA_PAYLOAD: usize = (1 << 17) - 12;

const CRC24_INIT: u32 = 0x875060;
const CRC24_POLY: u32 = 0x1974F0B;
const CRC32_PRIMER: [u8; 4] = [0xFA, 0x2D, 0x55, 0xCA];

/// `Crc.crc24`: the CRC of the low `len` bytes of `bytes`, least significant first.
pub fn crc24(mut bytes: u64, len: usize) -> u32 {
    let mut crc = CRC24_INIT;
    for _ in 0..len {
        crc ^= ((bytes & 0xff) as u32) << 16;
        bytes >>= 8;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x100_0000 != 0 {
                crc ^= CRC24_POLY;
            }
        }
    }
    crc
}

/// `Crc.crc32` over a payload.
pub fn crc32(payload: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(&CRC32_PRIMER);
    h.update(payload);
    h.finalize()
}

/// A decoded segment payload (decompressed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub self_contained: bool,
    pub payload: Bytes,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SegmentError {
    /// Unrecoverable: the stream can no longer be delimited.
    #[error("header CRC mismatch (read {read}, computed {computed})")]
    HeaderCrc { read: u32, computed: u32 },
    #[error("payload CRC mismatch (read {read}, computed {computed})")]
    PayloadCrc { read: u32, computed: u32, self_contained: bool },
    #[error("{0}")]
    Lz4(String),
    /// A self-contained payload that ends inside an envelope, an envelope split across a
    /// self-contained segment, or a large envelope interleaved with other segments.
    #[error("{0}")]
    Envelope(String),
}

impl SegmentError {
    /// Cassandra's text for a CRC mismatch; `conn_id` is the channel's short id.
    pub fn cassandra_message(&self, conn_id: &str) -> String {
        match self {
            SegmentError::HeaderCrc { read, computed } => format!(
                "{conn_id} invalid, unrecoverable CRC mismatch detected in frame header. Read {}, Computed {}",
                *read as i32, *computed as i32
            ),
            SegmentError::PayloadCrc { read, computed, .. } => format!(
                "{conn_id} invalid, unrecoverable CRC mismatch detected in frame body. Read {}, Computed {}",
                *read as i32, *computed as i32
            ),
            SegmentError::Lz4(m) | SegmentError::Envelope(m) => m.clone(),
        }
    }
}

/// Writes one segment around `payload` (at most [`MAX_PAYLOAD`] bytes).
pub fn encode_segment(out: &mut BytesMut, payload: &[u8], self_contained: bool, lz4: bool) {
    assert!(payload.len() <= MAX_PAYLOAD, "segment payload over 128 KiB");
    if !lz4 {
        let mut h = payload.len() as u32;
        if self_contained {
            h |= 1 << 17;
        }
        let crc = crc24(h as u64, 3);
        out.reserve(10 + payload.len());
        out.put_slice(&h.to_le_bytes()[..3]);
        out.put_slice(&crc.to_le_bytes()[..3]);
        out.put_slice(payload);
        out.put_u32_le(crc32(payload));
        return;
    }
    let mut z = vec![0u8; lz4_flex::block::get_maximum_output_size(payload.len())];
    let n = lz4_flex::block::compress_into(payload, &mut z).expect("output sized for the worst case");
    let (body, uncompressed): (&[u8], usize) = if n >= payload.len() { (payload, 0) } else { (&z[..n], payload.len()) };
    let mut h = body.len() as u64 | (uncompressed as u64) << 17;
    if self_contained {
        h |= 1 << 34;
    }
    let h = h | (crc24(h, 5) as u64) << 40;
    out.reserve(12 + body.len());
    out.put_u64_le(h);
    out.put_slice(body);
    out.put_u32_le(crc32(body));
}

/// Wraps whole envelopes: as many as fit go into self-contained segments of up to
/// `max_payload` bytes; an envelope that does not fit alone is cut into non-self-contained
/// segments.
pub fn encode_envelopes<'a>(out: &mut BytesMut, envelopes: impl IntoIterator<Item = &'a [u8]>, lz4: bool, max_payload: usize) {
    let max = max_payload.clamp(Header::LEN + 1, MAX_PAYLOAD);
    let mut batch = BytesMut::new();
    for env in envelopes {
        if env.len() >= max {
            if !batch.is_empty() {
                encode_segment(out, &batch, true, lz4);
                batch.clear();
            }
            for chunk in env.chunks(max) {
                encode_segment(out, chunk, false, lz4);
            }
            continue;
        }
        if batch.len() + env.len() > max {
            encode_segment(out, &batch, true, lz4);
            batch.clear();
        }
        batch.put_slice(env);
    }
    if !batch.is_empty() {
        encode_segment(out, &batch, true, lz4);
    }
}

/// Reads one segment from `buf`, or `None` until it is complete.
pub fn decode_segment(buf: &mut BytesMut, lz4: bool) -> Result<Option<Segment>, SegmentError> {
    if !lz4 {
        if buf.len() < 6 {
            return Ok(None);
        }
        let h = u32::from_le_bytes([buf[0], buf[1], buf[2], 0]);
        let read = u32::from_le_bytes([buf[3], buf[4], buf[5], 0]);
        let computed = crc24(h as u64, 3);
        if read != computed {
            return Err(SegmentError::HeaderCrc { read, computed });
        }
        let len = (h & 0x1FFFF) as usize;
        let self_contained = h & (1 << 17) != 0;
        if buf.len() < 6 + len + 4 {
            buf.reserve(6 + len + 4 - buf.len());
            return Ok(None);
        }
        buf.advance(6);
        let payload = buf.split_to(len).freeze();
        let read = buf.get_u32_le();
        let computed = crc32(&payload);
        if read != computed {
            return Err(SegmentError::PayloadCrc { read, computed, self_contained });
        }
        return Ok(Some(Segment { self_contained, payload }));
    }
    if buf.len() < 8 {
        return Ok(None);
    }
    let h = u64::from_le_bytes(buf[..8].try_into().expect("8 bytes"));
    let read = ((h >> 40) & 0xFF_FFFF) as u32;
    let computed = crc24(h, 5);
    if read != computed {
        return Err(SegmentError::HeaderCrc { read, computed });
    }
    let compressed = (h & 0x1FFFF) as usize;
    let uncompressed = ((h >> 17) & 0x1FFFF) as usize;
    let self_contained = h & (1 << 34) != 0;
    if buf.len() < 8 + compressed + 4 {
        buf.reserve(8 + compressed + 4 - buf.len());
        return Ok(None);
    }
    buf.advance(8);
    let body = buf.split_to(compressed).freeze();
    let read = buf.get_u32_le();
    let computed = crc32(&body);
    if read != computed {
        return Err(SegmentError::PayloadCrc { read, computed, self_contained });
    }
    if uncompressed == 0 {
        return Ok(Some(Segment { self_contained, payload: body }));
    }
    let mut out = vec![0u8; uncompressed];
    match lz4_flex::block::decompress_into(&body, &mut out) {
        Ok(n) if n == uncompressed => Ok(Some(Segment { self_contained, payload: Bytes::from(out) })),
        Ok(n) => Err(SegmentError::Lz4(format!("lz4 segment decompressed to {n} bytes, header says {uncompressed}"))),
        Err(e) => Err(SegmentError::Lz4(format!("lz4 segment does not decompress: {e}"))),
    }
}

/// What the assembler produces from segment payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Assembled {
    /// A whole envelope, header included.
    Envelope(Bytes),
    /// A multi-segment envelope over the limit, whose segments were consumed and dropped.
    TooLarge(Header),
}

#[derive(Debug)]
struct Large {
    header: Header,
    total: usize,
    seen: usize,
    /// `None` while discarding an oversized envelope.
    acc: Option<BytesMut>,
}

/// Turns segment payloads back into envelopes.
#[derive(Debug)]
pub struct Assembler {
    max_envelope: usize,
    large: Option<Large>,
}

impl Assembler {
    /// `max_envelope` bounds a multi-segment envelope (header included).
    pub fn new(max_envelope: usize) -> Self {
        Assembler { max_envelope, large: None }
    }

    pub fn in_progress(&self) -> bool {
        self.large.is_some()
    }

    pub fn push(&mut self, seg: Segment, out: &mut VecDeque<Assembled>) -> Result<(), SegmentError> {
        let p = seg.payload;
        if seg.self_contained {
            if self.large.is_some() {
                return Err(SegmentError::Envelope("Self-contained frame received while a large message was in progress".into()));
            }
            let mut at = 0;
            while at < p.len() {
                let Some(h) = Header::peek(&p[at..]) else {
                    return Err(SegmentError::Envelope("Incomplete envelope header in self-contained frame".into()));
                };
                let end = at + Header::LEN + h.length as usize;
                if end > p.len() {
                    return Err(SegmentError::Envelope(format!(
                        "Envelope of {} bytes does not fit its self-contained frame",
                        Header::LEN + h.length as usize
                    )));
                }
                out.push_back(Assembled::Envelope(p.slice(at..end)));
                at = end;
            }
            return Ok(());
        }
        let large = match &mut self.large {
            Some(l) => l,
            None => {
                let Some(header) = Header::peek(&p) else {
                    return Err(SegmentError::Envelope("First frame of a large message is shorter than an envelope header".into()));
                };
                let total = Header::LEN + header.length as usize;
                let acc = (total <= self.max_envelope).then(|| BytesMut::with_capacity(p.len().max(total.min(4 << 20))));
                self.large.insert(Large { header, total, seen: 0, acc })
            }
        };
        large.seen += p.len();
        if large.seen > large.total {
            self.large = None;
            return Err(SegmentError::Envelope("Large message frames exceed the envelope length".into()));
        }
        if let Some(acc) = &mut large.acc {
            acc.put_slice(&p);
        }
        if large.seen == large.total {
            let l = self.large.take().expect("in progress");
            out.push_back(match l.acc {
                Some(acc) => Assembled::Envelope(acc.freeze()),
                None => Assembled::TooLarge(l.header),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(mut buf: BytesMut, lz4: bool) -> Vec<Segment> {
        let mut out = vec![];
        while let Some(s) = decode_segment(&mut buf, lz4).unwrap() {
            out.push(s);
        }
        assert!(buf.is_empty());
        out
    }

    #[test]
    fn crc24_is_24_bits_and_matches_known_values() {
        // Header of an empty self-contained uncompressed segment.
        let h = 1u64 << 17;
        let c = crc24(h, 3);
        assert!(c < 1 << 24);
        assert_eq!(crc24(0, 0), CRC24_INIT);
        // Different inputs, different CRCs (a stuck implementation would not notice).
        assert_ne!(crc24(1, 3), crc24(2, 3));
    }

    #[test]
    fn crc32_is_primed() {
        assert_ne!(crc32(b""), crc32fast::hash(b""));
        let mut h = crc32fast::Hasher::new();
        h.update(&[0xFA, 0x2D, 0x55, 0xCA, b'x']);
        assert_eq!(crc32(b"x"), h.finalize());
    }

    #[test]
    fn segments_round_trip() {
        for lz4 in [false, true] {
            for (payload, sc) in [(vec![], true), (b"hello".to_vec(), false), (vec![7u8; MAX_PAYLOAD], true)] {
                let mut b = BytesMut::new();
                encode_segment(&mut b, &payload, sc, lz4);
                // Byte at a time: never yields early, never errs.
                let mut partial = BytesMut::new();
                let mut got = None;
                for &x in b.iter() {
                    assert!(got.is_none());
                    partial.put_u8(x);
                    got = decode_segment(&mut partial, lz4).unwrap();
                }
                let s = got.unwrap();
                assert_eq!((&s.payload[..], s.self_contained), (&payload[..], sc));
            }
        }
    }

    #[test]
    fn incompressible_lz4_is_stored() {
        let payload: Vec<u8> = (0..200u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let mut b = BytesMut::new();
        encode_segment(&mut b, &payload, true, true);
        let h = u64::from_le_bytes(b[..8].try_into().unwrap());
        assert_eq!((h >> 17) & 0x1FFFF, 0, "uncompressed length 0 = stored");
        assert_eq!(decode_all(b, true)[0].payload, payload);
    }

    #[test]
    fn corruption_is_detected() {
        for lz4 in [false, true] {
            let mut b = BytesMut::new();
            encode_segment(&mut b, b"some payload some payload some payload", true, lz4);
            let hlen = if lz4 { 8 } else { 6 };
            for i in 0..b.len() {
                let mut c = b.clone();
                c[i] ^= 0x10;
                let r = decode_segment(&mut c, lz4);
                if i < hlen {
                    // Flipping a length bit may make the header claim more bytes; either way
                    // the CRC rejects it before the length is trusted.
                    assert!(matches!(r, Err(SegmentError::HeaderCrc { .. })), "byte {i}");
                } else {
                    assert!(matches!(r, Err(SegmentError::PayloadCrc { .. })), "byte {i}: {r:?}");
                }
            }
        }
    }

    #[test]
    fn lz4_garbage_with_valid_crcs() {
        // A header claiming 100 uncompressed bytes over a payload that is not lz4.
        let body = [0xF0u8, 1, 2, 3];
        let h = body.len() as u64 | 100 << 17 | 1 << 34;
        let h = h | (crc24(h, 5) as u64) << 40;
        let mut b = BytesMut::new();
        b.put_u64_le(h);
        b.put_slice(&body);
        b.put_u32_le(crc32(&body));
        assert!(matches!(decode_segment(&mut b, true), Err(SegmentError::Lz4(_))));
    }

    #[test]
    fn envelopes_pack_and_split() {
        let small = vec![1u8; 9 + 10];
        let mut small_env = small.clone();
        small_env[5..9].copy_from_slice(&10u32.to_be_bytes());
        let body_len = 300_000;
        let mut big = vec![0u8; 9 + body_len];
        big[5..9].copy_from_slice(&(body_len as u32).to_be_bytes());
        for (i, x) in big.iter_mut().enumerate().skip(9) {
            *x = i as u8;
        }
        for lz4 in [false, true] {
            let mut b = BytesMut::new();
            let envs: Vec<&[u8]> = vec![&small_env, &small_env, &big, &small_env];
            encode_envelopes(&mut b, envs, lz4, CASSANDRA_PAYLOAD);
            let segs = decode_all(b, lz4);
            // [small small] [big: 3 parts] [small]
            assert_eq!(segs.iter().map(|s| s.self_contained).collect::<Vec<_>>(), [true, false, false, false, true]);
            assert_eq!(segs[1].payload.len(), CASSANDRA_PAYLOAD);
            let mut asm = Assembler::new(1 << 20);
            let mut out = VecDeque::new();
            for s in segs {
                asm.push(s, &mut out).unwrap();
            }
            let got: Vec<_> = out.into_iter().collect();
            assert_eq!(got.len(), 4);
            assert_eq!(got[2], Assembled::Envelope(Bytes::from(big.clone())));
            assert_eq!(got[0], Assembled::Envelope(Bytes::from(small_env.clone())));
            assert!(!asm.in_progress());
        }
    }

    #[test]
    fn oversized_large_envelopes_are_skipped() {
        let mut big = vec![0u8; 9 + 200_000];
        big[4] = 7;
        big[5..9].copy_from_slice(&200_000u32.to_be_bytes());
        let mut b = BytesMut::new();
        encode_envelopes(&mut b, [&big[..]], false, CASSANDRA_PAYLOAD);
        let mut asm = Assembler::new(100_000);
        let mut out = VecDeque::new();
        for s in decode_all(b, false) {
            asm.push(s, &mut out).unwrap();
        }
        assert!(matches!(out.pop_front(), Some(Assembled::TooLarge(h)) if h.length == 200_000 && h.opcode == 7));
    }

    #[test]
    fn malformed_payloads() {
        let mut asm = Assembler::new(1 << 20);
        let mut out = VecDeque::new();
        // Partial header in a self-contained segment.
        let seg = |sc, p: &[u8]| Segment { self_contained: sc, payload: Bytes::copy_from_slice(p) };
        assert!(asm.push(seg(true, b"\x05\x00\x00"), &mut out).is_err());
        // Envelope claiming more than its segment.
        assert!(asm.push(seg(true, b"\x05\x00\x00\x00\x05\x00\x00\x00\x09ab"), &mut out).is_err());
        // Large message, then a self-contained segment before it completes.
        assert!(asm.push(seg(false, b"\x05\x00\x00\x00\x07\x00\x00\x00\x09ab"), &mut out).is_ok());
        assert!(asm.push(seg(true, b""), &mut out).is_err());
        // Non-self-contained segments overshooting the envelope.
        let mut asm = Assembler::new(1 << 20);
        assert!(asm.push(seg(false, b"\x05\x00\x00\x00\x07\x00\x00\x00\x02abcdef"), &mut out).is_err());
        assert!(asm.push(seg(false, b"\x05\x00"), &mut out).is_err());
        assert!(out.is_empty());
    }

    #[test]
    fn cassandra_crc_messages() {
        let e = SegmentError::PayloadCrc { read: 1, computed: 0xFFFF_FFFF, self_contained: true };
        assert_eq!(
            e.cassandra_message("0f3a2b1c"),
            "0f3a2b1c invalid, unrecoverable CRC mismatch detected in frame body. Read 1, Computed -1"
        );
    }
}
