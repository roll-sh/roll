//! The ERROR message (spec section 8): every code and its code-specific body.
//!
//! Encoding applies Cassandra's downgrades for older versions
//! (`ErrorMessage.getBackwardsCompatibleException`): before v4, read/write failures become
//! timeouts and function/CDC failures become `Invalid`; before v5, CAS timeouts lose their
//! contention count and `CAS_WRITE_UNKNOWN` becomes a CAS write timeout. Follows
//! `org.apache.cassandra.transport.messages.ErrorMessage`.

use std::net::IpAddr;

use bytes::{BufMut, Bytes, BytesMut};

use crate::consistency::Consistency;
use crate::error::DecodeError;
use crate::version::{Ctx, Version};
use crate::wire::{PutExt, Reader};

/// Error codes.
pub mod code {
    pub const SERVER_ERROR: i32 = 0x0000;
    pub const PROTOCOL_ERROR: i32 = 0x000A;
    pub const BAD_CREDENTIALS: i32 = 0x0100;
    pub const UNAVAILABLE: i32 = 0x1000;
    pub const OVERLOADED: i32 = 0x1001;
    pub const IS_BOOTSTRAPPING: i32 = 0x1002;
    pub const TRUNCATE_ERROR: i32 = 0x1003;
    pub const WRITE_TIMEOUT: i32 = 0x1100;
    pub const READ_TIMEOUT: i32 = 0x1200;
    pub const READ_FAILURE: i32 = 0x1300;
    pub const FUNCTION_FAILURE: i32 = 0x1400;
    pub const WRITE_FAILURE: i32 = 0x1500;
    pub const CDC_WRITE_FAILURE: i32 = 0x1600;
    pub const CAS_WRITE_UNKNOWN: i32 = 0x1700;
    pub const SYNTAX_ERROR: i32 = 0x2000;
    pub const UNAUTHORIZED: i32 = 0x2100;
    pub const INVALID: i32 = 0x2200;
    pub const CONFIG_ERROR: i32 = 0x2300;
    pub const ALREADY_EXISTS: i32 = 0x2400;
    pub const UNPREPARED: i32 = 0x2500;
    /// The code ScyllaDB 2026.3 advertises in `SCYLLA_RATE_LIMIT_ERROR` (`ERROR_CODE=61440`).
    pub const SCYLLA_RATE_LIMIT: i32 = 0xF000;

    /// Cassandra's `ExceptionCode` constant name.
    pub fn name(code: i32) -> Option<&'static str> {
        Some(match code {
            SERVER_ERROR => "SERVER_ERROR",
            PROTOCOL_ERROR => "PROTOCOL_ERROR",
            BAD_CREDENTIALS => "BAD_CREDENTIALS",
            UNAVAILABLE => "UNAVAILABLE",
            OVERLOADED => "OVERLOADED",
            IS_BOOTSTRAPPING => "IS_BOOTSTRAPPING",
            TRUNCATE_ERROR => "TRUNCATE_ERROR",
            WRITE_TIMEOUT => "WRITE_TIMEOUT",
            READ_TIMEOUT => "READ_TIMEOUT",
            READ_FAILURE => "READ_FAILURE",
            FUNCTION_FAILURE => "FUNCTION_FAILURE",
            WRITE_FAILURE => "WRITE_FAILURE",
            CDC_WRITE_FAILURE => "CDC_WRITE_FAILURE",
            CAS_WRITE_UNKNOWN => "CAS_WRITE_UNKNOWN",
            SYNTAX_ERROR => "SYNTAX_ERROR",
            UNAUTHORIZED => "UNAUTHORIZED",
            INVALID => "INVALID",
            CONFIG_ERROR => "CONFIG_ERROR",
            ALREADY_EXISTS => "ALREADY_EXISTS",
            UNPREPARED => "UNPREPARED",
            _ => return None,
        })
    }
}

/// `SIMPLE`, `BATCH`, `UNLOGGED_BATCH`, `COUNTER`, `BATCH_LOG`, `CAS`, `VIEW`, `CDC`; kept as
/// the string the peer sent.
pub type WriteType = String;

/// The fields that follow `<code><message>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorDetail {
    None,
    Unavailable { consistency: Consistency, required: i32, alive: i32 },
    /// `contentions` is v5's CAS contention count (only with `write_type == "CAS"`).
    WriteTimeout { consistency: Consistency, received: i32, block_for: i32, write_type: WriteType, contentions: Option<u16> },
    ReadTimeout { consistency: Consistency, received: i32, block_for: i32, data_present: bool },
    /// `num_failures` is sent before v5; v5 sends `reasons` (endpoint → reason code) instead.
    ReadFailure {
        consistency: Consistency,
        received: i32,
        block_for: i32,
        num_failures: i32,
        reasons: Vec<(IpAddr, u16)>,
        data_present: bool,
    },
    WriteFailure {
        consistency: Consistency,
        received: i32,
        block_for: i32,
        num_failures: i32,
        reasons: Vec<(IpAddr, u16)>,
        write_type: WriteType,
    },
    FunctionFailure { keyspace: String, function: String, arg_types: Vec<String> },
    /// `table` is empty for a keyspace.
    AlreadyExists { keyspace: String, table: String },
    Unprepared { id: Bytes },
    CasWriteUnknown { consistency: Consistency, received: i32, block_for: i32 },
    /// ScyllaDB's per-partition rate limit: `op_type` 0 read, 1 write.
    RateLimit { op_type: u8, rejected_by_coordinator: bool },
}

/// An ERROR body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorBody {
    pub code: i32,
    pub message: String,
    pub detail: ErrorDetail,
}

impl ErrorBody {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        ErrorBody { code, message: message.into(), detail: ErrorDetail::None }
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(code::PROTOCOL_ERROR, message)
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::new(code::SERVER_ERROR, message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(code::INVALID, message)
    }

    pub fn with(code: i32, message: impl Into<String>, detail: ErrorDetail) -> Self {
        ErrorBody { code, message: message.into(), detail }
    }

    /// The error as the given version can carry it.
    pub fn for_version(&self, version: Version) -> ErrorBody {
        use ErrorDetail as D;
        if version < Version::V4 {
            match &self.detail {
                D::ReadFailure { consistency, received, block_for, data_present, .. } => {
                    return ErrorBody::with(
                        code::READ_TIMEOUT,
                        self.message.clone(),
                        D::ReadTimeout {
                            consistency: *consistency,
                            received: *received,
                            block_for: *block_for,
                            data_present: *data_present,
                        },
                    );
                }
                D::WriteFailure { consistency, received, block_for, write_type, .. } => {
                    return ErrorBody::with(
                        code::WRITE_TIMEOUT,
                        self.message.clone(),
                        D::WriteTimeout {
                            consistency: *consistency,
                            received: *received,
                            block_for: *block_for,
                            write_type: write_type.clone(),
                            contentions: None,
                        },
                    );
                }
                _ => {}
            }
            if self.code == code::FUNCTION_FAILURE || self.code == code::CDC_WRITE_FAILURE {
                // InvalidRequestException(ErrorMessage.toString())
                let name = code::name(self.code).unwrap_or_default();
                return ErrorBody::invalid(format!("ERROR {name}: {}", self.message));
            }
        }
        if version < Version::V5 {
            match &self.detail {
                D::WriteTimeout { contentions: Some(_), consistency, received, block_for, write_type } => {
                    return ErrorBody::with(
                        self.code,
                        self.message.clone(),
                        D::WriteTimeout {
                            consistency: *consistency,
                            received: *received,
                            block_for: *block_for,
                            write_type: write_type.clone(),
                            contentions: None,
                        },
                    );
                }
                D::CasWriteUnknown { consistency, received, block_for } => {
                    return ErrorBody::with(
                        code::WRITE_TIMEOUT,
                        self.message.clone(),
                        D::WriteTimeout {
                            consistency: *consistency,
                            received: *received,
                            block_for: *block_for,
                            write_type: "CAS".into(),
                            contentions: None,
                        },
                    );
                }
                _ => {}
            }
        }
        self.clone()
    }

    pub fn encode(&self, ctx: impl Into<Ctx>, out: &mut BytesMut) {
        let ctx = ctx.into();
        let e = self.for_version(ctx.version);
        out.put_i32(e.code);
        out.put_string(&e.message);
        let v5 = ctx.version >= Version::V5;
        match &e.detail {
            ErrorDetail::None => {}
            ErrorDetail::Unavailable { consistency, required, alive } => {
                out.put_consistency(*consistency);
                out.put_i32(*required);
                out.put_i32(*alive);
            }
            ErrorDetail::WriteTimeout { consistency, received, block_for, write_type, contentions } => {
                out.put_consistency(*consistency);
                out.put_i32(*received);
                out.put_i32(*block_for);
                out.put_string(write_type);
                if let (true, Some(c)) = (v5, contentions) {
                    out.put_u16(*c);
                }
            }
            ErrorDetail::ReadTimeout { consistency, received, block_for, data_present } => {
                out.put_consistency(*consistency);
                out.put_i32(*received);
                out.put_i32(*block_for);
                out.put_u8(*data_present as u8);
            }
            ErrorDetail::ReadFailure { consistency, received, block_for, num_failures, reasons, data_present } => {
                put_failure(out, v5, *consistency, *received, *block_for, *num_failures, reasons);
                out.put_u8(*data_present as u8);
            }
            ErrorDetail::WriteFailure { consistency, received, block_for, num_failures, reasons, write_type } => {
                put_failure(out, v5, *consistency, *received, *block_for, *num_failures, reasons);
                out.put_string(write_type);
            }
            ErrorDetail::FunctionFailure { keyspace, function, arg_types } => {
                out.put_string(keyspace);
                out.put_string(function);
                out.put_string_list(arg_types);
            }
            ErrorDetail::AlreadyExists { keyspace, table } => {
                out.put_string(keyspace);
                out.put_string(table);
            }
            ErrorDetail::Unprepared { id } => out.put_short_bytes(id),
            ErrorDetail::CasWriteUnknown { consistency, received, block_for } => {
                out.put_consistency(*consistency);
                out.put_i32(*received);
                out.put_i32(*block_for);
            }
            ErrorDetail::RateLimit { op_type, rejected_by_coordinator } => {
                out.put_u8(*op_type);
                out.put_u8(*rejected_by_coordinator as u8);
            }
        }
    }

    pub fn decode(ctx: impl Into<Ctx>, r: &mut Reader) -> Result<Self, DecodeError> {
        let ctx = ctx.into();
        let code = r.i32()?;
        let message = r.string()?;
        let v5 = ctx.version >= Version::V5;
        let detail = match code {
            code::UNAVAILABLE => ErrorDetail::Unavailable { consistency: r.consistency()?, required: r.i32()?, alive: r.i32()? },
            code::WRITE_TIMEOUT => {
                let (consistency, received, block_for) = (r.consistency()?, r.i32()?, r.i32()?);
                let write_type = r.string()?;
                let contentions = if v5 && write_type == "CAS" && r.remaining() >= 2 { Some(r.u16()?) } else { None };
                ErrorDetail::WriteTimeout { consistency, received, block_for, write_type, contentions }
            }
            code::READ_TIMEOUT => ErrorDetail::ReadTimeout {
                consistency: r.consistency()?,
                received: r.i32()?,
                block_for: r.i32()?,
                data_present: r.u8()? != 0,
            },
            code::READ_FAILURE | code::WRITE_FAILURE => {
                let (consistency, received, block_for) = (r.consistency()?, r.i32()?, r.i32()?);
                let num_failures = r.i32()?;
                let mut reasons = Vec::new();
                if v5 {
                    // Each entry is at least 1 + 4 + 2 bytes.
                    reasons.reserve((num_failures.max(0) as usize).min(r.remaining() / 7));
                    for _ in 0..num_failures.max(0) {
                        let ip = r.inetaddr()?;
                        reasons.push((ip, r.u16()?));
                    }
                }
                if code == code::READ_FAILURE {
                    ErrorDetail::ReadFailure { consistency, received, block_for, num_failures, reasons, data_present: r.u8()? != 0 }
                } else {
                    ErrorDetail::WriteFailure { consistency, received, block_for, num_failures, reasons, write_type: r.string()? }
                }
            }
            code::FUNCTION_FAILURE => {
                ErrorDetail::FunctionFailure { keyspace: r.string()?, function: r.string()?, arg_types: r.string_list()? }
            }
            code::ALREADY_EXISTS => ErrorDetail::AlreadyExists { keyspace: r.string()?, table: r.string()? },
            code::UNPREPARED => ErrorDetail::Unprepared { id: r.short_bytes()? },
            code::CAS_WRITE_UNKNOWN => {
                ErrorDetail::CasWriteUnknown { consistency: r.consistency()?, received: r.i32()?, block_for: r.i32()? }
            }
            c if Some(c) == ctx.features.rate_limit_code => {
                ErrorDetail::RateLimit { op_type: r.u8()?, rejected_by_coordinator: r.u8()? != 0 }
            }
            _ => ErrorDetail::None,
        };
        Ok(ErrorBody { code, message, detail })
    }
}

fn put_failure(
    out: &mut BytesMut,
    v5: bool,
    consistency: Consistency,
    received: i32,
    block_for: i32,
    num_failures: i32,
    reasons: &[(IpAddr, u16)],
) {
    out.put_consistency(consistency);
    out.put_i32(received);
    out.put_i32(block_for);
    if v5 {
        out.put_i32(reasons.len() as i32);
        for (ip, reason) in reasons {
            out.put_inetaddr(*ip);
            out.put_u16(*reason);
        }
    } else {
        out.put_i32(num_failures);
    }
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match code::name(self.code) {
            Some(n) => write!(f, "{n}: {}", self.message),
            None => write!(f, "0x{:04x}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for ErrorBody {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::Features;

    fn round(e: &ErrorBody, ctx: Ctx) -> ErrorBody {
        let mut b = BytesMut::new();
        e.encode(ctx, &mut b);
        let mut r = Reader::new(b.freeze());
        let back = ErrorBody::decode(ctx, &mut r).unwrap();
        assert!(r.is_empty());
        back
    }

    fn all() -> Vec<ErrorBody> {
        let c = Consistency::Quorum;
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        vec![
            ErrorBody::protocol("bad"),
            ErrorBody::with(code::UNAVAILABLE, "u", ErrorDetail::Unavailable { consistency: c, required: 3, alive: 1 }),
            ErrorBody::with(
                code::WRITE_TIMEOUT,
                "wt",
                ErrorDetail::WriteTimeout { consistency: c, received: 1, block_for: 2, write_type: "SIMPLE".into(), contentions: None },
            ),
            ErrorBody::with(
                code::READ_TIMEOUT,
                "rt",
                ErrorDetail::ReadTimeout { consistency: c, received: 1, block_for: 2, data_present: true },
            ),
            ErrorBody::with(
                code::READ_FAILURE,
                "rf",
                ErrorDetail::ReadFailure {
                    consistency: c,
                    received: 1,
                    block_for: 2,
                    num_failures: 1,
                    reasons: vec![(ip, 1)],
                    data_present: false,
                },
            ),
            ErrorBody::with(
                code::WRITE_FAILURE,
                "wf",
                ErrorDetail::WriteFailure {
                    consistency: c,
                    received: 1,
                    block_for: 2,
                    num_failures: 1,
                    reasons: vec![(ip, 0)],
                    write_type: "BATCH".into(),
                },
            ),
            ErrorBody::with(
                code::FUNCTION_FAILURE,
                "ff",
                ErrorDetail::FunctionFailure { keyspace: "ks".into(), function: "f".into(), arg_types: vec!["int".into()] },
            ),
            ErrorBody::with(code::ALREADY_EXISTS, "ae", ErrorDetail::AlreadyExists { keyspace: "ks".into(), table: "".into() }),
            ErrorBody::with(code::UNPREPARED, "up", ErrorDetail::Unprepared { id: Bytes::from_static(b"\x01\x02") }),
            ErrorBody::with(
                code::CAS_WRITE_UNKNOWN,
                "cas",
                ErrorDetail::CasWriteUnknown { consistency: Consistency::Serial, received: 0, block_for: 1 },
            ),
            ErrorBody::with(
                code::WRITE_TIMEOUT,
                "cas timeout",
                ErrorDetail::WriteTimeout { consistency: c, received: 0, block_for: 1, write_type: "CAS".into(), contentions: Some(4) },
            ),
        ]
    }

    #[test]
    fn v5_round_trips_everything() {
        for e in all() {
            assert_eq!(round(&e, Ctx::new(Version::V5)), e);
        }
    }

    #[test]
    fn v4_drops_v5_fields() {
        let ctx = Ctx::new(Version::V4);
        for e in all() {
            let back = round(&e, ctx);
            match &e.detail {
                ErrorDetail::ReadFailure { reasons, .. } | ErrorDetail::WriteFailure { reasons, .. } => {
                    assert!(!reasons.is_empty());
                    assert!(matches!(&back.detail, ErrorDetail::ReadFailure { reasons, .. } | ErrorDetail::WriteFailure { reasons, .. } if reasons.is_empty()));
                }
                ErrorDetail::CasWriteUnknown { .. } => {
                    assert_eq!(back.code, code::WRITE_TIMEOUT);
                    assert!(matches!(&back.detail, ErrorDetail::WriteTimeout { write_type, .. } if write_type == "CAS"));
                }
                ErrorDetail::WriteTimeout { contentions: Some(_), .. } => {
                    assert!(matches!(&back.detail, ErrorDetail::WriteTimeout { contentions: None, .. }));
                }
                _ => assert_eq!(back, e),
            }
        }
    }

    #[test]
    fn v3_downgrades_failures() {
        let ctx = Ctx::new(Version::V3);
        for e in all() {
            let back = round(&e, ctx);
            match e.code {
                code::READ_FAILURE => assert_eq!(back.code, code::READ_TIMEOUT),
                code::WRITE_FAILURE => assert_eq!(back.code, code::WRITE_TIMEOUT),
                code::FUNCTION_FAILURE => {
                    assert_eq!(back, ErrorBody::invalid("ERROR FUNCTION_FAILURE: ff"));
                }
                _ => {}
            }
        }
    }

    #[test]
    fn simple_codes_and_unknown_codes() {
        for c in [
            code::SERVER_ERROR,
            code::PROTOCOL_ERROR,
            code::BAD_CREDENTIALS,
            code::OVERLOADED,
            code::IS_BOOTSTRAPPING,
            code::TRUNCATE_ERROR,
            code::CDC_WRITE_FAILURE,
            code::SYNTAX_ERROR,
            code::UNAUTHORIZED,
            code::INVALID,
            code::CONFIG_ERROR,
        ] {
            let e = ErrorBody::new(c, "m");
            assert_eq!(round(&e, Ctx::new(Version::V4)), e);
            assert_eq!(e.to_string(), format!("{}: m", code::name(c).unwrap()));
        }
        // CDC failures are Invalid on v3, as Cassandra sends them.
        assert_eq!(round(&ErrorBody::new(code::CDC_WRITE_FAILURE, "m"), Ctx::new(Version::V3)), ErrorBody::invalid("ERROR CDC_WRITE_FAILURE: m"));
        // An unknown code keeps its message; trailing bytes are ignored.
        let mut b = BytesMut::new();
        b.put_i32(0x7777);
        b.put_string("odd");
        b.put_slice(b"trailing");
        let e = ErrorBody::decode(Version::V4, &mut Reader::new(b.freeze())).unwrap();
        assert_eq!((e.code, e.message.as_str(), &e.detail), (0x7777, "odd", &ErrorDetail::None));
        assert_eq!(e.to_string(), "0x7777: odd");
        // Without the negotiated extension the rate-limit code has no body.
        let rl = ErrorBody::new(code::SCYLLA_RATE_LIMIT, "rl");
        assert_eq!(round(&rl, Ctx::new(Version::V4)), rl);
    }

    #[test]
    fn scylla_rate_limit() {
        let f = Features { rate_limit_code: Some(code::SCYLLA_RATE_LIMIT), ..Default::default() };
        let ctx = Ctx::new(Version::V4).features(f);
        let e = ErrorBody::with(
            code::SCYLLA_RATE_LIMIT,
            "Per-partition rate limit reached",
            ErrorDetail::RateLimit { op_type: 1, rejected_by_coordinator: true },
        );
        assert_eq!(round(&e, ctx), e);
    }

    #[test]
    fn truncated_bodies_fail_cleanly() {
        for e in all() {
            let mut b = BytesMut::new();
            e.encode(Version::V5, &mut b);
            let full = b.freeze();
            for cut in 0..full.len() {
                let mut r = Reader::new(full.slice(..cut));
                // Either a clean error or, where the cut drops only optional trailing data, a value.
                let _ = ErrorBody::decode(Version::V5, &mut r);
            }
        }
        // A huge failure count with nothing behind it.
        let mut b = BytesMut::new();
        b.put_i32(code::READ_FAILURE);
        b.put_string("x");
        b.put_u16(1);
        b.put_i32(0);
        b.put_i32(0);
        b.put_i32(i32::MAX);
        assert!(ErrorBody::decode(Version::V5, &mut Reader::new(b.freeze())).is_err());
    }
}
