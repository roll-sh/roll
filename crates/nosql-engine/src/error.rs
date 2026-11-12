//! The engine's error: one variant per native-protocol error code, with every code-specific field
//! the ERROR body carries (`org.apache.cassandra.transport.messages.ErrorMessage`).
//!
//! Messages are the reference's exact text for the database's dialect; dialect-specific texts are
//! built in [`crate::messages`], never inline at the call site.

use bytes::Bytes;

/// The consistency levels (protocol codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum Consistency {
    Any,
    #[default]
    One,
    Two,
    Three,
    Quorum,
    All,
    LocalQuorum,
    EachQuorum,
    Serial,
    LocalSerial,
    LocalOne,
    /// `NODE_LOCAL` (11): this replica only.
    NodeLocal,
}

impl Consistency {
    pub fn code(self) -> u16 {
        match self {
            Consistency::Any => 0,
            Consistency::One => 1,
            Consistency::Two => 2,
            Consistency::Three => 3,
            Consistency::Quorum => 4,
            Consistency::All => 5,
            Consistency::LocalQuorum => 6,
            Consistency::EachQuorum => 7,
            Consistency::Serial => 8,
            Consistency::LocalSerial => 9,
            Consistency::LocalOne => 10,
            Consistency::NodeLocal => 11,
        }
    }

    pub fn from_code(c: u16) -> Option<Self> {
        Some(match c {
            0 => Consistency::Any,
            1 => Consistency::One,
            2 => Consistency::Two,
            3 => Consistency::Three,
            4 => Consistency::Quorum,
            5 => Consistency::All,
            6 => Consistency::LocalQuorum,
            7 => Consistency::EachQuorum,
            8 => Consistency::Serial,
            9 => Consistency::LocalSerial,
            10 => Consistency::LocalOne,
            11 => Consistency::NodeLocal,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Consistency::Any => "ANY",
            Consistency::One => "ONE",
            Consistency::Two => "TWO",
            Consistency::Three => "THREE",
            Consistency::Quorum => "QUORUM",
            Consistency::All => "ALL",
            Consistency::LocalQuorum => "LOCAL_QUORUM",
            Consistency::EachQuorum => "EACH_QUORUM",
            Consistency::Serial => "SERIAL",
            Consistency::LocalSerial => "LOCAL_SERIAL",
            Consistency::LocalOne => "LOCAL_ONE",
            Consistency::NodeLocal => "NODE_LOCAL",
        }
    }

    pub fn is_serial(self) -> bool {
        matches!(self, Consistency::Serial | Consistency::LocalSerial)
    }
}

/// Error codes (spec section 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    Server,
    Protocol,
    BadCredentials,
    Unavailable,
    Overloaded,
    IsBootstrapping,
    Truncate,
    WriteTimeout,
    ReadTimeout,
    ReadFailure,
    FunctionFailure,
    WriteFailure,
    CdcWriteFailure,
    CasWriteUnknown,
    Syntax,
    Unauthorized,
    Invalid,
    Config,
    AlreadyExists,
    Unprepared,
    /// ScyllaDB's per-partition rate limit error (`0xF000`).
    RateLimit,
}

impl ErrorCode {
    /// The protocol code.
    pub fn code(self) -> i32 {
        match self {
            ErrorCode::Server => 0x0000,
            ErrorCode::Protocol => 0x000A,
            ErrorCode::BadCredentials => 0x0100,
            ErrorCode::Unavailable => 0x1000,
            ErrorCode::Overloaded => 0x1001,
            ErrorCode::IsBootstrapping => 0x1002,
            ErrorCode::Truncate => 0x1003,
            ErrorCode::WriteTimeout => 0x1100,
            ErrorCode::ReadTimeout => 0x1200,
            ErrorCode::ReadFailure => 0x1300,
            ErrorCode::FunctionFailure => 0x1400,
            ErrorCode::WriteFailure => 0x1500,
            ErrorCode::CdcWriteFailure => 0x1600,
            ErrorCode::CasWriteUnknown => 0x1700,
            ErrorCode::Syntax => 0x2000,
            ErrorCode::Unauthorized => 0x2100,
            ErrorCode::Invalid => 0x2200,
            ErrorCode::Config => 0x2300,
            ErrorCode::AlreadyExists => 0x2400,
            ErrorCode::Unprepared => 0x2500,
            ErrorCode::RateLimit => 0xF000,
        }
    }
}

/// The code-specific part of an ERROR body.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ErrorDetail {
    #[default]
    None,
    Unavailable {
        consistency: Consistency,
        required: i32,
        alive: i32,
    },
    WriteTimeout {
        consistency: Consistency,
        received: i32,
        block_for: i32,
        write_type: String,
        contentions: Option<u16>,
    },
    ReadTimeout {
        consistency: Consistency,
        received: i32,
        block_for: i32,
        data_present: bool,
    },
    /// `reasons` is v5's endpoint → failure code map (`(address, code)`).
    ReadFailure {
        consistency: Consistency,
        received: i32,
        block_for: i32,
        num_failures: i32,
        reasons: Vec<(std::net::IpAddr, u16)>,
        data_present: bool,
    },
    WriteFailure {
        consistency: Consistency,
        received: i32,
        block_for: i32,
        num_failures: i32,
        reasons: Vec<(std::net::IpAddr, u16)>,
        write_type: String,
    },
    FunctionFailure {
        keyspace: String,
        function: String,
        arg_types: Vec<String>,
    },
    /// `table` is empty for a keyspace.
    AlreadyExists {
        keyspace: String,
        table: String,
    },
    Unprepared {
        id: Bytes,
    },
    CasWriteUnknown {
        consistency: Consistency,
        received: i32,
        block_for: i32,
    },
    RateLimit {
        op_type: u8,
        rejected_by_coordinator: bool,
    },
}

/// An error answered to the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: ErrorCode,
    pub message: String,
    pub detail: ErrorDetail,
    /// Warnings collected before the error (sent with the ERROR frame, as the references do).
    pub warnings: Vec<String>,
    /// The trace session of a traced request that failed (sent with the ERROR frame: both
    /// references trace failed requests too).
    pub tracing_id: Option<[u8; 16]>,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} (0x{:04X}): {}",
            self.code,
            self.code.code(),
            self.message
        )
    }
}

impl std::error::Error for Error {}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
            detail: ErrorDetail::None,
            warnings: Vec::new(),
            tracing_id: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }

    pub fn syntax(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Syntax, message)
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Config, message)
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Server, message)
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Protocol, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthorized, message)
    }

    pub fn already_exists(message: impl Into<String>, keyspace: &str, table: &str) -> Self {
        Error {
            detail: ErrorDetail::AlreadyExists {
                keyspace: keyspace.into(),
                table: table.into(),
            },
            ..Self::new(ErrorCode::AlreadyExists, message)
        }
    }

    pub fn unprepared(id: &[u8]) -> Self {
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        Error {
            detail: ErrorDetail::Unprepared {
                id: Bytes::copy_from_slice(id),
            },
            ..Self::new(
                ErrorCode::Unprepared,
                format!(
                    "Prepared query with ID {hex} not found (either the query was not prepared on this host (maybe the host has been restarted?) or you have prepared too many queries and it has been evicted from the internal cache)"
                ),
            )
        }
    }

    pub fn function_failure(
        message: impl Into<String>,
        keyspace: &str,
        function: &str,
        arg_types: Vec<String>,
    ) -> Self {
        Error {
            detail: ErrorDetail::FunctionFailure {
                keyspace: keyspace.into(),
                function: function.into(),
                arg_types,
            },
            ..Self::new(ErrorCode::FunctionFailure, message)
        }
    }

    /// An internal failure (storage, I/O): a server error with the cause's text.
    pub fn internal(e: impl std::fmt::Display) -> Self {
        Self::server(e.to_string())
    }

    pub fn with_warnings(mut self, warnings: Vec<String>) -> Self {
        self.warnings = warnings;
        self
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Shorthand for `Err(Error::invalid(..))`.
#[macro_export]
macro_rules! invalid {
    ($($arg:tt)*) => { $crate::error::Error::invalid(format!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_constructors() {
        assert_eq!(ErrorCode::Invalid.code(), 0x2200);
        assert_eq!(ErrorCode::RateLimit.code(), 0xF000);
        let e = Error::already_exists("x", "ks", "");
        assert_eq!(
            e.detail,
            ErrorDetail::AlreadyExists {
                keyspace: "ks".into(),
                table: "".into()
            }
        );
        let u = Error::unprepared(&[0xab, 0x01]);
        assert!(
            u.message
                .starts_with("Prepared query with ID ab01 not found")
        );
        for c in 0..=11 {
            assert_eq!(Consistency::from_code(c).unwrap().code(), c);
        }
        assert_eq!(Consistency::from_code(12), None);
        assert_eq!(Consistency::NodeLocal.name(), "NODE_LOCAL");
        assert_eq!(Consistency::LocalQuorum.name(), "LOCAL_QUORUM");
    }
}
