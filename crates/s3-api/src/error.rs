//! Errors as Supabase Storage answers them: `{"statusCode": "404", "code": "NoSuchKey", "error":
//! "not_found", "message": "Object not found"}`. Like Supabase, the HTTP status of a client error
//! is 400 and the body's `statusCode` carries the real one (storage-js reads it from there);
//! internal errors are 500. TUS answers use the real status (the protocol needs it), and so do
//! a few routes Supabase answers that way too (304, 412, 416, 501).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use s3_proto::{Code, S3Error};
use s3_store::StoreError;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct ApiError {
    /// The HTTP status sent.
    pub http: u16,
    /// The body's `statusCode`.
    pub status: u16,
    pub code: &'static str,
    pub error: String,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: u16, code: &'static str, error: impl Into<String>, message: impl Into<String>) -> Self {
        let http = if status >= 500 { status } else { 400 };
        Self { http, status, code, error: error.into(), message: message.into() }
    }

    /// Send the real status (TUS, conditional reads).
    pub fn real(mut self) -> Self {
        self.http = self.status;
        self
    }

    pub fn with_http(mut self, http: u16) -> Self {
        self.http = http;
        self
    }

    pub fn no_such_bucket() -> Self {
        Self::new(404, "NoSuchBucket", "Bucket not found", "Bucket not found")
    }

    pub fn no_such_key() -> Self {
        Self::new(404, "NoSuchKey", "not_found", "Object not found")
    }

    pub fn no_such_upload() -> Self {
        Self::new(404, "NoSuchUpload", "NoSuchUpload", "Upload not found")
    }

    pub fn key_exists() -> Self {
        Self::new(409, "KeyAlreadyExists", "Duplicate", "The resource already exists")
    }

    pub fn bucket_exists() -> Self {
        Self::new(409, "BucketAlreadyExists", "Duplicate", "The resource already exists")
    }

    pub fn bucket_not_empty() -> Self {
        Self::new(409, "ResourceNotEmpty", "ResourceNotEmpty", "The bucket you tried to delete is not empty")
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(403, "AccessDenied", "Unauthorized", message)
    }

    /// What Supabase answers when row-level security refuses a write.
    pub fn rls() -> Self {
        Self::denied("new row violates row-level security policy")
    }

    /// The service key is required (Supabase's default policies on `storage.buckets`).
    pub fn service_only() -> Self {
        Self::denied("new row violates row-level security policy: this needs the project's service key")
    }

    pub fn invalid_jwt(message: impl Into<String>) -> Self {
        Self::new(400, "InvalidJWT", "InvalidJWT", message)
    }

    pub fn too_large(entity: &str) -> Self {
        Self::new(413, "EntityTooLarge", "Payload too large", format!("The {entity} exceeded the maximum allowed size"))
    }

    pub fn mime(message: impl Into<String>) -> Self {
        Self::new(415, "InvalidMimeType", "invalid_mime_type", message)
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "InvalidRequest", "InvalidRequest", message)
    }

    pub fn invalid_key(key: &str) -> Self {
        Self::new(400, "InvalidKey", "InvalidKey", format!("Invalid key: {key}"))
    }

    pub fn invalid_bucket_name() -> Self {
        Self::new(400, "InvalidBucketName", "Invalid Input", "Bucket name invalid")
    }

    pub fn missing(param: &str) -> Self {
        Self::new(400, "MissingParameter", "MissingParameter", format!("Missing Required Parameter {param}"))
    }

    pub fn invalid_signature(message: impl Into<String>) -> Self {
        Self::new(400, "InvalidSignature", "InvalidSignature", message)
    }

    pub fn expired_token() -> Self {
        Self::new(400, "ExpiredToken", "ExpiredToken", "The provided token has expired.")
    }

    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(501, "NotImplemented", "Not Implemented", message)
    }

    pub fn internal() -> Self {
        Self::new(500, "InternalError", "Internal", "Internal Server Error")
    }

    pub fn unavailable() -> Self {
        Self::new(503, "ServiceUnavailable", "Service Unavailable", "The storage service is unavailable; try again")
    }

    pub fn into_response_for(self, head: bool) -> Response {
        let status = StatusCode::from_u16(self.http).unwrap_or(StatusCode::BAD_REQUEST);
        let body = json!({ "statusCode": self.status.to_string(), "code": self.code, "error": self.error, "message": self.message }).to_string();
        let mut r = if head { Response::new(axum::body::Body::empty()) } else { Response::new(axum::body::Body::from(body)) };
        *r.status_mut() = status;
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
        if status == StatusCode::NOT_MODIFIED {
            *r.body_mut() = axum::body::Body::empty();
        }
        r
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.into_response_for(false)
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NoSuchBucket(_) => Self::no_such_bucket(),
            StoreError::BucketExists(_) => Self::bucket_exists(),
            StoreError::BucketNotEmpty(_) => Self::bucket_not_empty(),
            StoreError::BucketHasBranches(b) => Self::new(409, "ResourceNotEmpty", "ResourceNotEmpty", format!("Bucket {b} has branches; delete them first")),
            StoreError::NoSuchKey(_) | StoreError::NotFound(_) | StoreError::NoSuchVersion(..) | StoreError::DeleteMarker { .. } => Self::no_such_key(),
            StoreError::NoSuchUpload(_) => Self::no_such_upload(),
            StoreError::PreconditionFailed => Self::key_exists(),
            StoreError::InvalidBucketName(_) => Self::invalid_bucket_name(),
            StoreError::KeyTooLong => Self::new(400, "InvalidKey", "InvalidKey", "Invalid key: the key is longer than 1024 bytes"),
            StoreError::MetadataTooLarge => Self::too_large("user_metadata"),
            StoreError::Refused { code: s3_store::api::ENTITY_TOO_LARGE, .. } => Self::too_large("object"),
            StoreError::Refused { code: s3_store::api::INVALID_MIME_TYPE, message } => Self::mime(message),
            StoreError::Refused { message, .. } => Self::invalid(message),
            StoreError::ObjectLocked(m) => Self::new(423, "ResourceLocked", "ResourceLocked", m),
            StoreError::InvalidPart(_) | StoreError::InvalidPartOrder | StoreError::EntityTooSmall(_) => Self::invalid(e.to_string()),
            StoreError::InvalidArgument(m) | StoreError::InvalidRequest(m) | StoreError::InvalidBucketState(m) | StoreError::LimitExceeded(m) | StoreError::DeleteConflict(m) => Self::invalid(m),
            StoreError::Conflict(m) => Self::new(409, "InvalidRequest", "InvalidRequest", m),
            StoreError::Unavailable(why) => {
                tracing::error!(why, "storage unavailable");
                Self::unavailable()
            }
            StoreError::Internal(e) => {
                tracing::error!(error = %e, "storage API error");
                Self::internal()
            }
        }
    }
}

impl From<S3Error> for ApiError {
    fn from(e: S3Error) -> Self {
        match e.code {
            Code::AccessDenied => Self::denied(e.message),
            Code::NoSuchBucket => Self::no_such_bucket(),
            Code::NoSuchKey => Self::no_such_key(),
            Code::EntityTooLarge => Self::too_large("object"),
            Code::InternalError => Self::internal(),
            Code::ServiceUnavailable => Self::unavailable(),
            _ => Self::invalid(e.message),
        }
    }
}
