//! DynamoDB API errors, as Alternator reports them.
//!
//! The body is `{"__type":"com.amazonaws.dynamodb.v20120810#<Kind>","message":"<text>"}` with
//! the HTTP status of the kind. A failed condition may carry the old item
//! (`ReturnValuesOnConditionCheckFailure=ALL_OLD`).

use crate::json::Json;

/// The `__type` of an error, minus the `com.amazonaws.dynamodb.v20120810#` prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Validation,
    ResourceNotFound,
    ResourceInUse,
    ConditionalCheckFailed,
    UnknownOperation,
    Serialization,
    AccessDenied,
    UnrecognizedClient,
    InvalidSignature,
    MissingAuthenticationToken,
    LimitExceeded,
    RequestLimitExceeded,
    ExpiredIterator,
    TrimmedDataAccess,
    InternalServerError,
    PayloadTooLarge,
    TableNotFound,
}

impl ErrorKind {
    pub fn name(self) -> &'static str {
        match self {
            ErrorKind::Validation => "ValidationException",
            ErrorKind::ResourceNotFound => "ResourceNotFoundException",
            ErrorKind::ResourceInUse => "ResourceInUseException",
            ErrorKind::ConditionalCheckFailed => "ConditionalCheckFailedException",
            ErrorKind::UnknownOperation => "UnknownOperationException",
            ErrorKind::Serialization => "SerializationException",
            ErrorKind::AccessDenied => "AccessDeniedException",
            ErrorKind::UnrecognizedClient => "UnrecognizedClientException",
            ErrorKind::InvalidSignature => "InvalidSignatureException",
            ErrorKind::MissingAuthenticationToken => "MissingAuthenticationTokenException",
            ErrorKind::LimitExceeded => "LimitExceededException",
            ErrorKind::RequestLimitExceeded => "RequestLimitExceeded",
            ErrorKind::ExpiredIterator => "ExpiredIteratorException",
            ErrorKind::TrimmedDataAccess => "TrimmedDataAccessException",
            ErrorKind::InternalServerError => "InternalServerError",
            ErrorKind::PayloadTooLarge => "PayloadTooLarge",
            ErrorKind::TableNotFound => "TableNotFoundException",
        }
    }

    pub fn status(self) -> u16 {
        match self {
            ErrorKind::InternalServerError => 500,
            ErrorKind::PayloadTooLarge => 413,
            _ => 400,
        }
    }
}

/// An API error.
#[derive(Clone, Debug, PartialEq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    /// The old item, for a failed condition with `ReturnValuesOnConditionCheckFailure=ALL_OLD`.
    pub item: Option<Json>,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Error {
        Error {
            kind,
            message: message.into(),
            item: None,
        }
    }

    pub fn validation(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::Validation, message)
    }

    pub fn not_found(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::ResourceNotFound, message)
    }

    pub fn in_use(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::ResourceInUse, message)
    }

    pub fn internal(message: impl Into<String>) -> Error {
        Error::new(ErrorKind::InternalServerError, message)
    }

    pub fn condition_failed() -> Error {
        Error::new(
            ErrorKind::ConditionalCheckFailed,
            "The conditional request failed",
        )
    }

    pub fn type_name(&self) -> String {
        format!("com.amazonaws.dynamodb.v20120810#{}", self.kind.name())
    }

    pub fn status(&self) -> u16 {
        self.kind.status()
    }

    /// The response body.
    pub fn to_json(&self) -> Json {
        let mut j = Json::object()
            .with("__type", Json::str(self.type_name()))
            .with("message", Json::str(self.message.clone()));
        if let Some(item) = &self.item {
            j.push("Item", item.clone());
        }
        j
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.name(), self.message)
    }
}

impl std::error::Error for Error {}

/// `return Err(validation!(...))` shorthand.
#[macro_export]
#[doc(hidden)]
macro_rules! validation {
    ($($arg:tt)*) => {
        $crate::error::Error::validation(format!($($arg)*))
    };
}
