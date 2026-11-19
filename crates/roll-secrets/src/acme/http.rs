//! ACME's routes, `/v1/secrets/acme/{ca}/…`: no roll token (the account key's JWS and its binding
//! are the credential), every answer with a fresh `Replay-Nonce`, the directory as its `index`, and
//! errors as problem documents.

use super::{Answer, Body, Ep, JWS_MAX, Problem};
use crate::Secrets;
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use std::sync::Arc;

type S = State<Arc<Secrets>>;

pub(crate) fn routes() -> Router<Arc<Secrets>> {
    const A: &str = "/v1/secrets/acme/{ca}";
    Router::new()
        .route(&format!("{A}/directory"), get(directory))
        .route(&format!("{A}/new-nonce"), get(nonce_get).head(nonce_head))
        .route(&format!("{A}/new-account"), post(|State(s): S, Path(ca): Path<String>, r: Request| serve(s, ca, Ep::NewAccount, r)))
        .route(&format!("{A}/acct/{{id}}"), post(|State(s): S, Path((ca, id)): Path<(String, String)>, r: Request| serve(s, ca, Ep::Account(id), r)))
        .route(&format!("{A}/acct/{{id}}/orders"), post(|State(s): S, Path((ca, id)): Path<(String, String)>, r: Request| serve(s, ca, Ep::Orders(id), r)))
        .route(&format!("{A}/new-order"), post(|State(s): S, Path(ca): Path<String>, r: Request| serve(s, ca, Ep::NewOrder, r)))
        .route(&format!("{A}/order/{{id}}"), post(|State(s): S, Path((ca, id)): Path<(String, String)>, r: Request| serve(s, ca, Ep::Order(id), r)))
        .route(&format!("{A}/order/{{id}}/finalize"), post(|State(s): S, Path((ca, id)): Path<(String, String)>, r: Request| serve(s, ca, Ep::Finalize(id), r)))
        .route(&format!("{A}/authz/{{id}}"), post(|State(s): S, Path((ca, id)): Path<(String, String)>, r: Request| serve(s, ca, Ep::Authz(id), r)))
        .route(&format!("{A}/chall/{{id}}"), post(|State(s): S, Path((ca, id)): Path<(String, String)>, r: Request| serve(s, ca, Ep::Chall(id), r)))
        .route(&format!("{A}/cert/{{id}}"), post(|State(s): S, Path((ca, id)): Path<(String, String)>, r: Request| serve(s, ca, Ep::Cert(id), r)))
        .route(&format!("{A}/revoke-cert"), post(|State(s): S, Path(ca): Path<String>, r: Request| serve(s, ca, Ep::Revoke, r)))
        .route(&format!("{A}/key-change"), post(|State(s): S, Path(ca): Path<String>, r: Request| serve(s, ca, Ep::KeyChange, r)))
}

async fn directory(State(s): S, Path(ca): Path<String>) -> Response {
    let (out, nonce, index) = blocking(s, ca, |s, ca| s.acme_directory(ca).map(|d| Answer { status: 200, body: Body::Json(d), location: None, up: None })).await;
    respond(out, nonce, index)
}

/// `HEAD …/new-nonce` answers 200, `GET` 204 (RFC 8555 §7.2); both carry a nonce.
async fn nonce_head(State(s): S, Path(ca): Path<String>) -> Response {
    nonce(s, ca, 200).await
}

async fn nonce_get(State(s): S, Path(ca): Path<String>) -> Response {
    nonce(s, ca, 204).await
}

async fn nonce(s: Arc<Secrets>, ca: String, status: u16) -> Response {
    let (out, nonce, index) = blocking(s, ca, move |s, ca| s.acme_directory(ca).map(|_| Answer { status, body: Body::Empty, location: None, up: None })).await;
    respond(out, nonce, index)
}

async fn serve(s: Arc<Secrets>, ca: String, ep: Ep, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, JWS_MAX).await else {
        let (out, nonce, index) = blocking(s, ca, |_, _| Err(Problem::malformed(format!("an ACME request is {} KiB at most", JWS_MAX / 1024)))).await;
        return respond(out, nonce, index);
    };
    let (out, nonce, index) = blocking(s, ca, move |s, ca| {
        let ip = s.client_ip(&parts);
        let ct = parts.headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
        s.acme_post(ca, ep, parts.uri.path(), ct, ip, &bytes)
    })
    .await;
    respond(out, nonce, index)
}

/// Run `f` off the async runtime (the store blocks on its database, challenges on rolld's
/// checks), then a fresh nonce and the directory's URL for the answer's headers.
async fn blocking(s: Arc<Secrets>, ca: String, f: impl FnOnce(&Secrets, &str) -> Result<Answer, Problem> + Send + 'static) -> (Result<Answer, Problem>, Option<String>, Option<String>) {
    let done = tokio::task::spawn_blocking(move || {
        let out = f(&s, &ca);
        let nonce = s.acme_nonce().ok();
        let index = s.acme_base(&ca).ok().map(|b| format!("<{b}/directory>;rel=\"index\""));
        (out, nonce, index)
    })
    .await;
    done.unwrap_or_else(|e| (Err(Problem::from(crate::Error::Internal(e.to_string()))), None, None))
}

fn respond(out: Result<Answer, Problem>, nonce: Option<String>, index: Option<String>) -> Response {
    let status = |s: u16| StatusCode::from_u16(s).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut r = match out {
        Ok(a) => {
            let mut r = match a.body {
                Body::Json(j) => (status(a.status), [(header::CONTENT_TYPE, "application/json")], j.to_string()).into_response(),
                Body::Pem(p) => (status(a.status), [(header::CONTENT_TYPE, "application/pem-certificate-chain")], p).into_response(),
                Body::Empty => status(a.status).into_response(),
            };
            if let Some(l) = a.location.and_then(|l| HeaderValue::from_str(&l).ok()) {
                r.headers_mut().insert(header::LOCATION, l);
            }
            if let Some(up) = a.up.and_then(|u| HeaderValue::from_str(&format!("<{u}>;rel=\"up\"")).ok()) {
                r.headers_mut().append(header::LINK, up);
            }
            r
        }
        Err(p) => {
            let mut r = (status(p.status), [(header::CONTENT_TYPE, "application/problem+json")], p.body().to_string()).into_response();
            if let Some(l) = p.location.as_deref().and_then(|l| HeaderValue::from_str(l).ok()) {
                r.headers_mut().insert(header::LOCATION, l);
            }
            r
        }
    };
    let h = r.headers_mut();
    if let Some(n) = nonce.and_then(|n| HeaderValue::from_str(&n).ok()) {
        h.insert("replay-nonce", n);
    }
    if let Some(i) = index.and_then(|i| HeaderValue::from_str(&i).ok()) {
        h.append(header::LINK, i);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}
