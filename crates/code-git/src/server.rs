//! git smart HTTP server: `/git/<repo>.git/info/refs`, `/git-upload-pack` and
//! `/git-receive-pack`. `<repo>` can contain slashes (`org/repo`) and `.git` is optional.
//!
//! - upload-pack speaks protocol v2 (`ls-refs`, `fetch` with shallow and filter support) when the
//!   client sends `Git-Protocol: version=2`, and v0 otherwise (for libgit2, JGit and old gits).
//! - receive-pack speaks v0 with report-status, side-band-64k, atomic, delete-refs and push
//!   options. Rejections carry their reason to the user's terminal, and `remote:` lines carry
//!   details.
//!
//! Packs are streamed undeltified as objects are read, in batches. Pushed packs are unpacked as
//! they arrive, never buffered whole.
//!
//! Every request charges one [`MemoryBudget`] shared by the server for what it holds (see
//! [`crate::budget`]): request bodies as they're read, a push's inflated objects as unpacking
//! produces them, a fetch's planned ids, the objects it has read and the entry being written. A
//! push that can't get memory reads the rest of its body without holding it and gets
//! `unpack <reason>` (git prints `remote unpack failed: …`). A fetch that can't get memory gets
//! `503` with `Retry-After` if its response hasn't started, and a side-band error otherwise.

use crate::backend::{
    Access, Credentials, FetchRequest, Filter, GitBackend, GitError, ReceiveOutcome, ReceiveRequest, RefList, RefStatus,
    RefUpdate,
};
use crate::budget::{BudgetError, CancelOnDrop, Charge, Lease, LeaseMeter, MemoryBudget};
use crate::object::{Object, ObjectId};
use crate::pack::{self, Limits, Meter};
use crate::pktline::{self, BAND_DATA, BAND_ERROR, BAND_PROGRESS, Pkt, PktError, Reader};
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::{Buf, Bytes, BytesMut};
use futures::StreamExt;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub limits: Limits,
    /// Largest upload-pack request (wants and haves), and largest list of ref updates in a push.
    pub max_request_bytes: u64,
    /// Sent as `agent=` in capability advertisements.
    pub agent: String,
    /// Memory every request together may hold: request bodies, unpacked pushes, objects being
    /// sent. Default 1 GiB.
    pub memory_budget_bytes: u64,
    /// Most one request may hold; more fails it at once. A request past a small allowance can
    /// only grow while the total stays under this, so the rest of the budget is kept for small
    /// requests. Default (`None`): three quarters of the budget.
    pub max_request_memory_bytes: Option<u64>,
    /// How long a request waits for memory before it fails as busy. Default 30 s.
    pub memory_wait: Duration,
    /// A budget to use instead of one built from the three fields above: to share one between
    /// routers, or to watch it.
    pub memory_budget: Option<MemoryBudget>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            limits: Limits::default(),
            max_request_bytes: 64 << 20,
            agent: concat!("roll-code/", env!("CARGO_PKG_VERSION")).into(),
            memory_budget_bytes: 1 << 30,
            max_request_memory_bytes: None,
            memory_wait: Duration::from_secs(30),
            memory_budget: None,
        }
    }
}

impl ServerConfig {
    /// The budget the server charges.
    pub fn budget(&self) -> MemoryBudget {
        self.memory_budget.clone().unwrap_or_else(|| {
            let per_request = self.max_request_memory_bytes.unwrap_or(self.memory_budget_bytes / 4 * 3);
            MemoryBudget::new(self.memory_budget_bytes, per_request, self.memory_wait)
        })
    }
}

struct Server {
    backend: Arc<dyn GitBackend>,
    config: ServerConfig,
    budget: MemoryBudget,
}

/// The smart HTTP routes, under `/git/`.
pub fn router(backend: Arc<dyn GitBackend>, config: ServerConfig) -> Router {
    let budget = config.budget();
    Router::new().route("/git/{*path}", get(get_handler).post(post_handler)).with_state(Arc::new(Server { backend, config, budget }))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endpoint {
    InfoRefs,
    UploadPack,
    ReceivePack,
}

/// `org/repo.git/info/refs` → (`org/repo`, InfoRefs).
fn split_path(path: &str) -> Option<(String, Endpoint)> {
    let path = path.trim_start_matches('/');
    let (rest, ep) = if let Some(r) = path.strip_suffix("/info/refs") {
        (r, Endpoint::InfoRefs)
    } else if let Some(r) = path.strip_suffix("/git-upload-pack") {
        (r, Endpoint::UploadPack)
    } else {
        (path.strip_suffix("/git-receive-pack")?, Endpoint::ReceivePack)
    };
    let repo = rest.strip_suffix(".git").unwrap_or(rest).trim_end_matches('/');
    if repo.is_empty() || repo.split('/').any(|s| s.is_empty() || s == "." || s == "..") {
        return None;
    }
    Some((repo.to_string(), ep))
}

fn credentials(headers: &HeaderMap) -> Option<Credentials> {
    use base64::Engine;
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, value) = v.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = base64::engine::general_purpose::STANDARD.decode(value.trim()).ok()?;
        let s = String::from_utf8(decoded).ok()?;
        let (u, p) = s.split_once(':')?;
        Some(Credentials { username: u.into(), password: p.into() })
    } else if scheme.eq_ignore_ascii_case("bearer") {
        Some(Credentials { username: String::new(), password: value.trim().into() })
    } else {
        None
    }
}

fn wants_v2(headers: &HeaderMap) -> bool {
    headers
        .get_all("git-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(':'))
        .any(|p| p.trim() == "version=2")
}

fn error_response(e: GitError) -> Response {
    match e {
        GitError::NotFound(m) => (StatusCode::NOT_FOUND, format!("repository not found: {m}\n")).into_response(),
        GitError::Unauthorized => {
            let mut r = (StatusCode::UNAUTHORIZED, "authentication required\n").into_response();
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"roll code\""));
            r
        }
        GitError::Forbidden(m) => (StatusCode::FORBIDDEN, format!("{m}\n")).into_response(),
        GitError::Invalid(m) => (StatusCode::BAD_REQUEST, format!("{m}\n")).into_response(),
        GitError::Internal(e) => {
            tracing::error!(error = %e, "git request failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error\n").into_response()
        }
    }
}

/// No memory for a request whose response hasn't started.
fn memory_response(e: &BudgetError) -> Response {
    tracing::info!(error = %e, "git request refused: memory budget");
    match e {
        BudgetError::Busy => {
            let mut r = (StatusCode::SERVICE_UNAVAILABLE, format!("{e}\n")).into_response();
            r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            r
        }
        BudgetError::TooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, format!("{e}\n")).into_response(),
    }
}

fn git_response(content_type: &'static str, body: Body) -> Response {
    let mut r = Response::new(body);
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, max-age=0, must-revalidate"));
    r
}

async fn get_handler(
    State(s): State<Arc<Server>>,
    Path(path): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some((repo, Endpoint::InfoRefs)) = split_path(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (access, upload) = match q.get("service").map(String::as_str) {
        Some("git-upload-pack") => (Access::Read, true),
        Some("git-receive-pack") => (Access::Write, false),
        _ => return (StatusCode::FORBIDDEN, "the dumb HTTP protocol is not supported\n").into_response(),
    };
    if let Err(e) = s.backend.authorize(&repo, credentials(&headers).as_ref(), access).await {
        return error_response(e);
    }
    let mut b = BytesMut::new();
    pktline::write_line(&mut b, if upload { "# service=git-upload-pack" } else { "# service=git-receive-pack" });
    pktline::flush(&mut b);
    if upload && wants_v2(&headers) {
        pktline::write_line(&mut b, "version 2");
        pktline::write_line(&mut b, &format!("agent={}", s.config.agent));
        pktline::write_line(&mut b, "ls-refs=unborn");
        pktline::write_line(&mut b, "fetch=shallow filter");
        pktline::write_line(&mut b, "server-option");
        pktline::write_line(&mut b, "object-format=sha1");
        pktline::flush(&mut b);
    } else {
        let refs = match s.backend.refs(&repo).await {
            Ok(r) => r,
            Err(e) => return error_response(e),
        };
        let mut caps = if upload {
            "thin-pack side-band-64k ofs-delta shallow no-progress include-tag allow-tip-sha1-in-want allow-reachable-sha1-in-want filter".to_string()
        } else {
            "report-status delete-refs side-band-64k quiet atomic ofs-delta push-options".to_string()
        };
        if let Some(h) = &refs.head {
            caps.push_str(&format!(" symref=HEAD:{h}"));
        }
        caps.push_str(&format!(" object-format=sha1 agent={}", s.config.agent));
        advertise_v0(&mut b, &refs, &caps, upload);
    }
    let ct = if upload { "application/x-git-upload-pack-advertisement" } else { "application/x-git-receive-pack-advertisement" };
    git_response(ct, Body::from(b.freeze()))
}

fn advertise_v0(b: &mut BytesMut, refs: &RefList, caps: &str, upload: bool) {
    let mut lines: Vec<(ObjectId, String)> = Vec::new();
    if upload && let Some(oid) = refs.head_oid() {
        lines.push((oid, "HEAD".into()));
    }
    let mut sorted: Vec<_> = refs.refs.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for r in sorted {
        lines.push((r.oid, r.name.clone()));
        if upload && let Some(p) = r.peeled {
            lines.push((p, format!("{}^{{}}", r.name)));
        }
    }
    if lines.is_empty() {
        lines.push((ObjectId::ZERO, "capabilities^{}".into()));
    }
    for (i, (oid, name)) in lines.iter().enumerate() {
        let mut l = format!("{oid} {name}").into_bytes();
        if i == 0 {
            l.push(0);
            l.extend_from_slice(caps.as_bytes());
        }
        l.push(b'\n');
        pktline::write(b, &l);
    }
    pktline::flush(b);
}

enum BodyError {
    TooLarge(u64),
    BadGzip,
    /// The client went away or sent a broken body.
    Io,
    Memory(BudgetError),
}

impl BodyError {
    fn response(&self) -> Response {
        match self {
            BodyError::TooLarge(max) => (StatusCode::PAYLOAD_TOO_LARGE, format!("request larger than {max} bytes\n")).into_response(),
            BodyError::BadGzip => (StatusCode::BAD_REQUEST, "bad gzip body\n").into_response(),
            BodyError::Io => (StatusCode::BAD_REQUEST, "error reading the request body\n").into_response(),
            BodyError::Memory(e) => memory_response(e),
        }
    }
}

/// A request body, gunzipped if it says so, chunk by chunk as it arrives. At most `max` decoded
/// bytes are accepted.
struct BodyReader {
    stream: axum::body::BodyDataStream,
    gzip: Option<flate2::write::GzDecoder<Vec<u8>>>,
    /// Compressed bytes not yet inflated.
    compressed: Bytes,
    ended: bool,
    max: u64,
    total: u64,
    lease: Arc<Lease>,
}

impl BodyReader {
    fn new(headers: &HeaderMap, body: Body, max: u64, lease: Arc<Lease>) -> Self {
        let gzip = headers.get(header::CONTENT_ENCODING).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("gzip") || v.eq_ignore_ascii_case("x-gzip"));
        BodyReader {
            stream: body.into_data_stream(),
            gzip: gzip.then(|| flate2::write::GzDecoder::new(Vec::new())),
            compressed: Bytes::new(),
            ended: false,
            max,
            total: 0,
            lease,
        }
    }

    /// The next chunk, charged to the lease; `None` at the end.
    async fn next(&mut self) -> Result<Option<(Bytes, Charge)>, BodyError> {
        let Some(b) = self.next_raw().await? else { return Ok(None) };
        let charge = Charge::new(&self.lease, b.len() as u64).await.map_err(BodyError::Memory)?;
        Ok(Some((b, charge)))
    }

    /// Reads the rest of the body and drops it as it comes, holding nothing.
    async fn discard(&mut self) -> Result<(), BodyError> {
        while self.next_raw().await?.is_some() {}
        Ok(())
    }

    /// The whole body (bounded by `max`), charged.
    async fn read_all(&mut self) -> Result<(Bytes, Charge), BodyError> {
        let mut chunks = Vec::new();
        let mut charge = Charge::empty(&self.lease);
        while let Some((b, c)) = self.next().await? {
            chunks.push(b);
            charge.absorb(c);
        }
        if chunks.len() <= 1 {
            return Ok((chunks.pop().unwrap_or_default(), charge));
        }
        let len: usize = chunks.iter().map(Bytes::len).sum();
        let whole = Charge::new(&self.lease, len as u64).await.map_err(BodyError::Memory)?;
        let mut buf = BytesMut::with_capacity(len);
        for c in chunks {
            buf.extend_from_slice(&c);
        }
        Ok((buf.freeze(), whole))
    }

    async fn next_raw(&mut self) -> Result<Option<Bytes>, BodyError> {
        loop {
            if let Some(gz) = &mut self.gzip
                && !self.compressed.is_empty()
            {
                // 8 KiB of deflate inflates to at most about 8 MiB, which bounds a chunk.
                let n = self.compressed.len().min(8192);
                gz.write_all(&self.compressed[..n]).map_err(|_| BodyError::BadGzip)?;
                self.compressed.advance(n);
                let out = std::mem::take(gz.get_mut());
                if !out.is_empty() {
                    return self.count(out.into());
                }
                continue;
            }
            if self.ended {
                return Ok(None);
            }
            match self.stream.next().await {
                Some(Ok(b)) if self.gzip.is_some() => self.compressed = b,
                Some(Ok(b)) => return self.count(b),
                Some(Err(_)) => return Err(BodyError::Io),
                None => {
                    self.ended = true;
                    let Some(gz) = &mut self.gzip else { return Ok(None) };
                    gz.try_finish().map_err(|_| BodyError::BadGzip)?;
                    let out = std::mem::take(gz.get_mut());
                    if !out.is_empty() {
                        return self.count(out.into());
                    }
                    return Ok(None);
                }
            }
        }
    }

    fn count(&mut self, b: Bytes) -> Result<Option<Bytes>, BodyError> {
        self.total += b.len() as u64;
        if self.total > self.max {
            return Err(BodyError::TooLarge(self.max));
        }
        Ok(Some(b))
    }
}

async fn post_handler(State(s): State<Arc<Server>>, Path(path): Path<String>, headers: HeaderMap, body: Body) -> Response {
    let Some((repo, ep)) = split_path(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let creds = credentials(&headers);
    match ep {
        Endpoint::InfoRefs => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        Endpoint::UploadPack => {
            if let Err(e) = s.backend.authorize(&repo, creds.as_ref(), Access::Read).await {
                return error_response(e);
            }
            let lease = Arc::new(s.budget.lease());
            let (body, charge) = match BodyReader::new(&headers, body, s.config.max_request_bytes, lease.clone()).read_all().await {
                Ok(b) => b,
                Err(e) => return e.response(),
            };
            let result = if wants_v2(&headers) { upload_v2(&s, repo, &body, &lease).await } else { upload_v0(&s, repo, &body, &lease).await };
            drop(charge);
            match result {
                Ok(body) => git_response("application/x-git-upload-pack-result", body),
                Err(UploadError::Protocol(m)) => {
                    let mut b = BytesMut::new();
                    pktline::write(&mut b, format!("ERR {m}\n").as_bytes());
                    git_response("application/x-git-upload-pack-result", Body::from(b.freeze()))
                }
                Err(UploadError::Git(e)) => error_response(e),
                Err(UploadError::Memory(e)) => memory_response(&e),
            }
        }
        Endpoint::ReceivePack => {
            let pusher = match s.backend.authorize(&repo, creds.as_ref(), Access::Write).await {
                Ok(p) => p,
                Err(e) => return error_response(e),
            };
            receive_pack(&s, &repo, &headers, body, pusher).await
        }
    }
}

enum UploadError {
    /// Sent to the client as an `ERR` packet.
    Protocol(String),
    Git(GitError),
    Memory(BudgetError),
}

impl From<GitError> for UploadError {
    fn from(e: GitError) -> Self {
        match e {
            GitError::Invalid(m) => UploadError::Protocol(m),
            e => UploadError::Git(e),
        }
    }
}

impl From<pktline::PktError> for UploadError {
    fn from(e: pktline::PktError) -> Self {
        UploadError::Protocol(e.to_string())
    }
}

fn parse_oid(s: &[u8]) -> Result<ObjectId, UploadError> {
    ObjectId::from_hex(s).ok_or_else(|| UploadError::Protocol(format!("invalid object id {}", String::from_utf8_lossy(s))))
}

fn text(b: &[u8]) -> Result<&str, UploadError> {
    std::str::from_utf8(b).map_err(|_| UploadError::Protocol("invalid utf-8".into()))
}

#[derive(Default)]
struct FetchArgs {
    req: FetchRequest,
    done: bool,
    include_tag: bool,
    no_progress: bool,
    sideband: bool,
    deepen_requested: bool,
}

impl FetchArgs {
    /// One fetch argument line (shared by v0 and v2). Returns false if unknown.
    fn arg(&mut self, line: &str) -> Result<bool, UploadError> {
        let (k, v) = line.split_once(' ').unwrap_or((line, ""));
        match k {
            "want" => self.req.wants.push(parse_oid(v.split(' ').next().unwrap_or("").as_bytes())?),
            "have" => self.req.haves.push(parse_oid(v.as_bytes())?),
            "shallow" => self.req.client_shallow.push(parse_oid(v.as_bytes())?),
            "deepen" => {
                let d: u32 = v.parse().map_err(|_| UploadError::Protocol(format!("invalid deepen {v}")))?;
                self.req.depth = Some(d.max(1));
                self.deepen_requested = true;
            }
            "deepen-since" | "deepen-not" => return Err(UploadError::Protocol(format!("{k} is not supported; use --depth"))),
            "deepen-relative" => self.req.deepen_relative = true,
            "filter" => {
                self.req.filter = Some(Filter::parse(v).ok_or_else(|| UploadError::Protocol(format!("unsupported filter {v}")))?)
            }
            "done" => self.done = true,
            "include-tag" => self.include_tag = true,
            "no-progress" => self.no_progress = true,
            "thin-pack" | "ofs-delta" | "wait-for-done" => {}
            _ => return Ok(false),
        }
        Ok(true)
    }
}

async fn upload_v2(s: &Arc<Server>, repo: String, body: &[u8], lease: &Arc<Lease>) -> Result<Body, UploadError> {
    let mut r = Reader::new(body);
    let command = match r.next_pkt()? {
        Some(p) => text(p.text().ok_or_else(|| UploadError::Protocol("expected a command".into()))?)?.to_string(),
        None => return Ok(Body::empty()),
    };
    // capabilities until delim or flush, then arguments until flush
    let mut args = Vec::new();
    let mut in_args = false;
    loop {
        match r.next_pkt()? {
            None | Some(Pkt::Flush) => break,
            Some(Pkt::Delim) => in_args = true,
            Some(p @ Pkt::Data(_)) => {
                if in_args {
                    args.push(text(p.text().unwrap())?.to_string());
                }
            }
            Some(Pkt::ResponseEnd) => break,
        }
    }
    match command.as_str() {
        "command=ls-refs" => {
            let refs = s.backend.refs(&repo).await?;
            Ok(Body::from(ls_refs(&refs, &args)))
        }
        "command=fetch" => {
            let mut f = FetchArgs { sideband: true, ..Default::default() };
            for a in &args {
                if !f.arg(a)? {
                    return Err(UploadError::Protocol(format!("unexpected fetch argument {a}")));
                }
            }
            let mut out = BytesMut::new();
            if !f.done {
                let mut acks = Vec::new();
                for h in &f.req.haves {
                    if s.backend.has_object(&repo, h).await? {
                        acks.push(*h);
                    }
                }
                pktline::write_line(&mut out, "acknowledgments");
                if acks.is_empty() {
                    pktline::write_line(&mut out, "NAK");
                    pktline::flush(&mut out);
                    return Ok(Body::from(out.freeze()));
                }
                for a in &acks {
                    pktline::write_line(&mut out, &format!("ACK {a}"));
                }
                pktline::write_line(&mut out, "ready");
                pktline::delim(&mut out);
            }
            let (objects, planned) = plan(s, &repo, &mut f, &mut out, true, lease).await?;
            pktline::write_line(&mut out, "packfile");
            let opts = PackStream { sideband: true, progress: !f.no_progress, trailing_flush: true };
            Ok(stream_pack(s.clone(), repo, out, objects, planned, lease.clone(), opts))
        }
        other => Err(UploadError::Protocol(format!("unknown command {other}"))),
    }
}

fn ls_refs(refs: &RefList, args: &[String]) -> Bytes {
    let symrefs = args.iter().any(|a| a == "symrefs");
    let peel = args.iter().any(|a| a == "peel");
    let unborn = args.iter().any(|a| a == "unborn");
    let prefixes: Vec<&str> = args.iter().filter_map(|a| a.strip_prefix("ref-prefix ")).collect();
    let wanted = |name: &str| prefixes.is_empty() || prefixes.iter().any(|p| name.starts_with(p));
    let mut b = BytesMut::new();
    if wanted("HEAD")
        && let Some(target) = &refs.head
    {
        let sym = if symrefs { format!(" symref-target:{target}") } else { String::new() };
        match refs.get(target) {
            Some(r) => pktline::write_line(&mut b, &format!("{} HEAD{sym}", r.oid)),
            None if unborn => pktline::write_line(&mut b, &format!("unborn HEAD{sym}")),
            None => {}
        }
    }
    let mut sorted: Vec<_> = refs.refs.iter().filter(|r| wanted(&r.name)).collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for r in sorted {
        let mut line = format!("{} {}", r.oid, r.name);
        if peel && let Some(p) = r.peeled {
            line.push_str(&format!(" peeled:{p}"));
        }
        pktline::write_line(&mut b, &line);
    }
    pktline::flush(&mut b);
    b.freeze()
}

/// Memory charged per planned object: its id in the plan, and in the `include-tag` set.
const PLANNED_OBJECT_BYTES: u64 = 64;

/// Plans the pack, adds tags for `include-tag` and writes the shallow section. The plan is
/// charged to `lease`.
async fn plan(
    s: &Arc<Server>,
    repo: &str,
    f: &mut FetchArgs,
    out: &mut BytesMut,
    v2: bool,
    lease: &Arc<Lease>,
) -> Result<(Vec<ObjectId>, Charge), UploadError> {
    let plan = s.backend.plan_fetch(repo, &f.req).await?;
    let charge = Charge::new(lease, plan.objects.len() as u64 * PLANNED_OBJECT_BYTES).await.map_err(UploadError::Memory)?;
    let mut objects = plan.objects;
    if f.include_tag {
        let sent: HashSet<ObjectId> = objects.iter().copied().collect();
        let refs = s.backend.refs(repo).await?;
        for r in refs.refs {
            if r.name.starts_with("refs/tags/")
                && let Some(p) = r.peeled
                && sent.contains(&p)
                && !sent.contains(&r.oid)
            {
                objects.push(r.oid);
            }
        }
    }
    if f.deepen_requested || !f.req.client_shallow.is_empty() {
        if v2 {
            pktline::write_line(out, "shallow-info");
        }
        for id in &plan.shallow {
            pktline::write_line(out, &format!("shallow {id}"));
        }
        for id in &plan.unshallow {
            pktline::write_line(out, &format!("unshallow {id}"));
        }
        if v2 {
            pktline::delim(out);
        } else {
            pktline::flush(out);
        }
    }
    Ok((objects, charge))
}

async fn upload_v0(s: &Arc<Server>, repo: String, body: &[u8], lease: &Arc<Lease>) -> Result<Body, UploadError> {
    let mut r = Reader::new(body);
    let mut f = FetchArgs::default();
    let mut first_want = true;
    // Whether the request goes past the wants block (haves, a flush, or `done`). A request with
    // only wants (the first one of a shallow fetch) gets only the shallow list back.
    let mut negotiating = false;
    let mut flushes = 0;
    while let Some(p) = r.next_pkt()? {
        if flushes > 0 {
            negotiating = true;
        }
        let Some(line) = p.text() else {
            flushes += 1;
            continue;
        };
        let line = text(line)?;
        if first_want && let Some(rest) = line.strip_prefix("want ") {
            first_want = false;
            let caps: Vec<&str> = rest.split(' ').skip(1).collect();
            f.sideband = caps.contains(&"side-band-64k");
            f.no_progress = caps.contains(&"no-progress");
            f.include_tag = caps.contains(&"include-tag");
        }
        if !f.arg(line)? {
            return Err(UploadError::Protocol(format!("unexpected line {line}")));
        }
    }
    if f.req.wants.is_empty() {
        return Ok(Body::empty());
    }
    let mut out = BytesMut::new();
    if !f.done {
        // Without multi_ack the client keeps sending haves until it says `done`; the plan then
        // uses all of them.
        if f.deepen_requested || !f.req.client_shallow.is_empty() {
            plan(s, &repo, &mut f, &mut out, false, lease).await?;
        }
        if negotiating {
            pktline::write_line(&mut out, "NAK");
        }
        return Ok(Body::from(out.freeze()));
    }
    let (objects, planned) = plan(s, &repo, &mut f, &mut out, false, lease).await?;
    pktline::write_line(&mut out, "NAK");
    let sideband = f.sideband;
    let opts = PackStream { sideband, progress: sideband && !f.no_progress, trailing_flush: sideband };
    Ok(stream_pack(s.clone(), repo, out, objects, planned, lease.clone(), opts))
}

struct PackStream {
    /// Side-band-64k framing (otherwise the pack is sent raw).
    sideband: bool,
    progress: bool,
    /// A flush after the pack.
    trailing_flush: bool,
}

/// Streams `prefix`, then the pack (in side-band packets, or raw), then an optional flush.
///
/// Objects are read in batches sized from the previous batch's average object size (about
/// `BATCH_BYTES`), each charged as soon as it's read and released object by object as entries
/// are written. A batch too large for one request's share is dropped and read again one object
/// at a time.
fn stream_pack(
    s: Arc<Server>,
    repo: String,
    prefix: BytesMut,
    objects: Vec<ObjectId>,
    planned: Charge,
    lease: Arc<Lease>,
    opts: PackStream,
) -> Body {
    const FIRST_BATCH: usize = 16;
    const MAX_BATCH: usize = 256;
    const BATCH_BYTES: u64 = 8 << 20;
    const CHUNK: usize = 1 << 16;
    const FRAMES: usize = 8;
    let PackStream { sideband, progress, trailing_flush } = opts;
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(FRAMES);
    let frame = move |data: &[u8], band: u8| -> Bytes {
        if sideband {
            let mut b = BytesMut::with_capacity(data.len() + data.len() / pktline::MAX_SIDEBAND_DATA * 5 + 5);
            pktline::write_band(&mut b, band, data);
            b.freeze()
        } else {
            Bytes::copy_from_slice(data)
        }
    };
    // If the client goes away, the work stops wherever it is (waiting for memory or objects
    // included) and everything it holds is released.
    let gone = tx.clone();
    let work = async move {
        let _planned = planned;
        if tx.send(Ok(prefix.freeze())).await.is_err() {
            return;
        }
        let refused = |e: BudgetError| {
            tracing::info!(error = %e, repo, "fetch stopped: memory budget");
            fail(sideband, &e.to_string())
        };
        // Frames in the channel and being written, and the frame being filled.
        let _frames = match Charge::new(&lease, ((FRAMES + 2) * (CHUNK + 64)) as u64).await {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Ok(refused(e))).await;
                return;
            }
        };
        if progress && sideband {
            let msg = format!("roll: sending {} objects\n", objects.len());
            let _ = tx.send(Ok(frame(msg.as_bytes(), BAND_PROGRESS))).await;
        }
        let (mut w, header) = pack::Writer::new(objects.len() as u32);
        let mut buf = BytesMut::with_capacity(CHUNK);
        buf.extend_from_slice(&header);
        let mut next = 0;
        let mut batch_len = FIRST_BATCH;
        while next < objects.len() {
            let batch = &objects[next..(next + batch_len).min(objects.len())];
            let read = match s.backend.read_objects(&repo, batch).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, repo, "reading objects for a fetch");
                    let _ = tx.send(Ok(fail(sideband, "error reading objects"))).await;
                    return;
                }
            };
            let bytes: u64 = read.iter().flatten().map(|o| o.data.len() as u64).sum();
            let mut held = match Charge::new(&lease, bytes).await {
                Ok(c) => c,
                Err(BudgetError::TooLarge(_)) if batch.len() > 1 => {
                    drop(read);
                    batch_len = 1;
                    continue;
                }
                Err(e) => {
                    let _ = tx.send(Ok(refused(e))).await;
                    return;
                }
            };
            next += batch.len();
            batch_len = (BATCH_BYTES / (bytes / batch.len() as u64).max(1)).clamp(1, MAX_BATCH as u64) as usize;
            for (id, o) in batch.iter().zip(read) {
                let Some(o) = o else {
                    let _ = tx.send(Ok(fail(sideband, &format!("missing object {id}")))).await;
                    return;
                };
                let entry_charge = match Charge::new(&lease, pack::entry_bound(o.data.len()) as u64).await {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx.send(Ok(refused(e))).await;
                        return;
                    }
                };
                let entry = w.entry(o.kind, &o.data);
                held.release(o.data.len() as u64);
                drop(o);
                let mut e = &entry[..];
                while !e.is_empty() {
                    let take = (CHUNK - buf.len()).min(e.len());
                    buf.extend_from_slice(&e[..take]);
                    e = &e[take..];
                    if buf.len() == CHUNK {
                        if tx.send(Ok(frame(&buf, BAND_DATA))).await.is_err() {
                            return;
                        }
                        buf.clear();
                    }
                }
                drop(entry);
                drop(entry_charge);
            }
        }
        buf.extend_from_slice(&w.finish());
        let mut last = BytesMut::from(&frame(&buf, BAND_DATA)[..]);
        if trailing_flush {
            pktline::flush(&mut last);
        }
        let _ = tx.send(Ok(last.freeze())).await;
    };
    tokio::spawn(async move {
        tokio::select! {
            () = work => {}
            () = gone.closed() => {}
        }
    });
    Body::from_stream(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }))
}

fn fail(sideband: bool, msg: &str) -> Bytes {
    let mut b = BytesMut::new();
    if sideband {
        pktline::write_band(&mut b, BAND_ERROR, format!("{msg}\n").as_bytes());
    }
    b.freeze()
}

/// git's ref name rules, roughly (`git check-ref-format`).
pub fn valid_ref_name(name: &str) -> bool {
    name.starts_with("refs/")
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && !name.contains("//")
        && !name.split('/').any(|c| c.is_empty() || c.starts_with('.'))
        && name.bytes().all(|c| c > b' ' && c != 0x7f && !b"~^:?*[\\".contains(&c))
}

/// Capabilities on a command line (after the NUL), if it has them.
fn command_caps(d: &[u8]) -> Option<Vec<String>> {
    let d = d.strip_suffix(b"\n").unwrap_or(d);
    let i = d.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&d[i + 1..]).split(' ').map(str::to_string).collect())
}

/// Where a push's commands (and push options) end in `buf`, if all of them are there.
fn commands_end(buf: &[u8]) -> Result<Option<usize>, String> {
    let mut r = Reader::new(buf);
    let mut updates = false;
    let mut options = false;
    let mut in_options = false;
    loop {
        match r.next_pkt() {
            Ok(None) | Err(PktError::Truncated) => return Ok(None),
            Err(e) => return Err(e.to_string()),
            Ok(Some(Pkt::Flush)) => {
                if in_options || !(updates && options) {
                    return Ok(Some(buf.len() - r.rest().len()));
                }
                in_options = true;
            }
            Ok(Some(Pkt::Data(d))) if !in_options => {
                if let Some(caps) = command_caps(d) {
                    options = caps.iter().any(|c| c == "push-options");
                }
                updates |= !d.starts_with(b"shallow ");
            }
            Ok(Some(_)) if in_options => {}
            Ok(Some(_)) => return Err("unexpected packet".into()),
        }
    }
}

struct Commands {
    updates: Vec<RefUpdate>,
    caps: Vec<String>,
    options: Vec<String>,
}

fn parse_commands(buf: &[u8]) -> Result<Commands, GitError> {
    let mut r = Reader::new(buf);
    let mut updates = Vec::new();
    let mut caps: Vec<String> = Vec::new();
    let bad = |m: &str| GitError::Invalid(m.to_string());
    loop {
        match r.next_pkt().map_err(|e| bad(&e.to_string()))? {
            None | Some(Pkt::Flush) => break,
            Some(Pkt::Data(d)) => {
                if let Some(c) = command_caps(d) {
                    caps = c;
                }
                let d = d.strip_suffix(b"\n").unwrap_or(d);
                let line = &d[..d.iter().position(|&b| b == 0).unwrap_or(d.len())];
                if line.starts_with(b"shallow ") {
                    continue;
                }
                let line = std::str::from_utf8(line).map_err(|_| bad("invalid command"))?;
                let mut it = line.splitn(3, ' ');
                let (Some(old), Some(new), Some(name)) = (it.next(), it.next(), it.next()) else {
                    return Err(bad("invalid command"));
                };
                updates.push(RefUpdate {
                    name: name.to_string(),
                    old: old.parse().map_err(|_| bad("invalid object id"))?,
                    new: new.parse().map_err(|_| bad("invalid object id"))?,
                });
            }
            Some(_) => return Err(bad("unexpected packet")),
        }
    }
    let mut options = Vec::new();
    if !updates.is_empty() && caps.iter().any(|c| c == "push-options") {
        loop {
            match r.next_pkt().map_err(|e| bad(&e.to_string()))? {
                None | Some(Pkt::Flush) => break,
                Some(p) => options.push(String::from_utf8_lossy(p.text().unwrap_or_default()).into_owned()),
            }
        }
    }
    Ok(Commands { updates, caps, options })
}

/// Unpacks a pushed pack from `chunks` as they arrive (on a blocking thread). Returns the
/// objects and the meter that still accounts for them; no chunks means no pack.
fn unpack<F>(mut chunks: mpsc::Receiver<(Bytes, Charge)>, limits: Limits, meter: Box<dyn Meter>, lookup: F) -> anyhow::Result<(Vec<Object>, Box<dyn Meter>)>
where
    F: FnMut(ObjectId) -> anyhow::Result<Option<Object>>,
{
    let mut meter = Some(meter);
    let mut parser = None;
    while let Some((chunk, charge)) = chunks.blocking_recv() {
        if chunk.is_empty() {
            continue;
        }
        let p = parser.get_or_insert_with(|| pack::Parser::new(limits, meter.take().expect("meter")));
        p.feed(&chunk)?;
        drop(charge);
    }
    match parser {
        None => Ok((Vec::new(), meter.expect("meter"))),
        Some(p) => pack::resolve_with(p.finish()?, lookup),
    }
}

async fn receive_pack(s: &Arc<Server>, repo: &str, headers: &HeaderMap, body: Body, pusher: Option<String>) -> Response {
    let lease = Arc::new(s.budget.lease());
    // If this request is dropped (the client went away), unpacking stops waiting for memory.
    let _cancel = CancelOnDrop(lease.clone());
    let mut body = BodyReader::new(headers, body, s.config.limits.max_pack_bytes + (1 << 20), lease.clone());

    // The commands come first; the pack follows them. Without memory for them, up to
    // `UNCHARGED_COMMANDS` are still read, so git gets a report saying why (it doesn't show the
    // body of a failed POST); past that the push gets a 503.
    const UNCHARGED_COMMANDS: usize = 64 << 10;
    let mut head = BytesMut::new();
    let mut head_charge = Charge::empty(&lease);
    let mut refused: Option<BudgetError> = None;
    let end = loop {
        match commands_end(&head) {
            Ok(Some(end)) => break end,
            Ok(None) => {}
            Err(m) => return error_response(GitError::Invalid(m)),
        }
        if head.len() as u64 > s.config.max_request_bytes {
            return (StatusCode::PAYLOAD_TOO_LARGE, format!("ref updates larger than {} bytes\n", s.config.max_request_bytes)).into_response();
        }
        if let Some(e) = &refused
            && head.len() > UNCHARGED_COMMANDS
        {
            return memory_response(e);
        }
        let chunk = match body.next_raw().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break head.len(),
            Err(e) => return e.response(),
        };
        if refused.is_none() {
            match Charge::new(&lease, chunk.len() as u64).await {
                Ok(c) => head_charge.absorb(c),
                Err(e) => refused = Some(e),
            }
        }
        head.extend_from_slice(&chunk);
    };
    let Commands { mut updates, caps, options } = match parse_commands(&head[..end]) {
        Ok(c) => c,
        Err(e) => return error_response(e),
    };
    if updates.is_empty() {
        // e.g. git's probe request before a large chunked push; nothing may follow it.
        drop(head_charge);
        return match body.discard().await {
            Ok(()) => git_response("application/x-git-receive-pack-result", Body::empty()),
            Err(e) => e.response(),
        };
    }
    let has = |c: &str| caps.iter().any(|x| x == c);
    let sideband = has("side-band-64k");
    let atomic = has("atomic");
    let first = head.split_off(end).freeze();
    drop(head);

    // Unpack on a blocking thread as the pack arrives.
    let (tx, rx) = mpsc::channel::<(Bytes, Charge)>(4);
    let job = {
        let handle = tokio::runtime::Handle::current();
        let meter = Box::new(LeaseMeter::new(lease.clone(), handle.clone()));
        let backend = s.backend.clone();
        let repo = repo.to_string();
        let limits = s.config.limits;
        tokio::task::spawn_blocking(move || {
            unpack(rx, limits, meter, |id| handle.block_on(backend.read_object(&repo, &id)).map_err(anyhow::Error::from))
        })
    };
    let mut next = refused.is_none().then_some((first, head_charge));
    while refused.is_none() {
        let item = match next.take() {
            Some(item) => item,
            None => match body.next().await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(BodyError::Memory(e)) => {
                    refused = Some(e);
                    break;
                }
                // The job ends when `tx` is dropped with this function.
                Err(e) => return e.response(),
            },
        };
        if tx.send(item).await.is_err() {
            break; // unpacking failed
        }
    }
    drop(tx);
    // Read and drop whatever unpacking didn't take, so git reads the report rather than a
    // connection closed under it.
    if let Err(e) = body.discard().await {
        return e.response();
    }
    let unpacked = match job.await {
        Ok(r) => r,
        Err(e) => Err(anyhow::anyhow!("unpacking failed: {e}")),
    };
    // The meter still accounts for the objects until the backend is done with them.
    let (unpack, objects, meter) = match (refused, unpacked) {
        (Some(e), _) => (Err(e.to_string()), Vec::new(), None),
        (None, Ok((objects, meter))) => (Ok(()), objects, Some(meter)),
        (None, Err(e)) => (Err(e.to_string()), Vec::new(), None),
    };

    let mut outcome = ReceiveOutcome::default();
    let mut invalid = Vec::new();
    updates.retain(|u| {
        let ok = valid_ref_name(&u.name) && !(u.old.is_zero() && u.new.is_zero());
        if !ok {
            invalid.push(RefStatus { name: u.name.clone(), result: Err("invalid ref name".into()) });
        }
        ok
    });

    let statuses: Vec<RefStatus> = if let Err(e) = &unpack {
        tracing::info!(repo, error = %e, "push rejected: unpack failed");
        updates.iter().map(|u| RefStatus { name: u.name.clone(), result: Err("unpacker error".into()) }).collect()
    } else if atomic && !invalid.is_empty() {
        updates.iter().map(|u| RefStatus { name: u.name.clone(), result: Err("atomic push failed".into()) }).collect()
    } else if updates.is_empty() {
        Vec::new()
    } else {
        let names: Vec<String> = updates.iter().map(|u| u.name.clone()).collect();
        match s.backend.receive(repo, ReceiveRequest { updates, objects, atomic, options, pusher }).await {
            Ok(o) => {
                outcome.messages = o.messages;
                let mut by_name: HashMap<String, Result<(), String>> = o.refs.into_iter().map(|r| (r.name, r.result)).collect();
                names
                    .into_iter()
                    .map(|n| {
                        let result = by_name.remove(&n).unwrap_or_else(|| Err("no status from server".into()));
                        RefStatus { name: n, result }
                    })
                    .collect()
            }
            Err(GitError::Internal(e)) => {
                tracing::error!(repo, error = %e, "push failed");
                names.into_iter().map(|n| RefStatus { name: n, result: Err("internal error".into()) }).collect()
            }
            Err(e) => names.into_iter().map(|n| RefStatus { name: n, result: Err(e.to_string()) }).collect(),
        }
    };
    // The objects are the backend's now (and dropped); stop accounting for them.
    drop(meter);
    outcome.refs = invalid.into_iter().chain(statuses).collect();

    let mut report = BytesMut::new();
    match &unpack {
        Ok(()) => pktline::write_line(&mut report, "unpack ok"),
        Err(e) => pktline::write_line(&mut report, &format!("unpack {}", one_line(e))),
    }
    for st in &outcome.refs {
        match &st.result {
            Ok(()) => pktline::write_line(&mut report, &format!("ok {}", st.name)),
            Err(reason) => pktline::write_line(&mut report, &format!("ng {} {}", st.name, one_line(reason))),
        }
    }
    pktline::flush(&mut report);
    if !sideband {
        return git_response("application/x-git-receive-pack-result", Body::from(report.freeze()));
    }
    let mut out = BytesMut::new();
    for m in &outcome.messages {
        for line in m.lines() {
            pktline::write_band(&mut out, BAND_PROGRESS, format!("{line}\n").as_bytes());
        }
    }
    pktline::write_band(&mut out, BAND_DATA, &report);
    pktline::flush(&mut out);
    git_response("application/x-git-receive-pack-result", Body::from(out.freeze()))
}

fn one_line(s: &str) -> String {
    let s: String = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ");
    s.chars().take(1000).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert!(matches!(split_path("org/repo.git/info/refs"), Some((r, Endpoint::InfoRefs)) if r == "org/repo"));
        assert!(matches!(split_path("repo/git-upload-pack"), Some((r, Endpoint::UploadPack)) if r == "repo"));
        assert!(matches!(split_path("a.git/git-receive-pack"), Some((r, Endpoint::ReceivePack)) if r == "a"));
        assert!(split_path("../x.git/info/refs").is_none());
        assert!(split_path(".git/info/refs").is_none());
        assert!(split_path("x.git/objects/info/packs").is_none());
    }

    #[test]
    fn ref_names() {
        assert!(valid_ref_name("refs/heads/main"));
        assert!(valid_ref_name("refs/heads/feature/x-1"));
        assert!(!valid_ref_name("HEAD"));
        assert!(!valid_ref_name("refs/heads/a..b"));
        assert!(!valid_ref_name("refs/heads/x.lock"));
        assert!(!valid_ref_name("refs/heads/a b"));
        assert!(!valid_ref_name("refs/heads/.hidden"));
        assert!(!valid_ref_name("refs/heads/"));
    }

    #[test]
    fn commands_end_waits_for_the_whole_section() {
        let zero = ObjectId::ZERO;
        let line = |caps: &str| {
            let l = format!("{zero} {} refs/heads/main\0{caps}\n", "1".repeat(40));
            format!("{:04x}{l}", l.len() + 4)
        };
        let plain = line("report-status") + "0000PACK";
        assert_eq!(commands_end(plain.as_bytes()), Ok(Some(plain.len() - 4)));
        assert_eq!(commands_end(&plain.as_bytes()[..20]), Ok(None));
        // With push options, a second section follows.
        let opts = line("report-status push-options") + "0000";
        assert_eq!(commands_end(opts.as_bytes()), Ok(None));
        let opts = opts + "000bci.skip0000PACK";
        assert_eq!(commands_end(opts.as_bytes()), Ok(Some(opts.len() - 4)));
        // A probe has no commands and no options.
        assert_eq!(commands_end(b"0000"), Ok(Some(4)));
        assert!(commands_end(b"zzzz").is_err());
    }
}
