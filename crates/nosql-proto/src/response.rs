//! Response messages (spec 4.2) and their envelope prefix:
//! `[tracing id][warnings][custom payload]<message>`.

use bytes::{Bytes, BytesMut};

use crate::error::DecodeError;
use crate::error_body::ErrorBody;
use crate::event::Event;
use crate::frame::{Flags, Frame, Opcode};
use crate::request::CustomPayload;
use crate::result::ResultBody;
use crate::supported::Supported;
use crate::version::{Ctx, Direction, Version};
use crate::wire::{PutExt, Reader};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Error(ErrorBody),
    Ready,
    /// The authenticator's class name.
    Authenticate(String),
    Supported(Supported),
    Result(ResultBody),
    Event(Event),
    AuthChallenge(Option<Bytes>),
    AuthSuccess(Option<Bytes>),
}

impl Response {
    pub fn opcode(&self) -> Opcode {
        match self {
            Response::Error(_) => Opcode::Error,
            Response::Ready => Opcode::Ready,
            Response::Authenticate(_) => Opcode::Authenticate,
            Response::Supported(_) => Opcode::Supported,
            Response::Result(_) => Opcode::Result,
            Response::Event(_) => Opcode::Event,
            Response::AuthChallenge(_) => Opcode::AuthChallenge,
            Response::AuthSuccess(_) => Opcode::AuthSuccess,
        }
    }

    pub fn encode(&self, ctx: impl Into<Ctx>, out: &mut BytesMut) {
        let ctx = ctx.into();
        match self {
            Response::Error(e) => e.encode(ctx, out),
            Response::Ready => {}
            Response::Authenticate(a) => out.put_string(a),
            Response::Supported(s) => out.put_string_multimap(&s.0),
            Response::Result(r) => r.encode(ctx, out),
            Response::Event(e) => e.encode(ctx.version, out),
            Response::AuthChallenge(t) | Response::AuthSuccess(t) => out.put_cql_bytes(t.as_deref()),
        }
    }

    /// Decodes a body (after the prefix). Trailing bytes are ignored.
    pub fn decode(ctx: impl Into<Ctx>, opcode: Opcode, body: Bytes) -> Result<Self, DecodeError> {
        let ctx = ctx.into();
        let mut r = Reader::new(body);
        Ok(match opcode {
            Opcode::Error => Response::Error(ErrorBody::decode(ctx, &mut r)?),
            Opcode::Ready => Response::Ready,
            Opcode::Authenticate => Response::Authenticate(r.string()?),
            Opcode::Supported => Response::Supported(Supported(r.string_multimap()?)),
            Opcode::Result => Response::Result(ResultBody::decode(ctx, &mut r)?),
            Opcode::Event => Response::Event(Event::decode(&mut r)?),
            Opcode::AuthChallenge => Response::AuthChallenge(r.bytes()?),
            Opcode::AuthSuccess => Response::AuthSuccess(r.bytes()?),
            op => return Err(DecodeError::WrongDirection { opcode: op as u8, got: Direction::Response }),
        })
    }
}

/// A response with its envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseFrame {
    pub stream: i16,
    pub tracing_id: Option<[u8; 16]>,
    /// v4+.
    pub warnings: Vec<String>,
    /// v4+.
    pub custom_payload: Option<CustomPayload>,
    pub response: Response,
}

impl ResponseFrame {
    pub fn new(stream: i16, response: Response) -> Self {
        ResponseFrame { stream, tracing_id: None, warnings: vec![], custom_payload: None, response }
    }

    /// An EVENT push (stream -1).
    pub fn event(e: Event) -> Self {
        Self::new(-1, Response::Event(e))
    }

    pub fn error(stream: i16, e: ErrorBody) -> Self {
        Self::new(stream, Response::Error(e))
    }

    /// Encodes the envelope. Warnings and custom payloads are not representable before v4
    /// and are dropped there (Cassandra logs and drops warnings); v6 responses carry USE_BETA.
    pub fn to_frame(&self, ctx: impl Into<Ctx>) -> Frame {
        let ctx = ctx.into();
        let v4 = ctx.version >= Version::V4;
        let mut flags = Flags::default();
        let mut body = BytesMut::new();
        if let Some(id) = &self.tracing_id {
            flags.set(Flags::TRACING, true);
            body.extend_from_slice(id);
        }
        if v4 && !self.warnings.is_empty() {
            flags.set(Flags::WARNING, true);
            body.put_string_list(&self.warnings);
        }
        if let (true, Some(p)) = (v4, &self.custom_payload) {
            flags.set(Flags::CUSTOM_PAYLOAD, true);
            body.put_bytes_map(p);
        }
        flags.set(Flags::USE_BETA, ctx.version.is_beta());
        self.response.encode(ctx, &mut body);
        Frame {
            version: ctx.version,
            direction: Direction::Response,
            flags,
            stream: self.stream,
            opcode: self.response.opcode(),
            body: body.freeze(),
        }
    }

    pub fn decode(ctx: impl Into<Ctx>, frame: &Frame) -> Result<Self, DecodeError> {
        let ctx = ctx.into();
        if frame.direction != Direction::Response || frame.opcode.direction() != Direction::Response {
            return Err(DecodeError::WrongDirection { opcode: frame.opcode as u8, got: frame.direction });
        }
        let mut r = Reader::new(frame.body.clone());
        let tracing_id = if frame.flags.has(Flags::TRACING) { Some(r.uuid()?) } else { None };
        let warnings = if frame.flags.has(Flags::WARNING) { r.string_list()? } else { vec![] };
        let custom_payload = if frame.flags.has(Flags::CUSTOM_PAYLOAD) { Some(r.bytes_map()?) } else { None };
        let response = Response::decode(ctx, frame.opcode, r.rest())?;
        Ok(ResponseFrame { stream: frame.stream, tracing_id, warnings, custom_payload, response })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error_body::code;
    use crate::event::{SchemaChange, SchemaChangeType};
    use crate::result::RowsMetadata;

    fn all() -> Vec<Response> {
        vec![
            Response::Error(ErrorBody::new(code::SYNTAX_ERROR, "line 1:0 no viable alternative")),
            Response::Ready,
            Response::Authenticate("org.apache.cassandra.auth.PasswordAuthenticator".into()),
            Response::Supported(Supported(vec![("COMPRESSION".into(), vec!["snappy".into(), "lz4".into()])])),
            Response::Result(ResultBody::Void),
            Response::Result(ResultBody::Rows(crate::result::Rows::empty(RowsMetadata::new(vec![])))),
            Response::Event(Event::SchemaChange(SchemaChange::keyspace(SchemaChangeType::Dropped, "ks"))),
            Response::AuthChallenge(Some(Bytes::from_static(b"challenge"))),
            Response::AuthSuccess(None),
        ]
    }

    #[test]
    fn every_response_round_trips_with_every_prefix() {
        for v in [Version::V3, Version::V4, Version::V5, Version::V6] {
            for resp in all() {
                for (tracing, warnings, payload) in [(false, false, false), (true, false, false), (true, true, true), (false, true, true)] {
                    let rf = ResponseFrame {
                        stream: 42,
                        tracing_id: tracing.then_some([7; 16]),
                        warnings: if warnings { vec!["Aggregation query used without partition key".into()] } else { vec![] },
                        custom_payload: payload.then(|| vec![("tablets-routing-v1".into(), Some(Bytes::from_static(b"x")))]),
                        response: resp.clone(),
                    };
                    let f = rf.to_frame(v);
                    assert_eq!(f.flags.has(Flags::USE_BETA), v == Version::V6);
                    let back = ResponseFrame::decode(v, &f).unwrap();
                    if v >= Version::V4 {
                        assert_eq!(back, rf);
                    } else {
                        assert_eq!((back.tracing_id, back.warnings.len(), back.custom_payload), (rf.tracing_id, 0, None));
                        assert_eq!(back.response, rf.response);
                    }
                }
            }
        }
    }

    #[test]
    fn matches_reference_bytes() {
        // SUPPORTED from Cassandra 5.0.9 (v4, stream 0x0102), captured from the image.
        let raw = b"\x84\x00\x01\x02\x06\x00\x00\x00\x66\x00\x03\x00\x11PROTOCOL_VERSIONS\x00\x04\x00\x043/v3\x00\x044/v4\
\x00\x045/v5\x00\x096/v6-beta\x00\x0bCOMPRESSION\x00\x02\x00\x06snappy\x00\x03lz4\x00\x0bCQL_VERSION\x00\x01\x00\x053.4.7";
        let rf = ResponseFrame::new(0x0102, Response::Supported(crate::supported::cassandra()));
        assert_eq!(&rf.to_frame(Version::V4).to_bytes()[..], &raw[..]);
        // A traced RESULT from ScyllaDB (flags 0x02, tracing id first).
        let body = Bytes::from_static(b"\xf4\xa1\x7f\x80\xcc\xa9\x11\xf1\x92\x0d\x6b\x11\xa5\x58\x4c\x93\x00\x00\x00\x01");
        let f = Frame { version: Version::V4, direction: Direction::Response, flags: Flags(Flags::TRACING), stream: 9, opcode: Opcode::Result, body };
        let rf = ResponseFrame::decode(Version::V4, &f).unwrap();
        assert_eq!(rf.tracing_id.unwrap()[0], 0xf4);
        assert_eq!(rf.response, Response::Result(ResultBody::Void));
    }

    #[test]
    fn malformed() {
        let f = |flags: u8, opcode, body: &'static [u8]| Frame {
            version: Version::V4,
            direction: Direction::Response,
            flags: Flags(flags),
            stream: 0,
            opcode,
            body: Bytes::from_static(body),
        };
        assert!(ResponseFrame::decode(Version::V4, &f(Flags::TRACING, Opcode::Ready, b"short")).is_err());
        assert!(ResponseFrame::decode(Version::V4, &f(Flags::WARNING, Opcode::Ready, b"\xff\xff")).is_err());
        assert!(ResponseFrame::decode(Version::V4, &f(Flags::CUSTOM_PAYLOAD, Opcode::Ready, b"\x00\x01\x00")).is_err());
        assert!(ResponseFrame::decode(Version::V4, &f(0, Opcode::Query, b"")).is_err());
        assert!(ResponseFrame::decode(Version::V4, &f(0, Opcode::Authenticate, b"\x00\x09abc")).is_err());
        for resp in all() {
            let mut b = BytesMut::new();
            resp.encode(Version::V5, &mut b);
            let full = b.freeze();
            for cut in 0..full.len() {
                assert!(Response::decode(Version::V5, resp.opcode(), full.slice(..cut)).is_err(), "{resp:?} {cut}");
            }
        }
    }
}
