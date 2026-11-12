//! Building responses.

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use s3_proto::S3Error;

pub(crate) type Reply = Result<Response, S3Error>;

pub(crate) fn status(code: u16) -> StatusCode {
    StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

pub(crate) fn xml(code: StatusCode, body: String) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = code;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
    r
}

pub(crate) fn ok_xml(body: String) -> Reply {
    Ok(xml(StatusCode::OK, body))
}

pub(crate) fn empty(code: StatusCode) -> Response {
    let mut r = Response::new(Body::empty());
    *r.status_mut() = code;
    r
}

/// Set a header, skipping values that can't be sent (they were validated on the way in).
pub(crate) fn set(r: &mut Response, name: &str, value: &str) {
    if let (Ok(n), Ok(v)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
        r.headers_mut().insert(n, v);
    }
}

/// An error response: XML body, except for HEAD requests and 304s.
pub(crate) fn error(e: &S3Error, request_id: &str, head: bool) -> Response {
    let code = status(e.status());
    let mut r = if head || code == StatusCode::NOT_MODIFIED { empty(code) } else { xml(code, e.to_xml(request_id)) };
    if code == StatusCode::METHOD_NOT_ALLOWED {
        set(&mut r, "allow", "GET, PUT, POST, DELETE, HEAD");
    }
    for (name, value) in &e.headers {
        set(&mut r, name, value);
    }
    r
}
