//! [`SelectError`]: S3 Select's error codes (the "List of SELECT Object Content Error Codes").

use std::fmt;

/// An S3 Select error: S3 Select's own error code (e.g. `ParseUnexpectedToken`), a message and
/// the HTTP status it maps to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectError {
    pub code: &'static str,
    pub message: String,
    pub status: u16,
}

impl SelectError {
    /// A client error (HTTP 400), the status of almost every S3 Select error.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        SelectError { code, message: message.into(), status: 400 }
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        SelectError::new("InternalError", message).with_status(500)
    }

    pub(crate) fn over_max_record() -> Self {
        SelectError::new(
            "OverMaxRecordSize",
            "The length of a record in the input or result is greater than maxCharsPerRecord of 1 MB.",
        )
    }

    pub(crate) fn cast_failed(detail: impl fmt::Display) -> Self {
        SelectError::new(
            "CastFailed",
            format!("Attempt to convert from one data type to another using CAST failed in the SQL expression: {detail}."),
        )
    }

    pub(crate) fn arg_type(func: &str) -> Self {
        SelectError::new(
            "IncorrectSqlFunctionArgumentType",
            format!("An incorrect argument type was specified in a function call in the SQL expression ({func})."),
        )
    }

    pub(crate) fn illegal_arg(func: &str, detail: impl fmt::Display) -> Self {
        SelectError::new("IllegalSqlFunctionArgument", format!("An illegal argument was used in the SQL function {func}: {detail}."))
    }

    pub(crate) fn overflow() -> Self {
        SelectError::new("IntegerOverflow", "An integer overflow or underflow occurred in the SQL expression.")
    }

    pub(crate) fn json(detail: impl fmt::Display) -> Self {
        SelectError::new("JSONParsingError", format!("Encountered an error parsing the JSON file: {detail}."))
    }

    pub(crate) fn encoding() -> Self {
        SelectError::new("InvalidTextEncoding", "Invalid encoding type. Only UTF-8 encoding is supported.")
    }
}

impl fmt::Display for SelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SelectError {}

pub(crate) type Result<T> = std::result::Result<T, SelectError>;
