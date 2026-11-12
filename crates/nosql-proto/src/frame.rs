//! Envelopes ("frames" before v5): the 9-byte header and the body.
//!
//! ```text
//! version(1) flags(1) stream(2) opcode(1) length(4) body(length)
//! ```
//!
//! The version byte carries the direction in its top bit. Compression of the body (v3/v4) is
//! applied by the codecs; a [`Frame`] here always holds the plain body. Follows
//! `org.apache.cassandra.transport.Envelope`.

use bytes::{BufMut, Bytes, BytesMut};

use crate::version::{Direction, Version};

/// Header flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags(pub u8);

impl Flags {
    /// The body is compressed (v3/v4; ignored from v5).
    pub const COMPRESSION: u8 = 0x01;
    /// Request: trace this. Response: the body starts with a tracing id.
    pub const TRACING: u8 = 0x02;
    /// A `[bytes map]` custom payload precedes the message (v4+).
    pub const CUSTOM_PAYLOAD: u8 = 0x04;
    /// A response carries warnings (v4+).
    pub const WARNING: u8 = 0x08;
    /// The client opts in to a beta version.
    pub const USE_BETA: u8 = 0x10;

    pub fn has(self, f: u8) -> bool {
        self.0 & f != 0
    }

    pub fn set(&mut self, f: u8, on: bool) {
        if on {
            self.0 |= f;
        } else {
            self.0 &= !f;
        }
    }

    pub fn with(mut self, f: u8) -> Self {
        self.0 |= f;
        self
    }
}

/// Message types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Opcode {
    Error = 0x00,
    Startup = 0x01,
    Ready = 0x02,
    Authenticate = 0x03,
    /// Protocol v1's credentials message; accepted as an opcode, refused as a message.
    Credentials = 0x04,
    Options = 0x05,
    Supported = 0x06,
    Query = 0x07,
    Result = 0x08,
    Prepare = 0x09,
    Execute = 0x0A,
    Register = 0x0B,
    Event = 0x0C,
    Batch = 0x0D,
    AuthChallenge = 0x0E,
    AuthResponse = 0x0F,
    AuthSuccess = 0x10,
}

impl Opcode {
    pub fn from_u8(n: u8) -> Option<Self> {
        use Opcode::*;
        Some(match n {
            0x00 => Error,
            0x01 => Startup,
            0x02 => Ready,
            0x03 => Authenticate,
            0x04 => Credentials,
            0x05 => Options,
            0x06 => Supported,
            0x07 => Query,
            0x08 => Result,
            0x09 => Prepare,
            0x0A => Execute,
            0x0B => Register,
            0x0C => Event,
            0x0D => Batch,
            0x0E => AuthChallenge,
            0x0F => AuthResponse,
            0x10 => AuthSuccess,
            _ => return None,
        })
    }

    /// Who sends it.
    pub fn direction(self) -> Direction {
        use Opcode::*;
        match self {
            Startup | Credentials | Options | Query | Prepare | Execute | Register | Batch | AuthResponse => {
                Direction::Request
            }
            Error | Ready | Authenticate | Supported | Result | Event | AuthChallenge | AuthSuccess => Direction::Response,
        }
    }

    /// Cassandra's `Message.Type` name (`AUTH_RESPONSE`).
    pub fn name(self) -> &'static str {
        use Opcode::*;
        match self {
            Error => "ERROR",
            Startup => "STARTUP",
            Ready => "READY",
            Authenticate => "AUTHENTICATE",
            Credentials => "CREDENTIALS",
            Options => "OPTIONS",
            Supported => "SUPPORTED",
            Query => "QUERY",
            Result => "RESULT",
            Prepare => "PREPARE",
            Execute => "EXECUTE",
            Register => "REGISTER",
            Event => "EVENT",
            Batch => "BATCH",
            AuthChallenge => "AUTH_CHALLENGE",
            AuthResponse => "AUTH_RESPONSE",
            AuthSuccess => "AUTH_SUCCESS",
        }
    }
}

/// A raw envelope header, as read off the wire (nothing validated).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u8,
    pub flags: Flags,
    pub stream: i16,
    pub opcode: u8,
    pub length: u32,
}

impl Header {
    pub const LEN: usize = 9;

    pub fn parse(b: &[u8; 9]) -> Self {
        Header {
            version: b[0],
            flags: Flags(b[1]),
            stream: i16::from_be_bytes([b[2], b[3]]),
            opcode: b[4],
            length: u32::from_be_bytes([b[5], b[6], b[7], b[8]]),
        }
    }

    /// Parses the first 9 bytes of `b`, if present.
    pub fn peek(b: &[u8]) -> Option<Self> {
        b.get(..Self::LEN).map(|h| Self::parse(h.try_into().expect("9 bytes")))
    }

    pub fn write(&self, out: &mut impl BufMut) {
        out.put_u8(self.version);
        out.put_u8(self.flags.0);
        out.put_i16(self.stream);
        out.put_u8(self.opcode);
        out.put_u32(self.length);
    }
}

/// A complete envelope with a validated version and opcode and an uncompressed body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub version: Version,
    pub direction: Direction,
    pub flags: Flags,
    pub stream: i16,
    pub opcode: Opcode,
    pub body: Bytes,
}

impl Frame {
    pub fn header(&self) -> Header {
        Header {
            version: self.version.byte(self.direction),
            flags: self.flags,
            stream: self.stream,
            opcode: self.opcode as u8,
            length: self.body.len() as u32,
        }
    }

    /// Header and body, as they go on the wire (or into a v5 segment).
    pub fn encode(&self, out: &mut BytesMut) {
        out.reserve(Header::LEN + self.body.len());
        self.header().write(out);
        out.put_slice(&self.body);
    }

    pub fn encoded_len(&self) -> usize {
        Header::LEN + self.body.len()
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut b = BytesMut::new();
        self.encode(&mut b);
        b.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opcodes() {
        for n in 0..=0x10u8 {
            let op = Opcode::from_u8(n).unwrap();
            assert_eq!(op as u8, n);
        }
        assert_eq!(Opcode::from_u8(0x11), None);
        assert_eq!(Opcode::AuthResponse.name(), "AUTH_RESPONSE");
        assert_eq!(Opcode::Query.direction(), Direction::Request);
        assert_eq!(Opcode::Event.direction(), Direction::Response);
        assert_eq!(Opcode::Credentials.direction(), Direction::Request);
    }

    #[test]
    fn header_round_trip() {
        let f = Frame {
            version: Version::V4,
            direction: Direction::Response,
            flags: Flags(Flags::TRACING | Flags::WARNING),
            stream: -1,
            opcode: Opcode::Event,
            body: Bytes::from_static(b"abc"),
        };
        let b = f.to_bytes();
        assert_eq!(&b[..], b"\x84\x0a\xff\xff\x0c\x00\x00\x00\x03abc");
        let h = Header::peek(&b).unwrap();
        assert_eq!(h, f.header());
        assert_eq!(Header::peek(&b[..8]), None);
        let mut fl = Flags::default();
        fl.set(Flags::USE_BETA, true);
        assert!(fl.has(Flags::USE_BETA));
        fl.set(Flags::USE_BETA, false);
        assert_eq!(fl, Flags(0));
    }
}
