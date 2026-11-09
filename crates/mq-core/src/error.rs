//! AMQP reply codes. Every protocol maps these onto its own errors.

use std::fmt;

pub const REPLY_SUCCESS: u16 = 200;
pub const CONTENT_TOO_LARGE: u16 = 311;
pub const NO_ROUTE: u16 = 312;
pub const NO_CONSUMERS: u16 = 313;
pub const CONNECTION_FORCED: u16 = 320;
pub const INVALID_PATH: u16 = 402;
pub const ACCESS_REFUSED: u16 = 403;
pub const NOT_FOUND: u16 = 404;
pub const RESOURCE_LOCKED: u16 = 405;
pub const PRECONDITION_FAILED: u16 = 406;
pub const FRAME_ERROR: u16 = 501;
pub const SYNTAX_ERROR: u16 = 502;
pub const COMMAND_INVALID: u16 = 503;
pub const CHANNEL_ERROR: u16 = 504;
pub const UNEXPECTED_FRAME: u16 = 505;
pub const RESOURCE_ERROR: u16 = 506;
pub const NOT_ALLOWED: u16 = 530;
pub const NOT_IMPLEMENTED: u16 = 540;
pub const INTERNAL_ERROR: u16 = 541;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AmqpError {
    pub code: u16,
    pub text: String,
}

impl AmqpError {
    pub fn new(code: u16, text: impl Into<String>) -> Self {
        Self { code, text: text.into() }
    }

    /// Channel errors close the channel; everything else closes the connection.
    pub fn is_channel_error(&self) -> bool {
        matches!(self.code, ACCESS_REFUSED | NOT_FOUND | RESOURCE_LOCKED | PRECONDITION_FAILED | CONTENT_TOO_LARGE | NO_ROUTE | NO_CONSUMERS)
    }

    pub fn name(&self) -> &'static str {
        match self.code {
            CONTENT_TOO_LARGE => "CONTENT_TOO_LARGE",
            NO_ROUTE => "NO_ROUTE",
            NO_CONSUMERS => "NO_CONSUMERS",
            CONNECTION_FORCED => "CONNECTION_FORCED",
            INVALID_PATH => "INVALID_PATH",
            ACCESS_REFUSED => "ACCESS_REFUSED",
            NOT_FOUND => "NOT_FOUND",
            RESOURCE_LOCKED => "RESOURCE_LOCKED",
            PRECONDITION_FAILED => "PRECONDITION_FAILED",
            FRAME_ERROR => "FRAME_ERROR",
            SYNTAX_ERROR => "SYNTAX_ERROR",
            COMMAND_INVALID => "COMMAND_INVALID",
            CHANNEL_ERROR => "CHANNEL_ERROR",
            UNEXPECTED_FRAME => "UNEXPECTED_FRAME",
            RESOURCE_ERROR => "RESOURCE_ERROR",
            NOT_ALLOWED => "NOT_ALLOWED",
            NOT_IMPLEMENTED => "NOT_IMPLEMENTED",
            INTERNAL_ERROR => "INTERNAL_ERROR",
            _ => "REPLY_SUCCESS",
        }
    }

    /// RabbitMQ's text format: `NOT_FOUND - no queue 'q' in vhost '/'`.
    pub fn reply_text(&self) -> String {
        let t = format!("{} - {}", self.name(), self.text);
        // Reply text is a short string.
        if t.len() > 255 { t[..t.floor_char_boundary(255)].to_string() } else { t }
    }

    pub fn not_found(what: impl fmt::Display) -> Self {
        Self::new(NOT_FOUND, format!("no {what}"))
    }
    pub fn access_refused(text: impl Into<String>) -> Self {
        Self::new(ACCESS_REFUSED, text)
    }
    pub fn precondition(text: impl Into<String>) -> Self {
        Self::new(PRECONDITION_FAILED, text)
    }
    pub fn locked(text: impl Into<String>) -> Self {
        Self::new(RESOURCE_LOCKED, text)
    }
    pub fn internal(text: impl fmt::Display) -> Self {
        Self::new(INTERNAL_ERROR, text.to_string())
    }
    pub fn command_invalid(text: impl Into<String>) -> Self {
        Self::new(COMMAND_INVALID, text)
    }
    pub fn not_allowed(text: impl Into<String>) -> Self {
        Self::new(NOT_ALLOWED, text)
    }
}

impl fmt::Display for AmqpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reply_text())
    }
}

impl std::error::Error for AmqpError {}

impl From<anyhow::Error> for AmqpError {
    fn from(e: anyhow::Error) -> Self {
        AmqpError::internal(format!("{e:#}"))
    }
}

pub type Result<T, E = AmqpError> = std::result::Result<T, E>;
