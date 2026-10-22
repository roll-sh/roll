//! Decoding failures.
//!
//! A [`DecodeError`] keeps the facts (which field, how many bytes were needed and present)
//! rather than a sentence, because the two references word the same failure differently:
//! Cassandra says `Not enough bytes to read an UTF8 serialized string preceded by its 2 bytes
//! length`, ScyllaDB `truncated frame: expected 5 bytes, length is 2`.
//! [`DecodeError::to_error`] renders the ERROR body the chosen dialect sends.

use bytes::Bytes;

use crate::error_body::{ErrorBody, code};
use crate::frame::Opcode;
use crate::version::{Dialect, Direction};

/// The kind of item a truncated read was after; it selects Cassandra's message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    Byte,
    Short,
    Int,
    Long,
    Uuid,
    Inet,
    /// `[string]`, its length or its bytes.
    String,
    /// `[long string]`
    LongString,
    /// `[short bytes]`
    ShortBytes,
    /// The bytes of a `[bytes]` whose length was read.
    Bytes,
    /// The bytes of a `[value]` whose length was read.
    Value,
    /// Anything else (segment payloads, compressed bodies).
    Raw,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// `need` bytes were required at body offset `at`, only `have` remained of `len`.
    #[error("truncated {field:?} at offset {at}: need {need} bytes, {have} remaining")]
    Truncated { field: Field, need: usize, have: usize, at: usize, len: usize },
    #[error("Cannot decode string as UTF8")]
    Utf8 { bytes: Bytes, valid_up_to: usize, error_len: usize },
    #[error("Invalid ByteBuf length {0}")]
    InvalidValueLength(i32),
    #[error("Unknown code {0} for a consistency level")]
    UnknownConsistency(u16),
    /// A string that names an enum constant (event types, schema change kinds…). `ty` is
    /// qualified (`Event.Type`); Cassandra prints the last part.
    #[error("Invalid value '{value}' for {ty}")]
    InvalidEnum { value: String, ty: &'static str },
    #[error("Invalid query kind in BATCH messages. Must be 0 or 1 but got {0}")]
    BatchKind(u8),
    #[error("Invalid BATCH message type {0}")]
    BatchType(u8),
    #[error("Unknown opcode {0}")]
    UnknownOpcode(u8),
    /// A known opcode sent in the wrong direction (a response opcode from a client, or the
    /// response bit on a request).
    #[error("Wrong protocol direction for opcode {opcode}")]
    WrongDirection { opcode: u8, got: Direction },
    /// CREDENTIALS (protocol v1 authentication).
    #[error("Unsupported message")]
    UnsupportedMessage,
    #[error("Received frame with CUSTOM_PAYLOAD flag for native protocol version < 4")]
    CustomPayloadBeforeV4,
    #[error("Out of bound timestamp, must be in [{min}, {max}] (got {0})", min = i64::MIN + 1, max = i64::MAX)]
    TimestampOutOfRange(i64),
    /// Compressed body or segment that does not decompress.
    #[error("{0}")]
    Compression(String),
    /// Any other malformed input, with the message to send as a protocol error.
    #[error("{0}")]
    Invalid(String),
}

impl DecodeError {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        DecodeError::Invalid(msg.into())
    }

    /// The ERROR the dialect answers this failure with.
    pub fn to_error(&self, dialect: Dialect) -> ErrorBody {
        let protocol = |m: String| ErrorBody::new(code::PROTOCOL_ERROR, m);
        match (self, dialect) {
            (DecodeError::Truncated { need, have, .. }, Dialect::Scylla) => {
                protocol(format!("truncated frame: expected {need} bytes, length is {have}"))
            }
            (DecodeError::Truncated { field, need, have, at, len }, Dialect::Cassandra) => match field {
                Field::String => {
                    protocol("Not enough bytes to read an UTF8 serialized string preceded by its 2 bytes length".into())
                }
                Field::LongString => {
                    protocol("Not enough bytes to read an UTF8 serialized string preceded by its 4 bytes length".into())
                }
                Field::ShortBytes => {
                    protocol("Not enough bytes to read a byte array preceded by its 2 bytes length".into())
                }
                Field::Bytes | Field::Value => protocol(format!(
                    "Cannot read value of length {need}, only {have} bytes remaining in the message"
                )),
                // Fixed-size reads go straight to Netty, whose bounds check surfaces as a
                // server error. The outer buffer is the whole envelope (9-byte header + body);
                // its capacity is Netty's adaptive read size, 2048 for small requests.
                _ => ErrorBody::new(
                    code::SERVER_ERROR,
                    format!(
                        "java.lang.IndexOutOfBoundsException: readerIndex({at}) + length({need}) exceeds \
                         writerIndex({len}): UnpooledSlicedByteBuf(ridx: {at}, widx: {len}, cap: {len}/{len}, \
                         unwrapped: PooledUnsafeDirectByteBuf(ridx: {o}, widx: {o}, cap: {cap}))",
                        o = len + 9,
                        cap = (len + 9).next_power_of_two().max(2048),
                    ),
                ),
            },
            (DecodeError::Utf8 { bytes, error_len, .. }, Dialect::Cassandra) => protocol(format!(
                "Cannot decode string as UTF8: '{}'; java.nio.charset.MalformedInputException: Input length = {error_len}",
                hex(bytes)
            )),
            (DecodeError::Utf8 { valid_up_to, .. }, Dialect::Scylla) => protocol(format!(
                "Cannot decode string as UTF8, invalid character at byte offset {valid_up_to}"
            )),
            (DecodeError::InvalidValueLength(n), Dialect::Scylla) => protocol(format!("invalid value length: {n}")),
            (DecodeError::InvalidEnum { value, ty }, Dialect::Cassandra) => {
                let simple = ty.rsplit('.').next().unwrap_or(ty);
                protocol(format!("Invalid value '{value}' for {simple}"))
            }
            (DecodeError::BatchKind(k), _) => protocol(format!(
                "Invalid query kind in BATCH messages. Must be 0 or 1 but got {}",
                *k as i8
            )),
            (DecodeError::BatchType(t), _) => protocol(format!("Invalid BATCH message type {}", *t as i8)),
            (DecodeError::WrongDirection { opcode, got }, _) => {
                let expected = match got {
                    Direction::Request => Direction::Response,
                    Direction::Response => Direction::Request,
                };
                let name = Opcode::from_u8(*opcode).map_or("?", Opcode::name);
                protocol(format!(
                    "Wrong protocol direction (expected {}, got {}) for opcode {opcode} ({name})",
                    expected.name(),
                    got.name()
                ))
            }
            (e, _) => protocol(e.to_string()),
        }
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &c in b {
        s.push(H[(c >> 4) as usize] as char);
        s.push(H[(c & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(e: &DecodeError, d: Dialect) -> (i32, String) {
        let b = e.to_error(d);
        (b.code, b.message)
    }

    #[test]
    fn truncation_texts_match_the_references() {
        let t = |field| DecodeError::Truncated { field, need: 100, have: 6, at: 4, len: 10 };
        assert_eq!(
            msg(&t(Field::LongString), Dialect::Cassandra).1,
            "Not enough bytes to read an UTF8 serialized string preceded by its 4 bytes length"
        );
        assert_eq!(msg(&t(Field::LongString), Dialect::Scylla).1, "truncated frame: expected 100 bytes, length is 6");
        // Observed from Cassandra 5.0.9: QUERY with a 6-byte query string and nothing after.
        let e = DecodeError::Truncated { field: Field::Short, need: 2, have: 0, at: 10, len: 10 };
        assert_eq!(
            msg(&e, Dialect::Cassandra),
            (
                0,
                "java.lang.IndexOutOfBoundsException: readerIndex(10) + length(2) exceeds writerIndex(10): \
                 UnpooledSlicedByteBuf(ridx: 10, widx: 10, cap: 10/10, unwrapped: \
                 PooledUnsafeDirectByteBuf(ridx: 19, widx: 19, cap: 2048))"
                    .to_string()
            )
        );
        assert_eq!(
            msg(&t(Field::Value), Dialect::Cassandra).1,
            "Cannot read value of length 100, only 6 bytes remaining in the message"
        );
    }

    #[test]
    fn other_texts() {
        let e = DecodeError::InvalidEnum { value: "FOO".into(), ty: "Event.Type" };
        assert_eq!(msg(&e, Dialect::Cassandra).1, "Invalid value 'FOO' for Type");
        assert_eq!(msg(&e, Dialect::Scylla).1, "Invalid value 'FOO' for Event.Type");
        assert_eq!(msg(&DecodeError::InvalidValueLength(-3), Dialect::Cassandra).1, "Invalid ByteBuf length -3");
        assert_eq!(msg(&DecodeError::InvalidValueLength(-3), Dialect::Scylla).1, "invalid value length: -3");
        assert_eq!(
            msg(&DecodeError::UnknownConsistency(25344), Dialect::Scylla).1,
            "Unknown code 25344 for a consistency level"
        );
        assert_eq!(
            msg(&DecodeError::WrongDirection { opcode: 2, got: Direction::Request }, Dialect::Cassandra).1,
            "Wrong protocol direction (expected RESPONSE, got REQUEST) for opcode 2 (READY)"
        );
        assert_eq!(
            msg(&DecodeError::WrongDirection { opcode: 5, got: Direction::Response }, Dialect::Cassandra).1,
            "Wrong protocol direction (expected REQUEST, got RESPONSE) for opcode 5 (OPTIONS)"
        );
        let u = DecodeError::Utf8 { bytes: Bytes::from_static(b"\xff\xfe"), valid_up_to: 0, error_len: 1 };
        assert_eq!(
            msg(&u, Dialect::Cassandra).1,
            "Cannot decode string as UTF8: 'fffe'; java.nio.charset.MalformedInputException: Input length = 1"
        );
        assert_eq!(msg(&u, Dialect::Scylla).1, "Cannot decode string as UTF8, invalid character at byte offset 0");
        assert_eq!(
            msg(&DecodeError::BatchKind(5), Dialect::Scylla).1,
            "Invalid query kind in BATCH messages. Must be 0 or 1 but got 5"
        );
        assert_eq!(
            msg(&DecodeError::TimestampOutOfRange(i64::MIN), Dialect::Cassandra).1,
            "Out of bound timestamp, must be in [-9223372036854775807, 9223372036854775807] (got -9223372036854775808)"
        );
    }
}
