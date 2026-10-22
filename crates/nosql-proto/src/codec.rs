//! `tokio_util::codec` framing for both ends of a connection.
//!
//! **Server.** [`ServerDecoder`] yields [`Inbound`] items: a validated [`Frame`], or a
//! [`Rejection`] — the ERROR the reference answers a bad envelope with (unsupported version,
//! missing USE_BETA, unknown opcode, oversized frame, corrupt segment…), with the stream id
//! and version byte it uses and whether it then closes the connection. Rejections are items,
//! not `Err`s, because most of them leave the connection usable; `Err` is only I/O.
//! Message bodies are decoded by the caller ([`crate::RequestFrame::decode`]).
//!
//! The caller drives the connection's phase, as the reference does:
//!
//! 1. Before STARTUP the decoder accepts OPTIONS and STARTUP only, in the version the client
//!    picked (checked against the dialect's versions).
//! 2. On STARTUP, call [`ServerDecoder::established`] with its version (Cassandra pins the
//!    connection before it even validates STARTUP), then, if STARTUP negotiated compression,
//!    [`ServerEncoder::set_compression`] *before* encoding the answer (READY is compressed on
//!    v3/v4) and [`ServerDecoder::set_compression`].
//! 3. On v5+, after encoding the answer to STARTUP (READY, AUTHENTICATE or an ERROR), call
//!    `start_segments(lz4)` on both halves: from then on everything is wrapped in segments.
//!
//! [`ServerCodec`] bundles both halves for `Framed`; with split I/O use the halves with
//! `FramedRead` / `FramedWrite`.
//!
//! **Client.** [`ClientCodec`] encodes requests and yields response frames; the client switches
//! it to compression or segments after STARTUP is answered.
//!
//! The error texts and reply conventions per dialect were captured from the reference images
//! by sending raw bytes; Cassandra's follow `org.apache.cassandra.transport.Envelope.Decoder`,
//! `ProtocolVersion`, `InitialConnectionHandler`, `PreV5Handlers` and `CQLMessageHandler`.

use std::collections::VecDeque;
use std::io;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::compress::Compression;
use crate::error::DecodeError;
use crate::error_body::ErrorBody;
use crate::frame::{Flags, Frame, Header, Opcode};
use crate::segment::{self, Assembled, Assembler, SegmentError};
use crate::version::{Dialect, Direction, Version};

/// The most the decoders reserve ahead for a frame still arriving: a header claiming a huge
/// body costs nothing until the bytes come.
const READ_RESERVE: usize = 64 * 1024;

/// Size limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Cassandra: the largest unframed (v3/v4) envelope, header included
    /// (`native_transport_max_frame_size`). ScyllaDB: the request memory budget an envelope's
    /// estimate (`2 × length + 8000`) must fit.
    pub max_frame_size: usize,
    /// The largest v5 message body (`native_transport_max_message_size`).
    pub max_message_size: usize,
    /// ScyllaDB's cap on a body before STARTUP.
    pub max_initial_body: Option<usize>,
    /// Payload bytes per outgoing v5 segment.
    pub segment_payload: usize,
}

impl Default for Limits {
    /// 256 MiB everywhere.
    fn default() -> Self {
        Limits {
            max_frame_size: 256 << 20,
            max_message_size: 256 << 20,
            max_initial_body: None,
            segment_payload: segment::CASSANDRA_PAYLOAD,
        }
    }
}

impl Limits {
    /// The reference images' defaults: Cassandra 16 MiB frames and messages (1 GiB heap);
    /// ScyllaDB 100000-byte initial messages and the 102131302-byte budget of `--memory 1G`.
    pub fn of(dialect: Dialect) -> Self {
        match dialect {
            Dialect::Cassandra => Limits { max_frame_size: 16 << 20, max_message_size: 16 << 20, ..Default::default() },
            Dialect::Scylla => Limits { max_frame_size: 102_131_302, max_initial_body: Some(100_000), ..Default::default() },
        }
    }
}

/// An envelope the server refuses, and how to answer it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejection {
    pub error: ErrorBody,
    /// `None`: close without answering (no stream id could be recovered).
    pub stream: Option<i16>,
    /// The version number of the reply's version byte (1 and 2 get the 8-byte v1/v2 header).
    pub version: u8,
    pub flags: Flags,
    /// Close the connection after sending the reply.
    pub close: bool,
}

impl Rejection {
    fn new(error: ErrorBody, stream: i16, version: u8, close: bool) -> Self {
        Rejection { error, stream: Some(stream), version, flags: Flags::default(), close }
    }

    fn silent() -> Self {
        Rejection { error: ErrorBody::protocol(""), stream: None, version: 0, flags: Flags::default(), close: true }
    }

    /// The reply as a frame (`None` when unanswerable or for v1/v2 headers).
    pub fn to_frame(&self) -> Option<Frame> {
        let stream = self.stream?;
        let version = Version::from_u8(self.version)?;
        let mut body = BytesMut::new();
        self.error.encode(version, &mut body);
        Some(Frame { version, direction: Direction::Response, flags: self.flags, stream, opcode: Opcode::Error, body: body.freeze() })
    }

    /// The reply as unframed bytes, with the v1/v2 short header where needed.
    pub fn to_bytes(&self) -> Option<Bytes> {
        if let Some(f) = self.to_frame() {
            return Some(f.to_bytes());
        }
        let stream = self.stream?;
        let mut body = BytesMut::new();
        self.error.encode(Version::V3, &mut body);
        let mut out = BytesMut::with_capacity(8 + body.len());
        out.put_u8(self.version | 0x80);
        out.put_u8(self.flags.0);
        out.put_u8(stream as u8);
        out.put_u8(Opcode::Error as u8);
        out.put_u32(body.len() as u32);
        out.put_slice(&body);
        Some(out.freeze())
    }
}

/// What the server decoder produces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    Frame(Frame),
    Reject(Rejection),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Before STARTUP: OPTIONS and STARTUP only.
    Initial,
    Established(Version),
}

#[derive(Debug)]
struct Segments {
    lz4: bool,
    asm: Assembler,
}

#[derive(Debug)]
struct Discard {
    left: usize,
    then: Rejection,
}

/// Cassandra's unsupported-version text, listing the dialect's versions.
pub fn invalid_version_message(dialect: Dialect, version: u8) -> String {
    match dialect {
        Dialect::Cassandra => {
            let list: Vec<String> = dialect.versions().iter().map(|v| v.to_string()).collect();
            format!("Invalid or unsupported protocol version ({version}); supported versions are ({})", list.join(", "))
        }
        Dialect::Scylla => format!("Invalid or unsupported protocol version: {version}"),
    }
}

/// Decodes requests for a server.
#[derive(Debug)]
pub struct ServerDecoder {
    dialect: Dialect,
    limits: Limits,
    phase: Phase,
    compression: Option<Compression>,
    segments: Option<Segments>,
    pending: VecDeque<Inbound>,
    discard: Option<Discard>,
    conn_id: String,
    closed: bool,
}

impl ServerDecoder {
    pub fn new(dialect: Dialect) -> Self {
        Self::with_limits(dialect, Limits::of(dialect))
    }

    pub fn with_limits(dialect: Dialect, limits: Limits) -> Self {
        ServerDecoder {
            dialect,
            limits,
            phase: Phase::Initial,
            compression: None,
            segments: None,
            pending: VecDeque::new(),
            discard: None,
            conn_id: "00000000".into(),
            closed: false,
        }
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Pins the connection's version (on STARTUP).
    pub fn established(&mut self, version: Version) {
        self.phase = Phase::Established(version);
    }

    /// v3/v4 body compression negotiated by STARTUP.
    pub fn set_compression(&mut self, c: Option<Compression>) {
        self.compression = c;
    }

    /// Switches to v5 segments (after the answer to STARTUP).
    pub fn start_segments(&mut self, lz4: bool) {
        self.segments = Some(Segments { lz4, asm: Assembler::new(self.limits.max_message_size.saturating_add(Header::LEN)) });
    }

    pub fn segments(&self) -> bool {
        self.segments.is_some()
    }

    /// The channel id Cassandra prints in CRC errors (eight hex digits).
    pub fn set_connection_id(&mut self, id: impl Into<String>) {
        self.conn_id = id.into();
    }

    fn reply_version(&self) -> u8 {
        match self.phase {
            Phase::Established(v) => v.as_u8(),
            Phase::Initial => self.dialect.current().as_u8(),
        }
    }

    fn reject(&mut self, r: Rejection) -> Inbound {
        if r.close {
            self.closed = true;
        }
        Inbound::Reject(r)
    }

    /// Skips the `total` bytes of the envelope at the front of `buf`, then rejects it.
    fn skip(&mut self, buf: &mut BytesMut, total: usize, r: Rejection) -> Option<Inbound> {
        let n = total.min(buf.len());
        buf.advance(n);
        if n == total {
            return Some(self.reject(r));
        }
        self.discard = Some(Discard { left: total - n, then: r });
        None
    }

    fn decode_cassandra(&mut self, buf: &mut BytesMut) -> Option<Inbound> {
        let first = *buf.first()?;
        let vnum = first & 0x7f;
        let dir = Direction::of(first);
        let Some(version) = Version::from_u8(vnum).filter(|v| self.dialect.supports(*v)) else {
            if buf.len() < Header::LEN {
                buf.clear();
                return Some(self.reject(Rejection::silent()));
            }
            // v1/v2 headers have a one-byte stream id; Cassandra reads it the same way for 0.
            let stream = if vnum < 3 { buf[2] as i8 as i16 } else { i16::from_be_bytes([buf[2], buf[3]]) };
            let reply = if vnum == 1 || vnum == 2 { vnum } else { self.reply_version() };
            let msg = invalid_version_message(self.dialect, vnum);
            buf.clear();
            return Some(self.reject(Rejection::new(ErrorBody::protocol(msg), stream, reply, false)));
        };
        let h = Header::peek(buf)?;
        let total = Header::LEN + h.length as usize;
        if version.is_beta() && !h.flags.has(Flags::USE_BETA) {
            let msg = format!("Beta version of the protocol used ({version}), but USE_BETA flag is unset");
            let mut r = Rejection::new(ErrorBody::protocol(msg), h.stream, version.as_u8(), false);
            r.flags = Flags(Flags::USE_BETA);
            return self.skip(buf, total, r);
        }
        let opcode = match Opcode::from_u8(h.opcode) {
            None => {
                let r = Rejection::new(DecodeError::UnknownOpcode(h.opcode).to_error(self.dialect), h.stream, self.reply_version(), false);
                return self.skip(buf, total, r);
            }
            Some(op) if op.direction() != dir => {
                let e = DecodeError::WrongDirection { opcode: h.opcode, got: dir }.to_error(self.dialect);
                let r = Rejection::new(e, h.stream, self.reply_version(), false);
                return self.skip(buf, total, r);
            }
            Some(op) => op,
        };
        if total > self.limits.max_frame_size {
            let msg = format!("Request is too big: length {total} exceeds maximum allowed length {}.", self.limits.max_frame_size);
            let r = Rejection::new(ErrorBody::invalid(msg), h.stream, self.reply_version(), false);
            return self.skip(buf, total, r);
        }
        if buf.len() < total {
            buf.reserve((total - buf.len()).min(READ_RESERVE));
            return None;
        }
        let mut env = buf.split_to(total);
        env.advance(Header::LEN);
        match self.phase {
            Phase::Initial if !matches!(opcode, Opcode::Options | Opcode::Startup) => {
                let msg = format!("Unexpected message {}, expecting STARTUP or OPTIONS", opcode.name());
                return Some(self.reject(Rejection::new(ErrorBody::protocol(msg), h.stream, vnum, true)));
            }
            Phase::Established(cv) if cv != version => {
                let msg = format!("Invalid message version. Got {version} but previous messages on this connection had version {cv}");
                return Some(self.reject(Rejection::new(ErrorBody::protocol(msg), h.stream, cv.as_u8(), false)));
            }
            _ => {}
        }
        let mut body = env.freeze();
        let mut flags = h.flags;
        if let (true, Some(c)) = (flags.has(Flags::COMPRESSION), self.compression) {
            match c.decompress(&body, self.limits.max_frame_size) {
                Ok(b) => body = b,
                Err(_) => return Some(self.reject(Rejection::silent())),
            }
            flags.set(Flags::COMPRESSION, false);
        }
        Some(Inbound::Frame(Frame { version, direction: dir, flags, stream: h.stream, opcode, body }))
    }

    fn decode_scylla(&mut self, buf: &mut BytesMut) -> Option<Inbound> {
        let first = *buf.first()?;
        let version = match self.phase {
            Phase::Initial => match Version::from_u8(first).filter(|v| self.dialect.supports(*v)) {
                Some(v) => v,
                None => {
                    buf.clear();
                    let msg = invalid_version_message(self.dialect, first);
                    return Some(self.reject(Rejection::new(ErrorBody::protocol(msg), 0, self.dialect.current().as_u8(), true)));
                }
            },
            Phase::Established(cv) => {
                if first != cv.as_u8() {
                    buf.clear();
                    let msg = format!(
                        "Invalid message version. Got {first} but previous messages on this connection had version {}",
                        cv.as_u8()
                    );
                    return Some(self.reject(Rejection::new(ErrorBody::protocol(msg), 0, cv.as_u8(), true)));
                }
                cv
            }
        };
        let h = Header::peek(buf)?;
        let len = h.length as usize;
        match (self.phase, self.limits.max_initial_body) {
            (Phase::Initial, Some(max)) if len > max => {
                buf.clear();
                let msg = format!("Initial message size too large ({len}), rejecting as invalid");
                return Some(self.reject(Rejection::new(ErrorBody::protocol(msg), 0, version.as_u8(), true)));
            }
            (Phase::Established(_), _) => {
                let estimate = 2 * len as u64 + 8000;
                if estimate > self.limits.max_frame_size as u64 {
                    buf.clear();
                    let msg = format!(
                        "request size too large (frame size {len}; estimate {estimate}; allowed {})",
                        self.limits.max_frame_size
                    );
                    return Some(self.reject(Rejection::new(ErrorBody::invalid(msg), h.stream, version.as_u8(), true)));
                }
            }
            _ => {}
        }
        let total = Header::LEN + len;
        if buf.len() < total {
            buf.reserve((total - buf.len()).min(READ_RESERVE));
            return None;
        }
        let mut env = buf.split_to(total);
        env.advance(Header::LEN);
        let opcode = Opcode::from_u8(h.opcode);
        match self.phase {
            Phase::Initial if !matches!(opcode, Some(Opcode::Options | Opcode::Startup)) => {
                let msg = format!("Unexpected message {}, expecting STARTUP or OPTIONS", h.opcode);
                return Some(self.reject(Rejection::new(ErrorBody::protocol(msg), h.stream, version.as_u8(), false)));
            }
            _ => {}
        }
        let opcode = match opcode {
            Some(op) if op.direction() == Direction::Request && op != Opcode::Credentials => op,
            _ => {
                let r = Rejection::new(DecodeError::UnknownOpcode(h.opcode).to_error(self.dialect), h.stream, version.as_u8(), false);
                return Some(self.reject(r));
            }
        };
        let mut body = env.freeze();
        let mut flags = h.flags;
        if flags.has(Flags::COMPRESSION) {
            let unknown = || Rejection::new(ErrorBody::protocol("Unknown compression algorithm"), 0, version.as_u8(), true);
            let Some(c) = self.compression else { return Some(self.reject(unknown())) };
            match c.decompress(&body, self.limits.max_frame_size) {
                Ok(b) => body = b,
                Err(e) => {
                    let r = Rejection::new(ErrorBody::protocol(e.to_string()), 0, version.as_u8(), true);
                    return Some(self.reject(r));
                }
            }
            flags.set(Flags::COMPRESSION, false);
        }
        Some(Inbound::Frame(Frame { version, direction: Direction::Request, flags, stream: h.stream, opcode, body }))
    }

    /// An envelope out of a v5 segment.
    fn segment_envelope(&self, env: Bytes) -> Inbound {
        let h = Header::peek(&env).expect("assembled envelopes have a header");
        let cv = match self.phase {
            Phase::Established(v) => v,
            Phase::Initial => self.dialect.current(),
        };
        let reject = |e: ErrorBody, close| Inbound::Reject(Rejection::new(e, h.stream, cv.as_u8(), close));
        let vnum = h.version & 0x7f;
        let dir = Direction::of(h.version);
        let Some(version) = Version::from_u8(vnum).filter(|v| self.dialect.supports(*v)) else {
            return reject(ErrorBody::protocol(invalid_version_message(self.dialect, vnum)), false);
        };
        if version.is_beta() && !h.flags.has(Flags::USE_BETA) {
            let msg = format!("Beta version of the protocol used ({version}), but USE_BETA flag is unset");
            return reject(ErrorBody::protocol(msg), false);
        }
        let opcode = match Opcode::from_u8(h.opcode) {
            None => return reject(DecodeError::UnknownOpcode(h.opcode).to_error(self.dialect), false),
            Some(op) if op.direction() != dir => {
                return reject(DecodeError::WrongDirection { opcode: h.opcode, got: dir }.to_error(self.dialect), false);
            }
            Some(op) => op,
        };
        if version != cv {
            // Sic: Cassandra's v5 handler has no space in "previousmessages".
            let msg = format!("Invalid message version. Got {version} but previousmessages on this connection had version {cv}");
            return reject(ErrorBody::protocol(msg), false);
        }
        Inbound::Frame(Frame { version, direction: dir, flags: h.flags, stream: h.stream, opcode, body: env.slice(Header::LEN..) })
    }

    fn segment_error(&mut self, e: SegmentError) -> Inbound {
        let cv = match self.phase {
            Phase::Established(v) => v.as_u8(),
            Phase::Initial => self.dialect.current().as_u8(),
        };
        let r = Rejection::new(ErrorBody::protocol(e.cassandra_message(&self.conn_id)), 0, cv, true);
        self.reject(r)
    }
}

impl Decoder for ServerDecoder {
    type Item = Inbound;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Inbound>, io::Error> {
        loop {
            if let Some(x) = self.pending.pop_front() {
                return Ok(Some(x));
            }
            if self.closed {
                buf.clear();
                return Ok(None);
            }
            if let Some(d) = &mut self.discard {
                let n = d.left.min(buf.len());
                buf.advance(n);
                d.left -= n;
                if d.left > 0 {
                    return Ok(None);
                }
                let r = self.discard.take().expect("discarding").then;
                return Ok(Some(self.reject(r)));
            }
            let Some(seg) = &mut self.segments else {
                return Ok(match self.dialect {
                    Dialect::Cassandra => self.decode_cassandra(buf),
                    Dialect::Scylla => self.decode_scylla(buf),
                });
            };
            let segment = match segment::decode_segment(buf, seg.lz4) {
                Ok(Some(s)) => s,
                Ok(None) => return Ok(None),
                Err(e) => return Ok(Some(self.segment_error(e))),
            };
            let mut out = VecDeque::new();
            if let Err(e) = seg.asm.push(segment, &mut out) {
                return Ok(Some(self.segment_error(e)));
            }
            for a in out {
                let item = match a {
                    Assembled::Envelope(env) => self.segment_envelope(env),
                    Assembled::TooLarge(h) => {
                        let msg = format!(
                            "CQL Message of size {} bytes exceeds allowed maximum of {} bytes",
                            h.length, self.limits.max_message_size
                        );
                        Inbound::Reject(Rejection::new(ErrorBody::invalid(msg), h.stream, self.reply_version(), false))
                    }
                };
                self.pending.push_back(item);
            }
        }
    }
}

/// Encodes responses for a server.
#[derive(Debug, Default)]
pub struct ServerEncoder {
    compression: Option<Compression>,
    segments: Option<bool>,
    segment_payload: usize,
}

impl ServerEncoder {
    pub fn new() -> Self {
        ServerEncoder { compression: None, segments: None, segment_payload: segment::CASSANDRA_PAYLOAD }
    }

    pub fn with_limits(limits: Limits) -> Self {
        ServerEncoder { segment_payload: limits.segment_payload, ..Self::new() }
    }

    pub fn set_compression(&mut self, c: Option<Compression>) {
        self.compression = c;
    }

    pub fn start_segments(&mut self, lz4: bool) {
        self.segments = Some(lz4);
    }
}

/// Writes envelopes unframed (compressing bodies, never STARTUP's) or in segments.
fn put_frames<'a>(frames: impl IntoIterator<Item = &'a Frame>, compression: Option<Compression>, segments: Option<bool>, payload: usize, dst: &mut BytesMut) {
    match segments {
        Some(lz4) => {
            let envs: Vec<Bytes> = frames.into_iter().map(Frame::to_bytes).collect();
            segment::encode_envelopes(dst, envs.iter().map(|b| &b[..]), lz4, payload);
        }
        None => {
            for f in frames {
                match compression {
                    Some(c) if f.opcode != Opcode::Startup => {
                        let body = c.compress(&f.body);
                        let mut h = f.header();
                        h.flags.set(Flags::COMPRESSION, true);
                        h.length = body.len() as u32;
                        dst.reserve(Header::LEN + body.len());
                        h.write(dst);
                        dst.put_slice(&body);
                    }
                    _ => f.encode(dst),
                }
            }
        }
    }
}

impl Encoder<Frame> for ServerEncoder {
    type Error = io::Error;

    fn encode(&mut self, f: Frame, dst: &mut BytesMut) -> io::Result<()> {
        put_frames([&f], self.compression, self.segments, self.segment_payload, dst);
        Ok(())
    }
}

/// Several responses at once: on v5 they share segments, as Cassandra's flusher packs them.
impl Encoder<Vec<Frame>> for ServerEncoder {
    type Error = io::Error;

    fn encode(&mut self, fs: Vec<Frame>, dst: &mut BytesMut) -> io::Result<()> {
        put_frames(&fs, self.compression, self.segments, self.segment_payload, dst);
        Ok(())
    }
}

impl Encoder<Rejection> for ServerEncoder {
    type Error = io::Error;

    fn encode(&mut self, r: Rejection, dst: &mut BytesMut) -> io::Result<()> {
        match r.to_frame() {
            Some(f) => self.encode(f, dst),
            None => {
                if let Some(b) = r.to_bytes() {
                    dst.put_slice(&b);
                }
                Ok(())
            }
        }
    }
}

/// Both server halves, for `Framed`.
#[derive(Debug)]
pub struct ServerCodec {
    pub decoder: ServerDecoder,
    pub encoder: ServerEncoder,
}

impl ServerCodec {
    pub fn new(dialect: Dialect) -> Self {
        Self::with_limits(dialect, Limits::of(dialect))
    }

    pub fn with_limits(dialect: Dialect, limits: Limits) -> Self {
        ServerCodec { decoder: ServerDecoder::with_limits(dialect, limits), encoder: ServerEncoder::with_limits(limits) }
    }

    pub fn established(&mut self, version: Version, compression: Option<Compression>) {
        self.decoder.established(version);
        if !version.uses_segments() {
            self.decoder.set_compression(compression);
            self.encoder.set_compression(compression);
        }
    }

    pub fn start_segments(&mut self, lz4: bool) {
        self.decoder.start_segments(lz4);
        self.encoder.start_segments(lz4);
    }
}

impl Decoder for ServerCodec {
    type Item = Inbound;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> io::Result<Option<Inbound>> {
        self.decoder.decode(buf)
    }
}

impl<T> Encoder<T> for ServerCodec
where
    ServerEncoder: Encoder<T, Error = io::Error>,
{
    type Error = io::Error;

    fn encode(&mut self, item: T, dst: &mut BytesMut) -> io::Result<()> {
        self.encoder.encode(item, dst)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("malformed frame: {0}")]
    Decode(#[from] DecodeError),
    #[error("corrupt segment: {0}")]
    Segment(#[from] SegmentError),
    #[error("frame of {0} bytes exceeds the limit")]
    TooLarge(usize),
}

/// A client's framing: encodes requests, decodes responses.
#[derive(Debug)]
pub struct ClientCodec {
    max_frame_size: usize,
    compression: Option<Compression>,
    segments: Option<Segments>,
    segment_payload: usize,
    pending: VecDeque<Frame>,
}

impl Default for ClientCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientCodec {
    pub fn new() -> Self {
        ClientCodec {
            max_frame_size: 256 << 20,
            compression: None,
            segments: None,
            segment_payload: segment::CASSANDRA_PAYLOAD,
            pending: VecDeque::new(),
        }
    }

    pub fn with_max_frame_size(mut self, n: usize) -> Self {
        self.max_frame_size = n;
        self
    }

    pub fn set_compression(&mut self, c: Option<Compression>) {
        self.compression = c;
    }

    pub fn start_segments(&mut self, lz4: bool) {
        self.segments = Some(Segments { lz4, asm: Assembler::new(self.max_frame_size) });
    }

    pub fn segments(&self) -> bool {
        self.segments.is_some()
    }

    fn parse(env: &Bytes) -> Result<(Header, Version, Opcode), CodecError> {
        let h = Header::peek(env).ok_or_else(|| DecodeError::invalid("short envelope"))?;
        let version = Version::from_u8(h.version & 0x7f)
            .ok_or_else(|| DecodeError::invalid(format!("unsupported response version {}", h.version & 0x7f)))?;
        let opcode = Opcode::from_u8(h.opcode).ok_or(DecodeError::UnknownOpcode(h.opcode))?;
        Ok((h, version, opcode))
    }
}

impl Decoder for ClientCodec {
    type Item = Frame;
    type Error = CodecError;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Frame>, CodecError> {
        loop {
            if let Some(f) = self.pending.pop_front() {
                return Ok(Some(f));
            }
            if let Some(seg) = &mut self.segments {
                let Some(s) = segment::decode_segment(buf, seg.lz4)? else { return Ok(None) };
                let mut out = VecDeque::new();
                seg.asm.push(s, &mut out)?;
                for a in out {
                    let env = match a {
                        Assembled::Envelope(e) => e,
                        Assembled::TooLarge(h) => return Err(CodecError::TooLarge(Header::LEN + h.length as usize)),
                    };
                    let (h, version, opcode) = Self::parse(&env)?;
                    self.pending.push_back(Frame {
                        version,
                        direction: Direction::of(h.version),
                        flags: h.flags,
                        stream: h.stream,
                        opcode,
                        body: env.slice(Header::LEN..),
                    });
                }
                continue;
            }
            let Some(h) = Header::peek(buf) else { return Ok(None) };
            let total = Header::LEN + h.length as usize;
            if total > self.max_frame_size {
                return Err(CodecError::TooLarge(total));
            }
            if buf.len() < total {
                buf.reserve((total - buf.len()).min(READ_RESERVE));
                return Ok(None);
            }
            let env = buf.split_to(total).freeze();
            let (h, version, opcode) = Self::parse(&env)?;
            let mut body = env.slice(Header::LEN..);
            let mut flags = h.flags;
            if let (true, Some(c)) = (flags.has(Flags::COMPRESSION), self.compression) {
                body = c.decompress(&body, self.max_frame_size)?;
                flags.set(Flags::COMPRESSION, false);
            }
            return Ok(Some(Frame { version, direction: Direction::of(h.version), flags, stream: h.stream, opcode, body }));
        }
    }
}

impl Encoder<Frame> for ClientCodec {
    type Error = CodecError;

    fn encode(&mut self, f: Frame, dst: &mut BytesMut) -> Result<(), CodecError> {
        put_frames([&f], self.compression, self.segments.as_ref().map(|s| s.lz4), self.segment_payload, dst);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
