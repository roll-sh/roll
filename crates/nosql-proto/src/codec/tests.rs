//! Every case here was observed on the reference images by sending raw bytes; the expected
//! replies are the bytes they sent back.

use super::*;
use crate::request::{Query, QueryParams, Request, RequestFrame, Startup};
use crate::response::{Response, ResponseFrame};

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn frame(version: u8, flags: u8, stream: i16, opcode: u8, body: &[u8]) -> BytesMut {
    let mut b = BytesMut::new();
    Header { version, flags: Flags(flags), stream, opcode, length: body.len() as u32 }.write(&mut b);
    b.put_slice(body);
    b
}

fn startup_body() -> Vec<u8> {
    let mut b = BytesMut::new();
    Request::Startup(Startup::new("3.0.0")).encode(Version::V4, &mut b);
    b.to_vec()
}

fn one(d: &mut ServerDecoder, mut b: BytesMut) -> Option<Inbound> {
    d.decode(&mut b).unwrap()
}

fn rejection(d: &mut ServerDecoder, b: BytesMut) -> Rejection {
    match one(d, b) {
        Some(Inbound::Reject(r)) => r,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

fn accepted(d: &mut ServerDecoder, b: BytesMut) -> Frame {
    match one(d, b) {
        Some(Inbound::Frame(f)) => f,
        other => panic!("expected a frame, got {other:?}"),
    }
}

fn reply(r: &Rejection) -> String {
    let b = r.to_bytes().unwrap();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn established(dialect: Dialect, v: Version) -> ServerDecoder {
    let mut d = ServerDecoder::new(dialect);
    d.established(v);
    d
}

// ---- Cassandra 5.0.9 ----

#[test]
fn cassandra_accepts_its_versions() {
    for v in [3, 4, 5] {
        let f = accepted(&mut ServerDecoder::new(Dialect::Cassandra), frame(v, 0, 0x0102, 5, b""));
        assert_eq!((f.version.as_u8(), f.opcode, f.stream), (v, Opcode::Options, 0x0102));
    }
    let f = accepted(&mut ServerDecoder::new(Dialect::Cassandra), frame(6, 0x10, 0x0102, 5, b""));
    assert_eq!(f.version, Version::V6);
}

#[test]
fn cassandra_beta_without_flag() {
    let r = rejection(&mut ServerDecoder::new(Dialect::Cassandra), frame(6, 0, 0x0102, 5, b""));
    assert!(!r.close);
    assert_eq!(
        reply(&r),
        "86100102000000004f0000000a0049426574612076657273696f6e206f66207468652070726f746f636f6c20757365642028362f76362d62657461292c20627574205553455f4245544120666c616720697320756e736574"
    );
}

#[test]
fn cassandra_unsupported_versions() {
    let msg = |n: &str| {
        format!("Invalid or unsupported protocol version ({n}); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta)")
    };
    for (v, stream) in [(7u8, 0x0102i16), (65, 0x0102), (66, 0x0102), (127, 0x0102), (0, 1)] {
        let r = rejection(&mut ServerDecoder::new(Dialect::Cassandra), frame(v, 0, 0x0102, 5, b""));
        assert_eq!((r.stream, r.version, r.close), (Some(stream), 5, false), "v{v}");
        assert_eq!(r.error, ErrorBody::protocol(msg(&v.to_string())));
    }
    assert_eq!(
        reply(&rejection(&mut ServerDecoder::new(Dialect::Cassandra), frame(7, 0, 0x0102, 5, b""))),
        "8500010200000000670000000a0061496e76616c6964206f7220756e737570706f727465642070726f746f636f6c2076657273696f6e202837293b20737570706f727465642076657273696f6e73206172652028332f76332c20342f76342c20352f76352c20362f76362d6265746129"
    );
    // v1/v2: answered with their own 8-byte header and the one-byte stream id.
    for v in [1u8, 2] {
        let mut b = BytesMut::new();
        b.put_slice(&[v, 0, 7, 5, 0, 0, 0, 0, 0]);
        let r = rejection(&mut ServerDecoder::new(Dialect::Cassandra), b);
        assert_eq!(&reply(&r)[..18], &format!("8{v}000700000000670000000a")[..18]);
        assert_eq!(r.to_bytes().unwrap().len(), 8 + 0x67);
        assert!(r.to_frame().is_none());
    }
    // Fewer than 9 bytes of an unsupported version: closed without a reply.
    let r = rejection(&mut ServerDecoder::new(Dialect::Cassandra), BytesMut::from(&[9u8, 0, 1, 2, 5][..]));
    assert_eq!((r.stream, r.close), (None, true));
    assert_eq!(r.to_bytes(), None);
}

#[test]
fn cassandra_opcode_errors() {
    let mut d = ServerDecoder::new(Dialect::Cassandra);
    let r = rejection(&mut d, frame(0x84, 0, 0x0102, 5, b""));
    assert_eq!(
        reply(&r),
        "8500010200000000560000000a005057726f6e672070726f746f636f6c20646972656374696f6e2028657870656374656420524551554553542c20676f7420524553504f4e53452920666f72206f70636f6465203520284f5054494f4e5329"
    );
    let r = rejection(&mut d, frame(4, 0, 0x0102, 0x20, b""));
    assert_eq!(reply(&r), "8500010200000000170000000a0011556e6b6e6f776e206f70636f6465203332");
    let r = rejection(&mut d, frame(4, 0, 0x0102, 2, b""));
    assert_eq!(r.error.message, "Wrong protocol direction (expected RESPONSE, got REQUEST) for opcode 2 (READY)");
    assert_eq!(r.version, 5);
    // Anything but OPTIONS/STARTUP before STARTUP: answered in the request's version, closed.
    let r = rejection(&mut ServerDecoder::new(Dialect::Cassandra), frame(4, 0, 0x0102, 4, b""));
    assert_eq!(
        reply(&r),
        "8400010200000000420000000a003c556e6578706563746564206d6573736167652043524544454e5449414c532c20657870656374696e672053544152545550206f72204f5054494f4e53"
    );
    assert!(r.close);
    let r = rejection(&mut ServerDecoder::new(Dialect::Cassandra), frame(4, 0, 0x0102, 7, b"\x00\x00\x00\x08select 1\x00\x01\x00"));
    assert_eq!(r.error.message, "Unexpected message QUERY, expecting STARTUP or OPTIONS");
    assert!(r.close);
    // Established: unknown opcodes answer in the connection's version.
    let mut d = established(Dialect::Cassandra, Version::V4);
    let r = rejection(&mut d, frame(4, 0, 9, 0x20, b""));
    assert_eq!(reply(&r), "8400000900000000170000000a0011556e6b6e6f776e206f70636f6465203332");
    // CREDENTIALS passes the codec; the message decoder refuses it ("Unsupported message").
    let f = accepted(&mut d, frame(4, 0, 9, 4, b""));
    let e = RequestFrame::decode(Version::V4, &f).unwrap_err();
    assert_eq!(e.to_error(Dialect::Cassandra), ErrorBody::protocol("Unsupported message"));
}

#[test]
fn cassandra_version_mismatch_after_startup() {
    let mut d = established(Dialect::Cassandra, Version::V4);
    let r = rejection(&mut d, frame(3, 0, 9, 5, b""));
    assert_eq!(r.error.message, "Invalid message version. Got 3/v3 but previous messages on this connection had version 4/v4");
    assert_eq!((r.stream, r.version, r.close), (Some(9), 4, false));
    let r = rejection(&mut d, frame(5, 0, 9, 5, b""));
    assert_eq!(r.error.message, "Invalid message version. Got 5/v5 but previous messages on this connection had version 4/v4");
    let r = rejection(&mut d, frame(7, 0, 9, 5, b""));
    assert_eq!((r.version, r.stream), (4, Some(9)));
    // Still usable.
    assert_eq!(accepted(&mut d, frame(4, 0, 10, 5, b"")).stream, 10);
}

#[test]
fn cassandra_oversized_frames_are_discarded_then_answered() {
    let limits = Limits { max_frame_size: 100, ..Limits::of(Dialect::Cassandra) };
    let mut d = ServerDecoder::with_limits(Dialect::Cassandra, limits);
    d.established(Version::V4);
    let mut b = frame(4, 0, 3, 7, &[0u8; 200]);
    let total = b.len();
    let mut first = b.split_to(50);
    assert_eq!(d.decode(&mut first).unwrap(), None);
    assert!(first.is_empty());
    let mut rest = b;
    let Some(Inbound::Reject(r)) = d.decode(&mut rest).unwrap() else { panic!() };
    assert_eq!(r.error, ErrorBody::invalid(format!("Request is too big: length {total} exceeds maximum allowed length 100.")));
    assert_eq!((r.stream, r.close), (Some(3), false));
    assert!(matches!(one(&mut d, frame(4, 0, 4, 5, b"")), Some(Inbound::Frame(_))));
}

#[test]
fn cassandra_compressed_requests() {
    for c in [Compression::Lz4, Compression::Snappy] {
        let mut d = established(Dialect::Cassandra, Version::V4);
        d.set_compression(Some(c));
        let mut cc = ClientCodec::new();
        cc.set_compression(Some(c));
        let req = RequestFrame::new(5, Request::Query(Query { query: "SELECT * FROM t".repeat(20), params: QueryParams::default() }));
        let mut b = BytesMut::new();
        cc.encode(req.to_frame(Version::V4), &mut b).unwrap();
        assert!(Flags(b[1]).has(Flags::COMPRESSION));
        let f = accepted(&mut d, b);
        assert_eq!(RequestFrame::decode(Version::V4, &f).unwrap(), req);
        // A body that does not decompress: closed without an answer.
        let r = rejection(&mut d, frame(4, 1, 6, 5, b"\x00\x00\x10\x00garbage"));
        assert_eq!((r.stream, r.close), (None, true));
    }
    // The compression flag without negotiated compression: the body is taken as is.
    let mut d = established(Dialect::Cassandra, Version::V4);
    assert_eq!(accepted(&mut d, frame(4, 1, 6, 5, b"raw")).body, &b"raw"[..]);
}

#[test]
fn closing_rejections_stop_the_decoder() {
    let mut d = ServerDecoder::new(Dialect::Cassandra);
    let mut b = frame(4, 0, 1, 7, b"");
    b.extend_from_slice(&frame(4, 0, 2, 5, b""));
    assert!(matches!(d.decode(&mut b).unwrap(), Some(Inbound::Reject(Rejection { close: true, .. }))));
    assert_eq!(d.decode(&mut b).unwrap(), None);
    assert!(b.is_empty());
}

// ---- ScyllaDB 2026.3 ----

#[test]
fn scylla_versions() {
    for v in [3, 4] {
        assert!(matches!(one(&mut ServerDecoder::new(Dialect::Scylla), frame(v, 0, 0x0102, 5, b"")), Some(Inbound::Frame(_))));
    }
    for v in [5u8, 6, 7, 0, 1, 2, 65, 66, 127, 0x84] {
        let r = rejection(&mut ServerDecoder::new(Dialect::Scylla), frame(v, 0x10, 0x0102, 5, b""));
        assert_eq!(r.error.message, format!("Invalid or unsupported protocol version: {v}"));
        assert_eq!((r.stream, r.version, r.close), (Some(0), 4, true));
    }
    let r = rejection(&mut ServerDecoder::new(Dialect::Scylla), frame(5, 0, 0x0102, 5, b""));
    assert_eq!(reply(&r), "8400000000000000300000000a002a496e76616c6964206f7220756e737570706f727465642070726f746f636f6c2076657273696f6e3a2035");
    // One byte is enough.
    let r = rejection(&mut ServerDecoder::new(Dialect::Scylla), BytesMut::from(&[9u8][..]));
    assert_eq!(r.error.message, "Invalid or unsupported protocol version: 9");
}

#[test]
fn scylla_initial_messages() {
    for (op, n) in [(4u8, "4"), (0x20, "32"), (2, "2"), (7, "7")] {
        let r = rejection(&mut ServerDecoder::new(Dialect::Scylla), frame(4, 0, 0x0102, op, b""));
        assert_eq!(r.error.message, format!("Unexpected message {n}, expecting STARTUP or OPTIONS"));
        assert_eq!((r.stream, r.version, r.close), (Some(0x0102), 4, false));
    }
    let r = rejection(&mut ServerDecoder::new(Dialect::Scylla), frame(4, 0, 0x0102, 4, b""));
    assert_eq!(reply(&r), "8400010200000000380000000a0032556e6578706563746564206d65737361676520342c20657870656374696e672053544152545550206f72204f5054494f4e53");
    // The initial message limit is 100000 bytes of body.
    let mut d = ServerDecoder::new(Dialect::Scylla);
    let mut h = BytesMut::new();
    Header { version: 4, flags: Flags(0), stream: 1, opcode: 5, length: 100_000 }.write(&mut h);
    assert_eq!(d.decode(&mut h).unwrap(), None);
    let mut h = BytesMut::new();
    Header { version: 4, flags: Flags(0), stream: 1, opcode: 5, length: 131_072 }.write(&mut h);
    let Some(Inbound::Reject(r)) = d.decode(&mut h).unwrap() else { panic!() };
    assert_eq!(reply(&r), "8400000000000000430000000a003d496e697469616c206d6573736167652073697a6520746f6f206c617267652028313331303732292c2072656a656374696e6720617320696e76616c6964");
}

#[test]
fn scylla_after_startup() {
    let mut d = established(Dialect::Scylla, Version::V4);
    for (op, n) in [(0x20u8, 32), (4, 4), (2, 2)] {
        let r = rejection(&mut d, frame(4, 0, 9, op, b""));
        assert_eq!(r.error.message, format!("Unknown opcode {n}"));
        assert_eq!((r.stream, r.close), (Some(9), false));
    }
    assert_eq!(reply(&rejection(&mut d, frame(4, 0, 9, 4, b""))), "8400000900000000160000000a0010556e6b6e6f776e206f70636f64652034");
    for (v, shown) in [(3u8, "3"), (5, "5"), (7, "7"), (0x84, "132")] {
        let r = rejection(&mut established(Dialect::Scylla, Version::V4), frame(v, 0, 9, 5, b""));
        assert_eq!(r.error.message, format!("Invalid message version. Got {shown} but previous messages on this connection had version 4"));
        assert_eq!((r.stream, r.close), (Some(0), true));
    }
    // Memory estimate: 2 × length + 8000 against the budget.
    let mut h = BytesMut::new();
    Header { version: 4, flags: Flags(0), stream: 1, opcode: 7, length: 268_435_456 }.write(&mut h);
    let r = rejection(&mut established(Dialect::Scylla, Version::V4), h);
    assert_eq!(
        reply(&r),
        "84000001000000005a000022000054726571756573742073697a6520746f6f206c6172676520286672616d652073697a65203236383433353435363b20657374696d617465203533363837383931323b20616c6c6f7765642031303231333133303229"
    );
    let mut h = BytesMut::new();
    Header { version: 4, flags: Flags(0), stream: 1, opcode: 7, length: 16_777_217 }.write(&mut h);
    assert_eq!(established(Dialect::Scylla, Version::V4).decode(&mut h).unwrap(), None);
    // The compression flag without compression.
    let r = rejection(&mut established(Dialect::Scylla, Version::V4), frame(4, 1, 9, 5, b""));
    assert_eq!(reply(&r), "8400000000000000230000000a001d556e6b6e6f776e20636f6d7072657373696f6e20616c676f726974686d");
    assert!(r.close);
    // Compressed and fine.
    let mut d = established(Dialect::Scylla, Version::V4);
    d.set_compression(Some(Compression::Lz4));
    let mut b = BytesMut::new();
    let mut cc = ClientCodec::new();
    cc.set_compression(Some(Compression::Lz4));
    cc.encode(RequestFrame::new(3, Request::Options).to_frame(Version::V4), &mut b).unwrap();
    assert_eq!(accepted(&mut d, b).opcode, Opcode::Options);
    // Bad compressed body: protocol error, closed.
    let r = rejection(&mut d, frame(4, 1, 9, 5, b"\x00\x00\x10\x00x"));
    assert!(r.close);
}

// ---- encoding ----

#[test]
fn ready_is_compressed_like_the_references() {
    let ready = ResponseFrame::new(0x0102, Response::Ready).to_frame(Version::V4);
    let mut e = ServerEncoder::new();
    let mut b = BytesMut::new();
    e.encode(ready.clone(), &mut b).unwrap();
    assert_eq!(&b[..], &unhex("840001020200000000")[..]);
    for (c, hex) in [(Compression::Lz4, "8401010202000000050000000000"), (Compression::Snappy, "84010102020000000100")] {
        e.set_compression(Some(c));
        let mut b = BytesMut::new();
        e.encode(ready.clone(), &mut b).unwrap();
        assert_eq!(&b[..], &unhex(hex)[..], "{c:?}");
        // The client codec reads it back.
        let mut cc = ClientCodec::new();
        cc.set_compression(Some(c));
        assert_eq!(cc.decode(&mut b).unwrap().unwrap(), ready);
    }
}

#[test]
fn startup_is_never_compressed() {
    let mut cc = ClientCodec::new();
    cc.set_compression(Some(Compression::Lz4));
    let mut b = BytesMut::new();
    let f = RequestFrame::new(0, Request::Startup(Startup::new("3.0.0"))).to_frame(Version::V4);
    cc.encode(f.clone(), &mut b).unwrap();
    assert_eq!(b.freeze(), f.to_bytes());
}

// ---- v5 segments ----

fn v5_pair(lz4: bool) -> (ServerCodec, ClientCodec) {
    let mut s = ServerCodec::new(Dialect::Cassandra);
    s.established(Version::V5, if lz4 { Some(Compression::Lz4) } else { None });
    s.start_segments(lz4);
    let mut c = ClientCodec::new();
    c.start_segments(lz4);
    (s, c)
}

#[test]
fn v5_round_trips_small_and_large_messages() {
    for lz4 in [false, true] {
        let (mut s, mut c) = v5_pair(lz4);
        let small = RequestFrame::new(1, Request::Options).to_frame(Version::V5);
        let big = RequestFrame::new(2, Request::Query(Query { query: "x".repeat(400_000), params: QueryParams::default() })).to_frame(Version::V5);
        let mut wire = BytesMut::new();
        for f in [&small, &big, &small] {
            c.encode(f.clone(), &mut wire).unwrap();
        }
        // Fed in uneven chunks.
        let mut got = vec![];
        let mut buf = BytesMut::new();
        for chunk in wire.chunks(7919) {
            buf.extend_from_slice(chunk);
            while let Some(Inbound::Frame(f)) = s.decode(&mut buf).unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got, vec![small.clone(), big.clone(), small.clone()]);
        // And back: several responses packed into segments.
        let resp: Vec<Frame> = (0..5).map(|i| ResponseFrame::new(i, Response::Ready).to_frame(Version::V5)).collect();
        let mut out = BytesMut::new();
        s.encode(resp.clone(), &mut out).unwrap();
        let big_resp = ResponseFrame::new(9, Response::Authenticate("a".repeat(60_000))).to_frame(Version::V5);
        let mut huge = big_resp.clone();
        huge.body = Bytes::from(vec![0u8; 300_000]);
        huge.opcode = Opcode::AuthSuccess;
        s.encode(huge.clone(), &mut out).unwrap();
        let mut back = vec![];
        while let Some(f) = c.decode(&mut out).unwrap() {
            back.push(f);
        }
        assert_eq!(back.len(), 6);
        assert_eq!(&back[..5], &resp[..]);
        assert_eq!(back[5], huge);
    }
}

#[test]
fn v5_envelope_errors() {
    let (mut s, mut c) = v5_pair(false);
    let wrong = RequestFrame::new(3, Request::Options).to_frame(Version::V4);
    let mut wire = BytesMut::new();
    c.encode(wrong, &mut wire).unwrap();
    let Some(Inbound::Reject(r)) = s.decode(&mut wire).unwrap() else { panic!() };
    assert_eq!(r.error.message, "Invalid message version. Got 4/v4 but previousmessages on this connection had version 5/v5");
    assert_eq!((r.stream, r.close), (Some(3), false));
    // Answered inside a segment.
    let mut out = BytesMut::new();
    s.encode(r, &mut out).unwrap();
    let f = c.decode(&mut out).unwrap().unwrap();
    assert_eq!((f.version, f.opcode, f.stream), (Version::V5, Opcode::Error, 3));
    // Unknown opcode and a v6 envelope without USE_BETA inside segments.
    for (env, msg) in [
        (frame(5, 0, 4, 0x33, b""), "Unknown opcode 51"),
        (frame(7, 0, 4, 5, b""), "Invalid or unsupported protocol version (7); supported versions are (3/v3, 4/v4, 5/v5, 6/v6-beta)"),
        (frame(0x85, 0, 4, 5, b""), "Wrong protocol direction (expected REQUEST, got RESPONSE) for opcode 5 (OPTIONS)"),
        (frame(6, 0, 4, 5, b""), "Beta version of the protocol used (6/v6-beta), but USE_BETA flag is unset"),
    ] {
        let mut wire = BytesMut::new();
        segment::encode_segment(&mut wire, &env, true, false);
        let Some(Inbound::Reject(r)) = s.decode(&mut wire).unwrap() else { panic!() };
        assert_eq!(r.error.message, msg);
    }
}

#[test]
fn v5_corrupt_segments_close_with_cassandras_message() {
    let (mut s, mut c) = v5_pair(false);
    s.decoder.set_connection_id("1a2b3c4d");
    let mut wire = BytesMut::new();
    c.encode(RequestFrame::new(1, Request::Options).to_frame(Version::V5), &mut wire).unwrap();
    let n = wire.len();
    wire[n - 1] ^= 1;
    let Some(Inbound::Reject(r)) = s.decode(&mut wire).unwrap() else { panic!() };
    assert!(r.error.message.starts_with("1a2b3c4d invalid, unrecoverable CRC mismatch detected in frame body. Read "));
    assert_eq!((r.stream, r.close), (Some(0), true));
    let (mut s, _) = v5_pair(true);
    let mut wire = BytesMut::from(&[0u8; 12][..]);
    let Some(Inbound::Reject(r)) = s.decode(&mut wire).unwrap() else { panic!() };
    assert!(r.error.message.contains("frame header"));
    // A client sees corruption as an error.
    let (_, mut c) = v5_pair(false);
    assert!(matches!(c.decode(&mut BytesMut::from(&[1u8, 2, 3, 4, 5, 6][..])), Err(CodecError::Segment(_))));
}

#[test]
fn v5_oversized_messages() {
    let limits = Limits { max_message_size: 1000, ..Limits::of(Dialect::Cassandra) };
    let mut s = ServerDecoder::with_limits(Dialect::Cassandra, limits);
    s.established(Version::V5);
    s.start_segments(false);
    let mut c = ClientCodec::new();
    c.start_segments(false);
    let big = RequestFrame::new(8, Request::Query(Query { query: "y".repeat(200_000), params: QueryParams::default() })).to_frame(Version::V5);
    let mut wire = BytesMut::new();
    c.encode(big.clone(), &mut wire).unwrap();
    c.encode(RequestFrame::new(9, Request::Options).to_frame(Version::V5), &mut wire).unwrap();
    let Some(Inbound::Reject(r)) = s.decode(&mut wire).unwrap() else { panic!() };
    assert_eq!(r.error, ErrorBody::invalid(format!("CQL Message of size {} bytes exceeds allowed maximum of 1000 bytes", big.body.len())));
    assert_eq!(r.stream, Some(8));
    assert!(matches!(s.decode(&mut wire).unwrap(), Some(Inbound::Frame(f)) if f.stream == 9));
}

// ---- client ----

#[test]
fn client_codec_errors_and_partial_input() {
    let mut c = ClientCodec::new().with_max_frame_size(64);
    let f = ResponseFrame::new(1, Response::Ready).to_frame(Version::V4);
    let bytes = f.to_bytes();
    let mut buf = BytesMut::new();
    for &b in bytes.iter() {
        assert_eq!(c.decode(&mut buf).unwrap(), None);
        buf.put_u8(b);
    }
    assert_eq!(c.decode(&mut buf).unwrap(), Some(f));
    assert!(matches!(c.decode(&mut frame(0x84, 0, 1, 2, &[0; 100])), Err(CodecError::TooLarge(109))));
    assert!(matches!(ClientCodec::new().decode(&mut frame(0x89, 0, 1, 2, b"")), Err(CodecError::Decode(_))));
    assert!(matches!(ClientCodec::new().decode(&mut frame(0x84, 0, 1, 0x40, b"")), Err(CodecError::Decode(_))));
    let mut cc = ClientCodec::new();
    cc.set_compression(Some(Compression::Lz4));
    assert!(matches!(cc.decode(&mut frame(0x84, 1, 1, 2, b"\xff\xff\xff\xff\x00")), Err(CodecError::Decode(_))));
}

#[test]
fn fuzz_server_decoder_never_panics() {
    // Deterministic pseudo-random garbage and mutations of valid traffic, both dialects and
    // every phase.
    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let valid = frame(4, 0, 1, 1, &startup_body());
    for round in 0..3000 {
        let dialect = if round % 2 == 0 { Dialect::Cassandra } else { Dialect::Scylla };
        let mut d = ServerDecoder::new(dialect);
        match round % 5 {
            1 => d.established(Version::V4),
            2 => {
                d.established(Version::V5);
                d.start_segments(round % 3 == 0);
            }
            3 => {
                d.established(Version::V4);
                d.set_compression(Some(if round % 3 == 0 { Compression::Lz4 } else { Compression::Snappy }));
            }
            _ => {}
        }
        let mut buf = BytesMut::from(&valid[..]);
        let len = (rnd() % 64) as usize;
        for _ in 0..len {
            let r = rnd();
            match r % 3 {
                0 => {
                    let i = (r >> 8) as usize % buf.len().max(1);
                    if !buf.is_empty() {
                        buf[i] ^= (r >> 16) as u8;
                    }
                }
                1 => buf.put_u8((r >> 24) as u8),
                _ => buf.truncate(buf.len().saturating_sub(1)),
            }
        }
        for _ in 0..8 {
            if d.decode(&mut buf).is_err() {
                break;
            }
        }
        let mut c = ClientCodec::new().with_max_frame_size(1 << 20);
        if round % 2 == 0 {
            c.start_segments(round % 4 == 0);
        }
        let mut cb = buf.clone();
        let _ = c.decode(&mut cb);
    }
}

#[test]
fn batched_unframed_responses() {
    let frames: Vec<Frame> = (0..3).map(|i| ResponseFrame::new(i, Response::Ready).to_frame(Version::V4)).collect();
    let mut e = ServerEncoder::new();
    e.set_compression(Some(Compression::Snappy));
    let mut b = BytesMut::new();
    e.encode(frames.clone(), &mut b).unwrap();
    let mut c = ClientCodec::new();
    c.set_compression(Some(Compression::Snappy));
    for f in frames {
        assert_eq!(c.decode(&mut b).unwrap(), Some(f));
    }
    assert!(b.is_empty());
    // A rejection with a v1 header goes out raw even when compression is on.
    let mut d = ServerDecoder::new(Dialect::Cassandra);
    let r = rejection(&mut d, BytesMut::from(&[2u8, 0, 7, 5, 0, 0, 0, 0, 0][..]));
    let mut out = BytesMut::new();
    e.encode(r.clone(), &mut out).unwrap();
    assert_eq!(out.freeze(), r.to_bytes().unwrap());
    // An unanswerable rejection writes nothing.
    let mut out = BytesMut::new();
    e.encode(Rejection { stream: None, ..r }, &mut out).unwrap();
    assert!(out.is_empty());
}

#[test]
fn phases_and_limits() {
    let mut d = ServerDecoder::new(Dialect::Scylla);
    assert_eq!((d.phase(), d.dialect(), d.segments()), (Phase::Initial, Dialect::Scylla, false));
    // A v3 STARTUP is fine before STARTUP on ScyllaDB.
    assert!(matches!(one(&mut d, frame(3, 0, 1, 1, &startup_body())), Some(Inbound::Frame(_))));
    d.established(Version::V3);
    assert_eq!(d.phase(), Phase::Established(Version::V3));
    assert_eq!(Limits::of(Dialect::Cassandra).max_frame_size, 16 << 20);
    assert_eq!(Limits::default().max_frame_size, 256 << 20);
    assert_eq!(invalid_version_message(Dialect::Scylla, 9), "Invalid or unsupported protocol version: 9");
}
