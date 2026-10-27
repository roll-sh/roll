//! Serving Alternator over HTTP with axum (hyper).
//!
//! The adapter only moves bytes: a request larger than [`MAX_REQUEST_BYTES`] is refused with
//! 413 from its `Content-Length` before the body is read, or as soon as a chunked body passes
//! the limit; everything else is [`Alternator::handle`]'s.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request};
use axum::response::Response;

use crate::http::{HttpRequest, HttpResponse, MAX_REQUEST_BYTES, payload_too_large};
use crate::service::Alternator;

/// An axum router answering every path and method.
pub fn router(alt: Arc<Alternator>) -> axum::Router {
    axum::Router::new().fallback(move |req: Request| {
        let alt = alt.clone();
        async move { handle(&alt, req).await }
    })
}

/// Serves `alt` on `listener` until the listener fails.
pub async fn serve(listener: tokio::net::TcpListener, alt: Arc<Alternator>) -> std::io::Result<()> {
    axum::serve(
        listener,
        router(alt).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
}

/// Responses larger than this go out with chunked transfer encoding, as ScyllaDB streams them.
pub const CHUNKED_BYTES: usize = 100 * 1024;

/// An [`HttpResponse`] as an axum response.
pub fn to_axum(r: HttpResponse) -> Response {
    let mut b = Response::builder().status(r.status);
    for (k, v) in &r.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    let body = if r.body.len() > CHUNKED_BYTES {
        // A body of unknown length: hyper sends it chunked.
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = (0..r.body.len())
            .step_by(64 * 1024)
            .map(|i| Ok(r.body.slice(i..(i + 64 * 1024).min(r.body.len()))))
            .collect();
        Body::from_stream(futures::stream::iter(chunks))
    } else {
        Body::from(r.body)
    };
    b.body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

async fn handle(alt: &Alternator, req: Request) -> Response {
    match read_request(req).await {
        Ok(r) => to_axum(alt.handle(r).await),
        Err(resp) => resp,
    }
}

/// Reads an axum request into an [`HttpRequest`] (the body up to [`MAX_REQUEST_BYTES`]); a
/// larger one is answered with 413 at once.
// The error is the response to send (axum's `Response` is large by design).
#[allow(clippy::result_large_err)]
pub async fn read_request(req: Request) -> Result<HttpRequest, Response> {
    let (parts, body) = req.into_parts();
    let headers: Vec<(String, String)> = parts
        .headers
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    let too_large = || {
        let mut r = HttpResponse::error(&payload_too_large());
        r.headers.push(("Date".into(), crate::http::http_date()));
        to_axum(r)
    };
    let declared = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_REQUEST_BYTES as u64) {
        return Err(too_large());
    }
    let Ok(bytes) = axum::body::to_bytes(body, MAX_REQUEST_BYTES).await else {
        return Err(too_large());
    };
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    Ok(HttpRequest {
        method: parts.method.as_str().to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or("").to_string(),
        headers,
        body: bytes,
        peer,
    })
}
