//! API errors: `{"error": {"code": "not_found", "message": "..."}}` with a matching status.

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mq_core::AmqpError;
use mq_core::error::*;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// Extra top-level fields (e.g. per-receipt `results`).
    pub extra: Option<Map<String, Value>>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError { status, code, message: message.into(), extra: None }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "access_refused", message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }
    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message.to_string())
    }

    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.extra.get_or_insert_with(Map::new).insert(key.into(), value);
        self
    }

    /// The `{"code", "message"}` object.
    pub fn body(&self) -> Value {
        json!({"code": self.code, "message": self.message})
    }
}

impl From<AmqpError> for ApiError {
    fn from(e: AmqpError) -> Self {
        let (status, code) = match e.code {
            NOT_FOUND => (StatusCode::NOT_FOUND, "not_found"),
            ACCESS_REFUSED | NOT_ALLOWED => (StatusCode::FORBIDDEN, "access_refused"),
            RESOURCE_LOCKED => (StatusCode::CONFLICT, "resource_locked"),
            PRECONDITION_FAILED => (StatusCode::CONFLICT, "precondition_failed"),
            CONTENT_TOO_LARGE => (StatusCode::PAYLOAD_TOO_LARGE, "content_too_large"),
            NOT_IMPLEMENTED => (StatusCode::NOT_IMPLEMENTED, "not_implemented"),
            RESOURCE_ERROR => (StatusCode::SERVICE_UNAVAILABLE, "resource_error"),
            INTERNAL_ERROR => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            _ => (StatusCode::BAD_REQUEST, "bad_request"),
        };
        ApiError::new(status, code, e.reply_text())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::internal(format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = self.extra.clone().unwrap_or_default();
        body.insert("error".into(), self.body());
        let mut r = (self.status, axum::Json(Value::Object(body))).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"roll queues\", Bearer"));
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amqp_errors_map_to_status() {
        let e: ApiError = AmqpError::not_found("queue 'q' in vhost '/'").into();
        assert_eq!(e.status, StatusCode::NOT_FOUND);
        assert_eq!(e.message, "NOT_FOUND - no queue 'q' in vhost '/'");
        let e: ApiError = AmqpError::access_refused("nope").into();
        assert_eq!((e.status, e.code), (StatusCode::FORBIDDEN, "access_refused"));
        let e: ApiError = AmqpError::precondition("inequivalent arg").into();
        assert_eq!(e.status, StatusCode::CONFLICT);
    }
}
