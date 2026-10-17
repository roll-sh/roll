//! The HTTP API: Redis without a connection, for serverless and edge functions. Compatible with
//! Upstash's REST protocol, so `@upstash/redis` and similar SDKs work unchanged.
//!
//! - `POST /` with a JSON array (`["SET","k","v"]`) runs one command → `{"result": …}`.
//! - `GET|POST /set/k/v?EX=100` runs a command from the path; query params are appended
//!   (`EX 100`, a bare `?NX` → `NX`); a POST body is appended as the last argument (binary safe).
//! - `POST /pipeline` with `[[…],[…]]` → `[{"result": …}, {"error": …}]`, in order, not atomic.
//! - `POST /multi-exec` with `[[…],[…]]` runs them in a MULTI/EXEC transaction.
//! - `GET /subscribe/<channel>`, `/psubscribe/<pattern>`, `/monitor` stream Server-Sent Events.
//! - Auth: `Authorization: Bearer <token>` or `?_token=<token>`.
//! - `Upstash-Encoding: base64` base64-encodes strings in the response (binary data).
//! - `Upstash-Response-Format: resp2` returns raw RESP2 instead of JSON.
//!
//! Errors are `{"error": "ERR …"}` with status 400 (401 for bad tokens).

use crate::directory::{AuthRequest, Directory, Session};
use crate::exec::Detached;
use axum::Router;
use axum::body::Bytes as AxBytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::{Bytes, BytesMut};
use redis_engine::Reply;
use serde_json::{Value as Json, json};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::net::TcpListener;

#[derive(Clone)]
pub struct HttpState {
    pub dir: Arc<dyn Directory>,
    /// Request bodies larger than this are rejected (413).
    pub max_body: usize,
}

pub fn router(dir: Arc<dyn Directory>) -> Router {
    let state = HttpState { dir, max_body: 64 << 20 };
    Router::new()
        .fallback(handle)
        .layer(axum::extract::DefaultBodyLimit::max(state.max_body))
        .with_state(state)
}

pub async fn serve_http(listener: TcpListener, dir: Arc<dyn Directory>) -> anyhow::Result<()> {
    axum::serve(listener, router(dir).into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

/// Start the HTTP API on an ephemeral port (tests, embedding).
pub async fn start(dir: Arc<dyn Directory>) -> anyhow::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move { serve_http(listener, dir).await });
    Ok(addr)
}

struct Opts {
    base64: bool,
    resp2: bool,
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, axum::Json(json!({ "error": msg.into() }))).into_response()
}

async fn handle(
    State(st): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: AxBytes,
) -> Response {
    if method == Method::OPTIONS {
        return cors(StatusCode::NO_CONTENT.into_response());
    }
    let query = parse_query(uri.query().unwrap_or(""));
    let token = bearer(&headers).or_else(|| query.iter().find(|(k, _)| k == b"_token").and_then(|(_, v)| v.as_ref()).map(|v| String::from_utf8_lossy(v).into_owned()));
    let session = match authenticate(st.dir.as_ref(), token.as_deref()).await {
        Ok(s) => s,
        Err(r) => return cors(*r),
    };
    if session.db.closed.load(Ordering::Relaxed) {
        return cors(error(StatusCode::SERVICE_UNAVAILABLE, "LOADING the database is moving, retry"));
    }
    session.db.net_in.fetch_add(body.len() as u64, Ordering::Relaxed);
    let opts = Opts {
        base64: header_eq(&headers, "upstash-encoding", "base64"),
        resp2: header_eq(&headers, "upstash-response-format", "resp2"),
    };
    let path = uri.path();
    let segments: Vec<Bytes> = path.split('/').filter(|s| !s.is_empty()).map(|s| Bytes::from(percent_decode(s))).collect();
    let first = segments.first().map(|s| s.to_ascii_lowercase());
    let addr = peer.to_string();
    let resp = match first.as_deref() {
        None if method == Method::POST => match parse_command(&body) {
            Ok(cmd) => single(&session, &addr, cmd, &opts).await,
            Err(e) => error(StatusCode::BAD_REQUEST, e),
        },
        None => error(StatusCode::BAD_REQUEST, "ERR send a command as a JSON array in the body, e.g. [\"PING\"]"),
        Some(b"pipeline") if segments.len() == 1 => match parse_commands(&body) {
            Ok(cmds) => pipeline(&session, &addr, cmds, &opts).await,
            Err(e) => error(StatusCode::BAD_REQUEST, e),
        },
        Some(b"multi-exec") if segments.len() == 1 => match parse_commands(&body) {
            Ok(cmds) => multi_exec(&session, &addr, cmds, &opts).await,
            Err(e) => error(StatusCode::BAD_REQUEST, e),
        },
        Some(b"subscribe" | b"psubscribe" | b"ssubscribe") if segments.len() >= 2 => {
            let mut cmd = segments.clone();
            cmd[0] = Bytes::from(first.unwrap());
            stream(&session, &addr, cmd).await
        }
        Some(b"monitor") if segments.len() == 1 => stream(&session, &addr, vec![Bytes::from_static(b"MONITOR")]).await,
        Some(_) => {
            let mut cmd = segments;
            for (k, v) in query.into_iter().filter(|(k, _)| k != b"_token") {
                cmd.push(Bytes::from(k));
                if let Some(v) = v {
                    cmd.push(Bytes::from(v));
                }
            }
            if method == Method::POST && !body.is_empty() {
                cmd.push(Bytes::copy_from_slice(&body));
            }
            single(&session, &addr, cmd, &opts).await
        }
    };
    cors(resp)
}

fn cors(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, "authorization, content-type, upstash-encoding, upstash-response-format".parse().unwrap());
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST, OPTIONS".parse().unwrap());
    r
}

async fn authenticate(dir: &dyn Directory, token: Option<&str>) -> Result<Session, Box<Response>> {
    let unauthorized = || Box::new(error(StatusCode::UNAUTHORIZED, "Unauthorized"));
    match token {
        Some(t) => dir.authenticate(AuthRequest { user: None, password: t.as_bytes(), sni: None }).await.map_err(|_| unauthorized()),
        None => dir.anonymous().await.ok_or_else(unauthorized),
    }
}

fn bearer(h: &HeaderMap) -> Option<String> {
    let v = h.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = v.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        return Some(rest.trim().to_string());
    }
    if scheme.eq_ignore_ascii_case("basic") {
        // `user:password`, the password is the token.
        let raw = B64.decode(rest.trim()).ok()?;
        let s = String::from_utf8(raw).ok()?;
        return Some(s.split_once(':').map_or(s.clone(), |(_, p)| p.to_string()));
    }
    None
}

fn header_eq(h: &HeaderMap, name: &str, want: &str) -> bool {
    h.get(name).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case(want))
}

fn parse_query(q: &str) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (percent_decode(k), Some(percent_decode(v))),
            None => (percent_decode(p), None),
        })
        .collect()
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn json_arg(v: &Json) -> Result<Bytes, String> {
    Ok(match v {
        Json::String(s) => Bytes::copy_from_slice(s.as_bytes()),
        Json::Number(n) => Bytes::from(n.to_string()),
        Json::Bool(b) => Bytes::from_static(if *b { b"1" } else { b"0" }),
        _ => return Err("ERR command arguments must be strings, numbers or booleans".into()),
    })
}

fn json_command(v: &Json) -> Result<Vec<Bytes>, String> {
    let Json::Array(a) = v else { return Err("ERR a command must be a JSON array".into()) };
    if a.is_empty() {
        return Err("ERR empty command".into());
    }
    a.iter().map(json_arg).collect()
}

fn parse_command(body: &[u8]) -> Result<Vec<Bytes>, String> {
    let v: Json = serde_json::from_slice(body).map_err(|e| format!("ERR invalid JSON: {e}"))?;
    json_command(&v)
}

fn parse_commands(body: &[u8]) -> Result<Vec<Vec<Bytes>>, String> {
    let v: Json = serde_json::from_slice(body).map_err(|e| format!("ERR invalid JSON: {e}"))?;
    let Json::Array(a) = v else { return Err("ERR expected an array of commands".into()) };
    a.iter().map(json_command).collect()
}

/// RESP2 view of a reply as JSON (strings, integers, null, arrays).
pub fn to_json(r: &Reply, b64: bool) -> Json {
    let s = |b: &[u8]| -> Json {
        if b64 { Json::String(B64.encode(b)) } else { Json::String(String::from_utf8_lossy(b).into_owned()) }
    };
    match r {
        Reply::Ok => s(b"OK"),
        Reply::Status(x) => s(x.as_bytes()),
        Reply::Error(e) => json!({ "error": e }),
        Reply::Int(i) => json!(i),
        Reply::Bulk(b) => s(b),
        Reply::Nil | Reply::NilArray => Json::Null,
        Reply::Array(a) | Reply::Set(a) | Reply::Push(a) | Reply::Seq(a) => Json::Array(a.iter().map(|x| to_json(x, b64)).collect()),
        Reply::Map(m) => Json::Array(m.iter().flat_map(|(k, v)| [to_json(k, b64), to_json(v, b64)]).collect()),
        Reply::Double(d) => s(redis_proto::fmt_double(*d).as_bytes()),
        Reply::Bool(b) => json!(*b as i64),
        Reply::BigNum(n) => s(n.as_bytes()),
        Reply::Verbatim(_, b) => s(b),
        Reply::Raw(b) => match redis_proto::decode(b) {
            Ok(Some((r, _))) => to_json(&r, b64),
            _ => Json::Null,
        },
    }
}

fn result_obj(r: &Reply, b64: bool) -> Json {
    match error_text(r) {
        Some(e) => json!({ "error": e }),
        None => json!({ "result": to_json(r, b64) }),
    }
}

fn error_text(r: &Reply) -> Option<String> {
    match r {
        Reply::Error(e) => Some(e.clone()),
        Reply::Raw(b) if b.first() == Some(&b'-') => match redis_proto::decode(b) {
            Ok(Some((Reply::Error(e), _))) => Some(e),
            _ => None,
        },
        _ => None,
    }
}

fn reply_response(session: &Session, status: StatusCode, body: Json) -> Response {
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    session.db.net_out.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    (status, [(header::CONTENT_TYPE, "application/json")], bytes).into_response()
}

fn resp2_response(session: &Session, replies: &[Reply]) -> Response {
    let mut out = BytesMut::new();
    for r in replies {
        match r {
            Reply::Raw(b) => out.extend_from_slice(b),
            r => r.encode(2, &mut out),
        }
    }
    session.db.net_out.fetch_add(out.len() as u64, Ordering::Relaxed);
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/octet-stream")], out.freeze()).into_response()
}

async fn single(session: &Session, addr: &str, cmd: Vec<Bytes>, opts: &Opts) -> Response {
    let mut d = Detached::new(session, addr);
    let replies = d.run(vec![cmd]).await;
    if opts.resp2 {
        return resp2_response(session, &replies);
    }
    let r = &replies[0];
    match error_text(r) {
        Some(e) => reply_response(session, StatusCode::BAD_REQUEST, json!({ "error": e })),
        None => reply_response(session, StatusCode::OK, json!({ "result": to_json(r, opts.base64) })),
    }
}

async fn pipeline(session: &Session, addr: &str, cmds: Vec<Vec<Bytes>>, opts: &Opts) -> Response {
    let mut d = Detached::new(session, addr);
    let replies = d.run(cmds).await;
    if opts.resp2 {
        return resp2_response(session, &replies);
    }
    reply_response(session, StatusCode::OK, Json::Array(replies.iter().map(|r| result_obj(r, opts.base64)).collect()))
}

async fn multi_exec(session: &Session, addr: &str, cmds: Vec<Vec<Bytes>>, opts: &Opts) -> Response {
    let mut d = Detached::new(session, addr);
    let n = cmds.len();
    let mut all = Vec::with_capacity(n + 2);
    all.push(vec![Bytes::from_static(b"MULTI")]);
    all.extend(cmds);
    all.push(vec![Bytes::from_static(b"EXEC")]);
    let mut replies = d.run(all).await;
    let exec = replies.pop().unwrap_or(Reply::Nil);
    if let Some(e) = error_text(&exec) {
        // EXECABORT: report the queueing error that caused it.
        let cause = replies.iter().find_map(error_text).unwrap_or(e);
        return reply_response(session, StatusCode::BAD_REQUEST, json!({ "error": cause }));
    }
    if opts.resp2 {
        return resp2_response(session, std::slice::from_ref(&exec));
    }
    let items = match exec {
        Reply::Array(a) => a,
        _ => return reply_response(session, StatusCode::BAD_REQUEST, json!({ "error": "EXECABORT Transaction discarded" })),
    };
    reply_response(session, StatusCode::OK, Json::Array(items.iter().map(|r| result_obj(r, opts.base64)).collect()))
}

/// Server-Sent Events: `data: message,<channel>,<payload>` per message (Upstash's format).
async fn stream(session: &Session, addr: &str, cmd: Vec<Bytes>) -> Response {
    let mut d = Detached::new(session, addr);
    let replies = d.run(vec![cmd]).await;
    let mut first = Vec::new();
    for r in &replies {
        if let Some(e) = error_text(r) {
            return error(StatusCode::BAD_REQUEST, e);
        }
        match r {
            Reply::Seq(items) => first.extend(items.iter().map(sse_line)),
            Reply::Ok => first.push("OK".to_string()),
            r => first.push(sse_line(r)),
        }
    }
    let state = (d, first.into_iter());
    let s = futures::stream::unfold(state, |(mut d, mut pending)| async move {
        if let Some(line) = pending.next() {
            return Some((Ok::<_, Infallible>(Event::default().data(line)), (d, pending)));
        }
        let msg = d.recv_push().await?;
        let line = match redis_proto::decode(&msg) {
            Ok(Some((r, _))) => sse_line(&r),
            _ => String::from_utf8_lossy(&msg).into_owned(),
        };
        Some((Ok(Event::default().data(line)), (d, pending)))
    });
    Sse::new(s).keep_alive(KeepAlive::default()).into_response()
}

fn sse_line(r: &Reply) -> String {
    match r {
        Reply::Array(a) | Reply::Push(a) => {
            a.iter().map(|x| match to_json(x, false) {
                Json::String(s) => s,
                j => j.to_string(),
            })
            .collect::<Vec<_>>()
            .join(",")
        }
        Reply::Status(s) => s.clone(),
        r => match to_json(r, false) {
            Json::String(s) => s,
            j => j.to_string(),
        },
    }
}
