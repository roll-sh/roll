//! Parse errors, with the exact text each reference server reports.

use std::fmt;

/// Which error a server sends for a statement that fails to parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// `SyntaxException` (protocol error code 0x2000).
    Syntax,
    /// `InvalidRequestException` (0x2200): the checks Cassandra's parser actions throw as invalid
    /// requests (duplicate column, several primary keys, duplicate clustering order column).
    Invalid,
}

/// A statement that does not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyntaxError {
    pub code: ErrorCode,
    /// The message, exactly as the reference of the dialect formats it, for example
    /// `line 1:9 mismatched input 'FRM' expecting K_FROM (SELECT * [FRM]...)`.
    pub message: String,
    /// The offending position as reported (`line`, `column`), when the message has one.
    /// Cassandra reports end of input as line 0, column -1.
    pub position: Option<(i32, i32)>,
}

impl SyntaxError {
    pub(crate) fn custom(message: impl Into<String>) -> Self {
        SyntaxError { code: ErrorCode::Syntax, message: message.into(), position: None }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        SyntaxError { code: ErrorCode::Invalid, message: message.into(), position: None }
    }
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SyntaxError {}
