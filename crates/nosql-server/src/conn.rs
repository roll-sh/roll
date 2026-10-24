//! One CQL connection: the native-protocol state machine of each reference, request
//! multiplexing, events and authentication.
//!
//! Stages follow `org.apache.cassandra.transport.ServerConnection`:
//!
//! - **Initial**: the decoder lets only OPTIONS and STARTUP through. STARTUP pins the version,
//!   is validated as the dialect does, negotiates compression (v3/v4 bodies; v5 segments start
//!   after its answer, whatever the answer is) and the ScyllaDB extensions, and is answered with
//!   READY (the database is known and allows everyone) or AUTHENTICATE.
//! - **Authenticating**: only AUTH_RESPONSE (`Unexpected message X, expecting SASL_RESPONSE`;
//!   ScyllaDB prints the opcode's number).
//!   The SASL PLAIN user routes the connection (`role@db`) unless SNI did. The engine's
//!   `PasswordAuthenticator` checks the credentials, including the platform's (the database's
//!   `DbConfig::superuser` role); only a platform credential the engine doesn't know (a
//!   [`Single`](crate::Single) whose spec adds one) is checked here.
//! - **Ready**: QUERY, PREPARE, EXECUTE and BATCH run concurrently, up to
//!   [`ServerSettings::max_in_flight`] per connection; responses go out as they finish, tagged
//!   with their stream id. When that many are executing, or the client doesn't read its
//!   responses, the connection stops reading (backpressure).
//!
//! The reader decodes frames and drives the stages; a writer task owns the encoder and
//! receives frames and codec switches (compression, segments) in order on one bounded queue.
//! Each request runs on its own engine [`eng::Session`] carrying the connection's state (keyspace,
//! role), so requests never wait for one another; `USE` updates the connection's keyspace.
//! Creating one per request costs an `Arc` clone and a copy of that state (well under a
//! microsecond: `nosql_engine`'s `per_request_sessions_are_cheap`). Frames with the TRACING flag
//! run traced (`Session::set_tracing`) and their RESULT carries the trace's id.
//!
//! Bound connections are registered with the database (`Database::register_client`) for
//! `system.clients` / `system_views.clients`: driver, STARTUP options, role, keyspace, request
//! count, TLS and shard.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use nosql_engine as eng;
use nosql_proto::event::EventType;
use nosql_proto::request::startup_key;
use nosql_proto::supported::{self, ScyllaNode, Supported};
use nosql_proto::wire::Reader;
use nosql_proto::{
    Compression, Ctx, Dialect, ErrorBody, Features, Flags, Frame, Inbound, Limits, Opcode, Rejection, Request, RequestFrame, Response,
    ResponseFrame, ServerDecoder, ServerEncoder, Startup, Version, code,
};
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf, ReadHalf, WriteHalf};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::convert::{self, MetadataRule};
use crate::directory::{Directory, Lease, Metrics, Presented, ServerSettings};
use crate::node::DbSpec;

/// The authenticator AUTHENTICATE names (both references report Cassandra's class).
pub const PASSWORD_AUTHENTICATOR: &str = "org.apache.cassandra.auth.PasswordAuthenticator";

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

type BoxIo = Counted<Box<dyn Io>>;
type FrameReader = FramedRead<ReadHalf<BoxIo>, ServerDecoder>;
type FrameWriter = FramedWrite<WriteHalf<BoxIo>, ServerEncoder>;

/// How a connection arrived.
#[derive(Clone, Debug)]
pub struct Accepted {
    /// The client (from the PROXY header when there is one).
    pub peer: SocketAddr,
    /// TLS server name.
    pub sni: Option<String>,
    /// Accepted on the shard-aware port: the shard is the client port modulo the shard count.
    pub shard_aware: bool,
    /// The negotiated TLS protocol and cipher suite ([`crate::net::tls_details`]).
    pub tls: Option<eng::system::clients::Tls>,
    /// The client certificate the TLS handshake verified ([`crate::net::client_certificate`]).
    pub certificate: Option<Arc<eng::auth::certificate::ClientCertificate>>,
}

/// Serves one connection until it closes.
pub async fn serve(io: Box<dyn Io>, accepted: Accepted, dir: Arc<dyn Directory>) {
    let settings = dir.settings().clone();
    let target = dir.route_sni(accepted.sni.as_deref()).await;
    let presented = target.as_deref().map(Presented::of).unwrap_or_else(|| Presented::unrouted(settings.dialect));
    let counters = Arc::new(IoCounters::default());
    let (r, w) = tokio::io::split(Counted { inner: io, counters: counters.clone() });
    let limits = Limits::of(presented.dialect);
    let reader = FramedRead::new(r, ServerDecoder::with_limits(presented.dialect, limits));
    let writer = FramedWrite::new(w, ServerEncoder::with_limits(limits));
    let (tx, rx) = mpsc::channel(settings.write_queue.max(1));
    let mut writer = tokio::spawn(write_loop(writer, rx));
    let mut c = Connection {
        shard: shard_for(&accepted, presented.shards),
        dir,
        accepted,
        target,
        presented,
        tx,
        counters,
        reader,
        stage: Stage::Initial,
        ctx: Ctx::new(presented.dialect.current()).dialect(presented.dialect),
        lwt_mark: false,
        segments: false,
        client: eng::ClientInfo::default(),
        shared: None,
        kicked: None,
        transport: None,
        events: None,
        registered: Arc::new(Mutex::new(Vec::new())),
        permits: Arc::new(Semaphore::new(settings.max_in_flight.max(1))),
        pending_auth: None,
        settings,
    };
    let kicked = c.run().await;
    drop(c);
    if kicked {
        writer.abort();
        return;
    }
    // Let in-flight requests answer and the queue drain (bounded: a client that stopped
    // reading doesn't hold the task forever).
    if tokio::time::timeout(std::time::Duration::from_secs(30), &mut writer).await.is_err() {
        writer.abort();
    }
}

static ROUND_ROBIN: AtomicU32 = AtomicU32::new(0);

/// The shard a connection is on: chosen by source port on the shard-aware port, round robin
/// otherwise.
pub fn shard_for(a: &Accepted, shards: u32) -> u32 {
    let shards = shards.max(1);
    if a.shard_aware { a.peer.port() as u32 % shards } else { ROUND_ROBIN.fetch_add(1, Ordering::Relaxed) % shards }
}

/// What the reader hands the writer, in order.
enum Out {
    Frame(Frame),
    Reject(Rejection),
    Compression(Option<Compression>),
    Segments(bool),
    Close,
}

async fn write_loop(mut w: FrameWriter, mut rx: mpsc::Receiver<Out>) {
    // Returns false to stop.
    async fn apply(w: &mut FrameWriter, o: Out) -> bool {
        match o {
            Out::Frame(f) => w.feed(f).await.is_ok(),
            Out::Reject(r) => w.feed(r).await.is_ok(),
            Out::Compression(c) => {
                w.encoder_mut().set_compression(c);
                true
            }
            Out::Segments(lz4) => {
                w.encoder_mut().start_segments(lz4);
                true
            }
            Out::Close => false,
        }
    }
    const BATCH_BYTES: usize = 1 << 20;
    'outer: while let Some(o) = rx.recv().await {
        if !apply(&mut w, o).await {
            break;
        }
        // Batch what is queued into one write. Then let the tasks that are ready run once (the
        // requests finishing in this scheduler pass) and take their answers too: under load
        // that is one `send` for many responses instead of one each (cassandra-stress on 2 CPUs:
        // writes +28%, reads +14%; docs/benchmarks/nosql.md). Alone on the runtime the yield
        // returns at once.
        let mut yielded = false;
        loop {
            while w.write_buffer().len() < BATCH_BYTES {
                match rx.try_recv() {
                    Ok(o) => {
                        if !apply(&mut w, o).await {
                            break 'outer;
                        }
                    }
                    Err(_) => break,
                }
            }
            if yielded || w.write_buffer().len() >= BATCH_BYTES {
                break;
            }
            yielded = true;
            tokio::task::yield_now().await;
        }
        if SinkExt::<Frame>::flush(&mut w).await.is_err() {
            return;
        }
    }
    let _ = SinkExt::<Frame>::flush(&mut w).await;
    let _ = w.get_mut().shutdown().await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Initial,
    Authenticating,
    Ready,
}

/// What request tasks share once the connection is bound to a database.
struct Shared {
    lease: Lease,
    state: Mutex<eng::SessionState>,
    /// The connection's row in the clients tables (removed when dropped).
    client: eng::system::clients::ClientHandle,
    ctx: Ctx,
    /// Error texts of decoding and protocol checks: the connection's dialect.
    dialect: Dialect,
    lwt_mark: bool,
    tx: mpsc::Sender<Out>,
    _guard: ConnGuard,
}

/// Counts a bound connection in its database's metrics.
struct ConnGuard {
    metrics: Arc<Metrics>,
    since: Instant,
}

impl ConnGuard {
    fn new(metrics: Arc<Metrics>) -> Self {
        Metrics::add(&metrics.connections, 1);
        Metrics::add(&metrics.connections_total, 1);
        ConnGuard { metrics, since: Instant::now() }
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.metrics.connections.fetch_sub(1, Ordering::Relaxed);
        Metrics::add(&self.metrics.connection_ms, self.since.elapsed().as_millis() as u64);
    }
}

struct Connection {
    dir: Arc<dyn Directory>,
    settings: ServerSettings,
    accepted: Accepted,
    /// The database SNI (or the directory's default) named, if any.
    target: Option<Arc<DbSpec>>,
    presented: Presented,
    shard: u32,
    tx: mpsc::Sender<Out>,
    counters: Arc<IoCounters>,
    reader: FrameReader,
    stage: Stage,
    /// Version (once STARTUP pinned it), dialect and negotiated extensions.
    ctx: Ctx,
    lwt_mark: bool,
    segments: bool,
    client: eng::ClientInfo,
    shared: Option<Arc<Shared>>,
    kicked: Option<watch::Receiver<bool>>,
    /// The database's native transport switch (REST `storage_service/native_transport`).
    transport: Option<watch::Receiver<bool>>,
    events: Option<tokio::task::JoinHandle<()>>,
    registered: Arc<Mutex<Vec<EventType>>>,
    permits: Arc<Semaphore>,
    /// A SASL negotiation in progress (after an AUTH_CHALLENGE).
    pending_auth: Option<(Lease, eng::Session, Arc<DbSpec>)>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(e) = self.events.take() {
            e.abort();
        }
    }
}

/// Resolves when the database's native transport is turned off.
async fn wait_transport_off(t: &mut Option<watch::Receiver<bool>>) {
    if let Some(rx) = t
        && rx.wait_for(|on| !*on).await.is_ok()
    {
        return;
    }
    std::future::pending().await
}

async fn wait_kicked(k: &mut Option<watch::Receiver<bool>>) {
    match k {
        Some(rx) => {
            let _ = rx.wait_for(|k| *k).await;
        }
        None => std::future::pending().await,
    }
}

impl Connection {
    fn dialect(&self) -> Dialect {
        self.presented.dialect
    }

    /// Reads and handles frames until the connection ends. Returns whether the database
    /// disconnected it.
    async fn run(&mut self) -> bool {
        let deadline = tokio::time::Instant::now() + self.settings.handshake_timeout;
        loop {
            let item = tokio::select! {
                biased;
                _ = wait_kicked(&mut self.kicked) => return true,
                _ = wait_transport_off(&mut self.transport) => return true,
                _ = tokio::time::sleep_until(deadline), if self.stage != Stage::Ready => return false,
                _ = self.tx.closed() => return false,
                item = self.reader.next() => item,
            };
            let item = match item {
                Some(Ok(item)) => item,
                _ => return false,
            };
            match item {
                Inbound::Reject(r) => {
                    let close = r.close;
                    self.count_request(true);
                    crate::metrics::count_error(r.error.code);
                    if !self.send(Out::Reject(r)).await || close {
                        let _ = self.tx.try_send(Out::Close);
                        return false;
                    }
                }
                Inbound::Frame(f) => {
                    if !self.frame(f).await {
                        return self.kicked.as_ref().is_some_and(|k| *k.borrow());
                    }
                }
            }
        }
    }

    fn count_request(&self, error: bool) {
        if let Some(sh) = &self.shared {
            sh.client.request();
            Metrics::add(&sh.lease.metrics.requests, 1);
            if error {
                Metrics::add(&sh.lease.metrics.errors, 1);
            }
        }
    }

    /// Queues output; false when the connection is gone or being kicked.
    async fn send(&mut self, out: Out) -> bool {
        let tx = self.tx.clone();
        tokio::select! {
            r = tx.send(out) => r.is_ok(),
            _ = wait_kicked(&mut self.kicked) => false,
        }
    }

    async fn reply(&mut self, stream: i16, response: Response) -> bool {
        let error = matches!(response, Response::Error(_));
        if let Response::Error(e) = &response {
            crate::metrics::count_error(e.code);
        }
        self.count_request(error);
        let f = ResponseFrame::new(stream, response).to_frame(self.ctx);
        self.send(Out::Frame(f)).await
    }

    async fn error(&mut self, stream: i16, e: ErrorBody) -> bool {
        self.reply(stream, Response::Error(e)).await
    }

    /// One frame. Returns false to close the connection.
    async fn frame(&mut self, f: Frame) -> bool {
        let stream = f.stream;
        if self.stage == Stage::Initial {
            // OPTIONS is answered in the version it came in.
            self.ctx.version = f.version;
        }
        if self.dialect() == Dialect::Scylla && scylla_flags_overflow(&f, &self.ctx) {
            return self.error(stream, ErrorBody::server("Bit mask contains invalid enumeration indices.")).await;
        }
        let rf = match RequestFrame::decode(self.ctx, &f) {
            Ok(rf) => rf,
            Err(e) => return self.error(stream, e.to_error(self.dialect())).await,
        };
        match (self.stage, rf.request) {
            (_, Request::Options) if self.stage != Stage::Authenticating => {
                let s = self.supported();
                self.reply(stream, Response::Supported(s)).await
            }
            (Stage::Initial, Request::Startup(s)) => self.startup(stream, f.version, s).await,
            (Stage::Initial, _) => {
                // The decoder only lets OPTIONS and STARTUP through before STARTUP.
                let msg = format!("Unexpected message {}, expecting STARTUP or OPTIONS", f.opcode.name());
                self.error(stream, ErrorBody::protocol(msg)).await
            }
            (Stage::Authenticating, Request::AuthResponse(token)) => {
                let r = self.auth_response(token).await;
                self.reply(stream, r).await
            }
            (Stage::Authenticating, _) => {
                let msg = match self.dialect() {
                    Dialect::Cassandra => format!("Unexpected message {}, expecting SASL_RESPONSE", f.opcode.name()),
                    Dialect::Scylla => format!("Unexpected message {}, expecting SASL_RESPONSE", f.opcode as u8),
                };
                self.error(stream, ErrorBody::protocol(msg)).await
            }
            (Stage::Ready, Request::Startup(_)) => {
                self.error(stream, ErrorBody::protocol("Unexpected message STARTUP, the connection is already initialized")).await
            }
            (Stage::Ready, Request::Register(names)) => match EventType::parse_all(&names, self.dialect()) {
                Err(e) => self.error(stream, e.to_error(self.dialect())).await,
                Ok(types) => {
                    {
                        let mut r = self.registered.lock();
                        for t in types {
                            if !r.contains(&t) {
                                r.push(t);
                            }
                        }
                    }
                    self.ensure_events();
                    self.reply(stream, Response::Ready).await
                }
            },
            (Stage::Ready, Request::AuthResponse(token)) => {
                let r = self.reauthenticate(token).await;
                self.reply(stream, r).await
            }
            (Stage::Ready, request) => {
                let Some(sh) = self.shared.clone() else { return false };
                let Some(permit) = self.acquire().await else { return false };
                let rf = RequestFrame { request, ..rf };
                tokio::spawn(execute(sh, rf, permit));
                true
            }
        }
    }

    async fn acquire(&mut self) -> Option<OwnedSemaphorePermit> {
        let permits = self.permits.clone();
        let tx = self.tx.clone();
        tokio::select! {
            p = permits.acquire_owned() => p.ok(),
            _ = wait_kicked(&mut self.kicked) => None,
            _ = tx.closed() => None,
        }
    }

    fn supported(&self) -> Supported {
        match self.dialect() {
            Dialect::Cassandra => supported::cassandra(),
            Dialect::Scylla => supported::scylla(&ScyllaNode {
                host_id: nosql_types::timeuuid::format_uuid(&self.presented.host_id),
                shard: self.shard,
                nr_shards: self.presented.shards,
                ignore_msb: 12,
                shard_aware_port: self.settings.shard_aware_port,
                shard_aware_port_ssl: self.settings.shard_aware_port_ssl,
                tablets: true,
                tablets_v2: false,
            }),
        }
    }

    async fn startup(&mut self, stream: i16, version: Version, s: Startup) -> bool {
        // Cassandra pins the version before it validates STARTUP.
        self.reader.decoder_mut().established(version);
        self.ctx = Ctx::new(version).dialect(self.dialect());
        let v5 = version.uses_segments();
        let mut lz4 = false;
        let answer = match s.validate(self.dialect(), version) {
            Err(e) => Response::Error(e),
            Ok(compression) => {
                if self.dialect() == Dialect::Scylla {
                    self.ctx.features = Features {
                        metadata_id: s.get(startup_key::SCYLLA_USE_METADATA_ID).is_some(),
                        rate_limit_code: s.get(startup_key::SCYLLA_RATE_LIMIT_ERROR).map(|_| code::SCYLLA_RATE_LIMIT),
                        ..Default::default()
                    };
                    self.lwt_mark = s.get(startup_key::SCYLLA_LWT_ADD_METADATA_MARK).is_some();
                }
                if v5 {
                    lz4 = compression == Some(Compression::Lz4);
                } else {
                    // READY goes out compressed already.
                    if !self.send(Out::Compression(compression)).await {
                        return false;
                    }
                    self.reader.decoder_mut().set_compression(compression);
                }
                // `..Default::default()`: the engine adds fields (the STARTUP options).
                #[allow(clippy::needless_update)]
                let client = eng::ClientInfo {
                    address: Some(self.accepted.peer),
                    driver_name: s.get(startup_key::DRIVER_NAME).map(str::to_string),
                    driver_version: s.get(startup_key::DRIVER_VERSION).map(str::to_string),
                    protocol_version: version.as_u8(),
                    options: s.options.clone(),
                    certificate: self.accepted.certificate.clone(),
                    ..Default::default()
                };
                self.client = client;
                let has_cert = self.accepted.certificate.is_some();
                match self.target.clone() {
                    Some(t) if !t.requires_auth() => match self.dir.open(&t).await {
                        Ok(lease) => {
                            self.bind(lease, eng::SessionState { client: self.client.clone(), ..Default::default() });
                            Response::Ready
                        }
                        Err(e) => Response::Error(ErrorBody::server(e.to_string())),
                    },
                    // ScyllaDB's certificate authenticators: no SASL for a certificate.
                    Some(t) if authenticator(&t).ready_at_startup(has_cert) => self.certificate_login(t).await,
                    target => {
                        self.stage = Stage::Authenticating;
                        let class = target.as_deref().map_or(PASSWORD_AUTHENTICATOR, |t| authenticator(t).authenticate_class(has_cert));
                        Response::Authenticate(class.into())
                    }
                }
            }
        };
        let ok = self.reply(stream, answer).await;
        if v5 && !self.segments {
            self.segments = true;
            if !self.send(Out::Segments(lz4)).await {
                return false;
            }
            self.reader.decoder_mut().start_segments(lz4);
        }
        ok
    }

    fn bind(&mut self, lease: Lease, state: eng::SessionState) {
        self.counters.bind(lease.metrics.clone());
        let p = Presented::of(&lease.spec);
        if p.host_id != self.presented.host_id || p.shards != self.presented.shards {
            self.presented.host_id = p.host_id;
            if p.shards != self.presented.shards {
                self.presented.shards = p.shards;
                self.shard = shard_for(&self.accepted, p.shards);
            }
        }
        self.kicked = Some(lease.kicked());
        self.transport = Some(lease.db().native_transport_watch());
        let guard = ConnGuard::new(lease.metrics.clone());
        let client = lease.db().register_client(self.client.clone());
        if lease.spec.requires_auth() {
            client.authenticated(state.user.as_deref());
        } else {
            client.ready();
        }
        client.set_shard(self.shard);
        client.set_tls(self.accepted.tls.clone());
        self.shared = Some(Arc::new(Shared {
            lease,
            state: Mutex::new(state),
            client,
            ctx: self.ctx,
            dialect: self.dialect(),
            lwt_mark: self.lwt_mark,
            tx: self.tx.clone(),
            _guard: guard,
        }));
        self.stage = Stage::Ready;
    }

    /// The token of an AUTH_SUCCESS the server answers itself: ScyllaDB sends empty bytes,
    /// Cassandra null.
    fn auth_ok(&self) -> Response {
        Response::AuthSuccess(match self.dialect() {
            Dialect::Scylla => Some(Bytes::new()),
            Dialect::Cassandra => None,
        })
    }

    /// STARTUP of a connection whose certificate authenticates it (ScyllaDB): READY, or the
    /// authenticator's error (the connection may STARTUP again).
    async fn certificate_login(&mut self, spec: Arc<DbSpec>) -> Response {
        let lease = match self.dir.open(&spec).await {
            Ok(l) => l,
            Err(e) => return Response::Error(ErrorBody::server(e.to_string())),
        };
        let mut session = lease.db().connect(self.client.clone());
        match session.authenticate(&[]).await {
            Ok(_) => {
                self.bind(lease, session.state);
                Response::Ready
            }
            Err(e) => Response::Error(convert::error_body(&e, &self.ctx.features)),
        }
    }

    async fn auth_response(&mut self, token: Option<Bytes>) -> Response {
        if let Some((lease, session, spec)) = self.pending_auth.take() {
            return self.engine_auth(lease, session, spec, token.unwrap_or_default()).await;
        }
        // A certificate authenticator ignores the token (Cassandra still fails to decode a null
        // one, as for every authenticator).
        if let Some(t) = self.target.clone()
            && t.requires_auth()
            && authenticator(&t).by_certificate(self.accepted.certificate.is_some())
        {
            if token.is_none() && self.dialect() == Dialect::Cassandra {
                return Response::Error(sasl_error(self.dialect(), None, ""));
            }
            let lease = match self.dir.open(&t).await {
                Ok(l) => l,
                Err(e) => return Response::Error(ErrorBody::server(e.to_string())),
            };
            let session = lease.db().connect(self.client.clone());
            return self.engine_auth(lease, session, t, token.unwrap_or_default()).await;
        }
        let plain = match token.as_deref().map(Plain::parse) {
            Some(Ok(p)) => p,
            Some(Err(e)) => return Response::Error(sasl_error(self.dialect(), token.as_deref(), e)),
            None => return Response::Error(sasl_error(self.dialect(), None, "")),
        };
        let token = token.unwrap_or_default();
        let (spec, role) = match &self.target {
            // `role@<this db>` names the database the connection is already on.
            Some(t) => (t.clone(), plain.user.strip_suffix(&format!("@{}", t.id)).unwrap_or(&plain.user).to_string()),
            None => match self.dir.route_user(&plain.user).await {
                Some(x) => x,
                None => return Response::Error(bad_credentials(self.dialect(), &plain.user)),
            },
        };
        let lease = match self.dir.open(&spec).await {
            Ok(l) => l,
            Err(e) => return Response::Error(ErrorBody::server(e.to_string())),
        };
        let mut session = lease.db().connect(self.client.clone());
        if platform_only(&spec, lease.db(), &role, &plain.password) {
            session.state.user = Some(role);
            self.bind(lease, session.state);
            return self.auth_ok();
        }
        if !spec.requires_auth() {
            // AllowAllAuthenticator: anyone, as the anonymous user.
            self.bind(lease, session.state);
            return self.auth_ok();
        }
        let token = plain.with_user(&token, &role);
        self.engine_auth(lease, session, spec, token).await
    }

    async fn engine_auth(&mut self, lease: Lease, session: eng::Session, spec: Arc<DbSpec>, token: Bytes) -> Response {
        let (session, answer) = call(session, Call::Authenticate(token)).await;
        let (Some(session), Answer::Auth(r)) = (session, answer) else {
            return Response::Error(ErrorBody::server("the engine is unavailable"));
        };
        match r {
            Ok(eng::AuthStep::Challenge(c)) => {
                self.pending_auth = Some((lease, session, spec));
                Response::AuthChallenge(Some(c))
            }
            Ok(eng::AuthStep::Success(t)) => {
                self.bind(lease, session.state);
                Response::AuthSuccess(t)
            }
            Err(e) => Response::Error(convert::error_body(&e, &self.ctx.features)),
        }
    }

    /// AUTH_RESPONSE on a ready connection: a new negotiation that logs the role in again.
    /// With AllowAllAuthenticator ScyllaDB refuses it and Cassandra always succeeds.
    async fn reauthenticate(&mut self, token: Option<Bytes>) -> Response {
        let Some(sh) = self.shared.clone() else { return Response::Error(ErrorBody::server("Should not reach")) };
        let spec = sh.lease.spec.clone();
        if !spec.requires_auth() {
            return match self.dialect() {
                Dialect::Scylla => Response::Error(ErrorBody::server("Should not reach")),
                Dialect::Cassandra => Response::AuthSuccess(None),
            };
        }
        let by_cert = authenticator(&spec).by_certificate(self.accepted.certificate.is_some());
        if by_cert && !(token.is_none() && self.dialect() == Dialect::Cassandra) {
            let (session, answer) = call(sh.session(), Call::Authenticate(token.unwrap_or_default())).await;
            return match (session, answer) {
                (Some(session), Answer::Auth(Ok(eng::AuthStep::Success(t)))) => {
                    sh.client.authenticated(session.state.user.as_deref());
                    sh.state.lock().user = session.state.user.clone();
                    Response::AuthSuccess(t)
                }
                (_, Answer::Auth(Err(e))) => Response::Error(convert::error_body(&e, &self.ctx.features)),
                _ => Response::Error(ErrorBody::server("the engine is unavailable")),
            };
        }
        let plain = match token.as_deref().map(Plain::parse) {
            Some(Ok(p)) => p,
            Some(Err(e)) => return Response::Error(sasl_error(self.dialect(), token.as_deref(), e)),
            None => return Response::Error(sasl_error(self.dialect(), None, "")),
        };
        let role = plain.user.strip_suffix(&format!("@{}", spec.id)).unwrap_or(&plain.user).to_string();
        if platform_only(&spec, sh.lease.db(), &role, &plain.password) {
            sh.client.authenticated(Some(&role));
            sh.state.lock().user = Some(role);
            return self.auth_ok();
        }
        let token = plain.with_user(&token.unwrap_or_default(), &role);
        let (session, answer) = call(sh.session(), Call::Authenticate(token)).await;
        let (Some(session), Answer::Auth(r)) = (session, answer) else {
            return Response::Error(ErrorBody::server("the engine is unavailable"));
        };
        match r {
            Ok(eng::AuthStep::Success(t)) => {
                sh.client.authenticated(session.state.user.as_deref());
                sh.state.lock().user = session.state.user.clone();
                Response::AuthSuccess(t)
            }
            Ok(eng::AuthStep::Challenge(c)) => Response::AuthChallenge(Some(c)),
            Err(e) => Response::Error(convert::error_body(&e, &self.ctx.features)),
        }
    }

    /// Starts forwarding the database's events to this connection (once).
    fn ensure_events(&mut self) {
        if self.events.is_some() {
            return;
        }
        let Some(sh) = self.shared.clone() else { return };
        let registered = self.registered.clone();
        let dialect = sh.lease.db().dialect();
        let mut from_db = sh.lease.db().events();
        self.events = Some(tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                let e = match from_db.recv().await {
                    Ok(e) => e,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => return,
                };
                if !pushed(dialect, &e) {
                    continue;
                }
                let Some(e) = convert::event(&e) else { continue };
                if !registered.lock().contains(&e.event_type()) {
                    continue;
                }
                let f = ResponseFrame::event(e).to_frame(sh.ctx);
                if sh.tx.send(Out::Frame(f)).await.is_err() {
                    return;
                }
            }
        }));
    }
}

/// Whether the reference pushes `e` to registered connections. ScyllaDB 2026.3 sends no
/// SCHEMA_CHANGE event for user-defined functions and aggregates (the DDL's own RESULT still
/// describes the change); Cassandra does. Drivers notice: gocql, for one, keeps serving cached
/// keyspace metadata without them.
fn pushed(d: eng::Dialect, e: &eng::Event) -> bool {
    !matches!(
        (d, e),
        (eng::Dialect::Scylla, eng::Event::SchemaChange(c)) if matches!(c.target, eng::response::ChangeTarget::Function | eng::response::ChangeTarget::Aggregate)
    )
}

/// A platform credential the engine can't check itself: it matches one of the spec's
/// credentials and isn't the database's `DbConfig::superuser` (which the engine created as a
/// role and authenticates with `PasswordAuthenticator`, the single source of truth — a rotated
/// password applies through `nosql_engine::auth::set_password`). Only a [`crate::Single`] whose
/// spec adds credentials to an already opened database has such credentials.
fn platform_only(spec: &DbSpec, db: &eng::Database, role: &str, password: &[u8]) -> bool {
    let engine_owns = eng::auth::requires_authentication(db.config()) && db.config().superuser.as_ref().is_some_and(|s| s.name == role);
    !engine_owns && spec.platform_login(role, password)
}

/// The database's authenticator class (`settings.authenticator`).
fn authenticator(spec: &DbSpec) -> eng::auth::certificate::Authenticator {
    eng::auth::certificate::Authenticator::of(&spec.config.settings.authenticator)
}

/// `BAD_CREDENTIALS` for a user that names no database.
pub fn bad_credentials(d: Dialect, user: &str) -> ErrorBody {
    let msg = match d {
        Dialect::Cassandra => format!("Provided username {user} and/or password are incorrect"),
        Dialect::Scylla => "Username and/or password are incorrect".to_string(),
    };
    ErrorBody::new(code::BAD_CREDENTIALS, msg)
}

/// The answer to an AUTH_RESPONSE whose token is not SASL PLAIN (`parse_error` from
/// [`Plain::parse`]; `token` is `None` for a null `[bytes]`). Captured from both references
/// with PasswordAuthenticator: Cassandra names the problem (and trips over a null token),
/// ScyllaDB only distinguishes an empty token.
pub fn sasl_error(d: Dialect, token: Option<&[u8]>, parse_error: &str) -> ErrorBody {
    match (d, token) {
        (Dialect::Cassandra, None) => {
            ErrorBody::server("java.lang.NullPointerException: Cannot invoke \"java.nio.ByteBuffer.remaining()\" because \"b\" is null")
        }
        (Dialect::Cassandra, Some(_)) => ErrorBody::new(code::BAD_CREDENTIALS, parse_error),
        (Dialect::Scylla, None | Some([])) => ErrorBody::new(code::BAD_CREDENTIALS, "Authentication ID must not be null"),
        (Dialect::Scylla, Some(_)) => ErrorBody::new(code::BAD_CREDENTIALS, "Username and/or password are incorrect"),
    }
}

/// A SASL PLAIN token (`[authzid] NUL authcid NUL password`), read from the end as
/// `PasswordAuthenticator.PlainTextSaslAuthenticator.decodeCredentials` does.
#[derive(Debug, PartialEq, Eq)]
pub struct Plain {
    pub user: String,
    pub password: Vec<u8>,
    /// Where the user starts in the token.
    user_at: usize,
}

impl Plain {
    pub fn parse(token: &[u8]) -> Result<Plain, &'static str> {
        let mut user: Option<(usize, usize)> = None;
        let mut pass: Option<(usize, usize)> = None;
        let mut end = token.len();
        for i in (0..token.len()).rev() {
            if token[i] == 0 {
                if pass.is_none() {
                    pass = Some((i + 1, end));
                } else if user.is_none() {
                    user = Some((i + 1, end));
                } else {
                    return Err("Credential format error: username or password is empty or contains NUL(\\0) character");
                }
                end = i;
            }
        }
        let pass = pass.filter(|(a, b)| b > a).ok_or("Password must not be null")?;
        let user = user.filter(|(a, b)| b > a).ok_or("Authentication ID must not be null")?;
        Ok(Plain {
            user: String::from_utf8_lossy(&token[user.0..user.1]).into_owned(),
            password: token[pass.0..pass.1].to_vec(),
            user_at: user.0,
        })
    }

    /// The token with another user (the role inside the routed database).
    pub fn with_user(&self, token: &[u8], user: &str) -> Bytes {
        let mut out = Vec::with_capacity(token.len());
        out.extend_from_slice(&token[..self.user_at]);
        out.extend_from_slice(user.as_bytes());
        out.push(0);
        out.extend_from_slice(&self.password);
        Bytes::from(out)
    }
}

/// ScyllaDB reads QUERY/EXECUTE flags as a set of v4 flags and refuses unknown bits before
/// reading anything else (Cassandra reads bit 0x80 as v5's keyspace flag on every version).
fn scylla_flags_overflow(f: &Frame, ctx: &Ctx) -> bool {
    if f.version >= Version::V5 || !matches!(f.opcode, Opcode::Query | Opcode::Execute) {
        return false;
    }
    let mut r = Reader::new(f.body.clone());
    let mut probe = || -> Result<u8, nosql_proto::DecodeError> {
        if f.flags.has(Flags::CUSTOM_PAYLOAD) {
            r.bytes_map()?;
        }
        if f.opcode == Opcode::Query {
            r.long_string()?;
        } else {
            r.short_bytes()?;
            if ctx.metadata_id() {
                r.short_bytes()?;
            }
        }
        r.u16()?;
        r.u8()
    };
    probe().is_ok_and(|flags| flags & 0x80 != 0)
}

/// An engine call.
pub(crate) enum Call {
    Query(String, eng::QueryParams),
    Prepare(String, Option<String>),
    Execute(Bytes, eng::QueryParams),
    Batch(eng::Batch),
    Authenticate(Bytes),
}

pub(crate) enum Answer {
    Response(eng::Result<eng::Response>),
    Prepared(eng::Result<eng::Prepared>),
    Auth(eng::Result<eng::AuthStep>),
}

/// Runs `c` on `s`; gives the session back with the answer.
pub(crate) async fn call(s: eng::Session, c: Call) -> (Option<eng::Session>, Answer) {
    call_traced(s, c, false).await
}

/// [`call`], traced into `system_traces` when `tracing` (the frame's TRACING flag).
pub(crate) async fn call_traced(mut s: eng::Session, c: Call, tracing: bool) -> (Option<eng::Session>, Answer) {
    s.set_tracing(tracing);
    let a = match c {
        Call::Query(t, p) => Answer::Response(s.query(&t, p).await),
        Call::Prepare(t, ks) => Answer::Prepared(s.prepare(&t, ks.as_deref()).await),
        Call::Execute(id, p) => Answer::Response(s.execute(&id, p).await),
        Call::Batch(b) => Answer::Response(s.batch(b).await),
        Call::Authenticate(t) => Answer::Auth(s.authenticate(&t).await),
    };
    (Some(s), a)
}

/// One request on a ready connection.
async fn execute(sh: Arc<Shared>, rf: RequestFrame, permit: OwnedSemaphorePermit) {
    let stream = rf.stream;
    let traced = rf.tracing;
    let client_ts = match &rf.request {
        Request::Query(q) => q.params.timestamp,
        Request::Execute(e) => e.params.timestamp,
        Request::Batch(b) => b.params.timestamp,
        _ => None,
    };
    if let Some(ts) = client_ts {
        crate::metrics::observe_client_timestamp(ts);
    }
    let out = match rf.request {
        Request::Query(q) => match sh.page_size_check(&q.params) {
            Some(e) => ResponseFrame::error(stream, e),
            None => {
                let (s, a) = call_traced(sh.session(), Call::Query(q.query, convert::params(&q.params)), traced).await;
                sh.respond(stream, s, a, MetadataRule { skip: q.params.skip_metadata, ..Default::default() })
            }
        },
        Request::Prepare(p) => {
            let (s, a) = call_traced(sh.session(), Call::Prepare(p.query, p.keyspace), traced).await;
            sh.respond(stream, s, a, MetadataRule::default())
        }
        Request::Execute(e) => match sh.page_size_check(&e.params) {
            Some(err) => ResponseFrame::error(stream, err),
            None => {
                let (s, a) = call_traced(sh.session(), Call::Execute(e.id, convert::params(&e.params)), traced).await;
                let client_id = if sh.ctx.metadata_id() { Some(e.result_metadata_id.as_deref().unwrap_or_default()) } else { None };
                sh.respond(stream, s, a, MetadataRule { skip: e.params.skip_metadata, client_id, ..Default::default() })
            }
        },
        Request::Batch(b) => {
            let (s, a) = call_traced(sh.session(), Call::Batch(convert::batch(&b)), traced).await;
            sh.respond(stream, s, a, MetadataRule::default())
        }
        // Handled by the reader.
        _ => ResponseFrame::error(stream, ErrorBody::server("unexpected request")),
    };
    let error = matches!(out.response, Response::Error(_));
    if let Response::Error(e) = &out.response {
        crate::metrics::count_error(e.code);
    }
    sh.client.request();
    let m = &sh.lease.metrics;
    Metrics::add(&m.requests, 1);
    if error {
        Metrics::add(&m.errors, 1);
    }
    let f = out.to_frame(sh.ctx);
    let _ = sh.tx.send(Out::Frame(f)).await;
    drop(permit);
}

impl Shared {
    /// An engine session carrying the connection's state.
    fn session(&self) -> eng::Session {
        let state = self.state.lock().clone();
        self.lease.db().session(state)
    }

    /// Cassandra refuses a zero page size (`QueryMessage`/`ExecuteMessage`); ScyllaDB pages
    /// with its default.
    fn page_size_check(&self, p: &nosql_proto::QueryParams) -> Option<ErrorBody> {
        (self.dialect == Dialect::Cassandra && p.page_size == Some(0)).then(|| ErrorBody::protocol("The page size cannot be 0"))
    }

    fn error_frame(&self, stream: i16, e: &eng::Error) -> ResponseFrame {
        let mut f = ResponseFrame::error(stream, convert::error_body(e, &self.ctx.features));
        f.warnings = e.warnings.clone();
        f.tracing_id = e.tracing_id;
        f
    }

    fn respond(&self, stream: i16, s: Option<eng::Session>, a: Answer, rule: MetadataRule) -> ResponseFrame {
        match a {
            Answer::Response(Ok(resp)) => {
                if let eng::ResponseBody::SetKeyspace(ks) = &resp.body {
                    let ks = s.and_then(|s| s.state.keyspace).unwrap_or_else(|| ks.clone());
                    self.client.set_keyspace(Some(&ks));
                    self.state.lock().keyspace = Some(ks);
                }
                ResponseFrame {
                    stream,
                    tracing_id: resp.tracing_id,
                    custom_payload: (!resp.custom_payload.is_empty()).then(|| resp.custom_payload.clone()),
                    response: Response::Result(convert::result_body(
                        &resp.body,
                        self.lease.db().dialect(),
                        MetadataRule { conditional: resp.conditional, ..rule },
                    )),
                    warnings: resp.warnings,
                }
            }
            Answer::Prepared(Ok(p)) => ResponseFrame {
                tracing_id: p.tracing_id,
                ..ResponseFrame::new(stream, Response::Result(convert::prepared(&p, self.lease.db().dialect(), self.lwt_mark)))
            },
            Answer::Response(Err(e)) | Answer::Prepared(Err(e)) | Answer::Auth(Err(e)) => self.error_frame(stream, &e),
            Answer::Auth(Ok(_)) => ResponseFrame::error(stream, ErrorBody::server("unexpected authentication step")),
        }
    }
}

/// Bytes in and out of a connection, charged to its database once it is bound.
#[derive(Default)]
struct IoCounters {
    metrics: OnceLock<Arc<Metrics>>,
    pending_in: AtomicU64,
    pending_out: AtomicU64,
}

impl IoCounters {
    fn add(&self, n: usize, inbound: bool) {
        match self.metrics.get() {
            Some(m) => Metrics::add(if inbound { &m.bytes_in } else { &m.bytes_out }, n as u64),
            None => {
                (if inbound { &self.pending_in } else { &self.pending_out }).fetch_add(n as u64, Ordering::Relaxed);
            }
        }
    }

    fn bind(&self, m: Arc<Metrics>) {
        if self.metrics.set(m.clone()).is_ok() {
            Metrics::add(&m.bytes_in, self.pending_in.swap(0, Ordering::Relaxed));
            Metrics::add(&m.bytes_out, self.pending_out.swap(0, Ordering::Relaxed));
        }
    }
}

/// A stream that counts its bytes.
pub struct Counted<T> {
    inner: T,
    counters: Arc<IoCounters>,
}

impl<T: AsyncRead + Unpin> AsyncRead for Counted<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let r = Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = buf.filled().len().saturating_sub(before);
        if n > 0 {
            self.counters.add(n, true);
        }
        r
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Counted<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &r {
            self.counters.add(*n, false);
        }
        r
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_tokens() {
        let p = Plain::parse(b"\0alice@n1\0secret").unwrap();
        assert_eq!((p.user.as_str(), &p.password[..]), ("alice@n1", &b"secret"[..]));
        assert_eq!(&p.with_user(b"\0alice@n1\0secret", "alice")[..], b"\0alice\0secret");
        let p = Plain::parse(b"authz\0bob\0pw").unwrap();
        assert_eq!(p.user, "bob");
        assert_eq!(&p.with_user(b"authz\0bob\0pw", "carol")[..], b"authz\0carol\0pw");
        assert_eq!(Plain::parse(b"bob\0pw"), Err("Authentication ID must not be null"));
        assert_eq!(Plain::parse(b"\0bob\0"), Err("Password must not be null"));
        assert_eq!(Plain::parse(b""), Err("Password must not be null"));
        assert_eq!(Plain::parse(b"\0\0pw"), Err("Authentication ID must not be null"));
        assert!(Plain::parse(b"a\0b\0c\0d").unwrap_err().starts_with("Credential format error"));
    }

    #[test]
    fn shards() {
        let a = Accepted { peer: "1.2.3.4:50003".parse().unwrap(), sni: None, shard_aware: true, tls: None, certificate: None };
        assert_eq!(shard_for(&a, 4), 3);
        assert_eq!(shard_for(&a, 1), 0);
        assert_eq!(shard_for(&a, 0), 0);
        let b = Accepted { shard_aware: false, ..a };
        let s: Vec<u32> = (0..4).map(|_| shard_for(&b, 2)).collect();
        assert!(s.contains(&0) && s.contains(&1));
    }

    #[test]
    fn credential_texts() {
        assert_eq!(bad_credentials(Dialect::Cassandra, "x@y").message, "Provided username x@y and/or password are incorrect");
        assert_eq!(bad_credentials(Dialect::Scylla, "x").code, code::BAD_CREDENTIALS);
    }

    #[test]
    fn scylla_flag_probe() {
        use bytes::{BufMut, BytesMut};
        let mut b = BytesMut::new();
        b.put_u32(1);
        b.put_slice(b"x");
        b.put_u16(1);
        b.put_u8(0x80);
        let f = Frame {
            version: Version::V4,
            direction: nosql_proto::Direction::Request,
            flags: Flags::default(),
            stream: 1,
            opcode: Opcode::Query,
            body: b.freeze(),
        };
        let ctx = Ctx::new(Version::V4).dialect(Dialect::Scylla);
        assert!(scylla_flags_overflow(&f, &ctx));
        let mut ok = f.clone();
        ok.body = Bytes::from_static(b"\x00\x00\x00\x01x\x00\x01\x04\x00\x00\x00\x10");
        assert!(!scylla_flags_overflow(&ok, &ctx));
        let mut v5 = f;
        v5.version = Version::V5;
        assert!(!scylla_flags_overflow(&v5, &ctx));
    }

    /// A transport that records each write call.
    struct Recorder(Arc<Mutex<Vec<Vec<u8>>>>);

    impl AsyncRead for Recorder {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    impl AsyncWrite for Recorder {
        fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
            self.0.lock().push(buf.to_vec());
            Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn recording_writer() -> (FrameWriter, Arc<Mutex<Vec<Vec<u8>>>>) {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let io: Box<dyn Io> = Box::new(Recorder(writes.clone()));
        let (_, w) = tokio::io::split(Counted { inner: io, counters: Arc::new(IoCounters::default()) });
        (FramedWrite::new(w, ServerEncoder::with_limits(Limits::of(Dialect::Scylla))), writes)
    }

    fn ready(stream: i16) -> Out {
        Out::Frame(ResponseFrame::new(stream, Response::Ready).to_frame(Ctx::new(Version::V4).dialect(Dialect::Scylla)))
    }

    /// Answers that become ready while the writer is about to flush go out in the same write,
    /// in order; a lone answer is written without waiting for more.
    #[tokio::test(flavor = "current_thread")]
    async fn write_loop_coalesces_answers_of_one_scheduler_pass() {
        let (w, writes) = recording_writer();
        let (tx, rx) = mpsc::channel(16);
        let writer = tokio::spawn(write_loop(w, rx));
        let tx2 = tx.clone();
        // The first answer wakes the writer; the second comes from a task spawned after that
        // wake (a request finishing later in the same pass).
        tokio::spawn(async move {
            tx.send(ready(1)).await.unwrap();
            tokio::spawn(async move { tx2.send(ready(2)).await.unwrap() });
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let w = writes.lock().clone();
        assert_eq!(w.len(), 1, "one write for both answers");
        let one = ResponseFrame::new(1, Response::Ready).to_frame(Ctx::new(Version::V4).dialect(Dialect::Scylla));
        let mut enc = ServerEncoder::with_limits(Limits::of(Dialect::Scylla));
        let mut expect = bytes::BytesMut::new();
        tokio_util::codec::Encoder::encode(&mut enc, one, &mut expect).unwrap();
        let two = ResponseFrame::new(2, Response::Ready).to_frame(Ctx::new(Version::V4).dialect(Dialect::Scylla));
        tokio_util::codec::Encoder::encode(&mut enc, two, &mut expect).unwrap();
        assert_eq!(w[0], expect.to_vec(), "both frames, in order");
        writer.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_loop_writes_a_lone_answer_at_once() {
        let (w, writes) = recording_writer();
        let (tx, rx) = mpsc::channel(16);
        let writer = tokio::spawn(write_loop(w, rx));
        tx.send(ready(7)).await.unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(writes.lock().len(), 1);
        tx.send(Out::Close).await.unwrap();
        writer.await.unwrap();
    }
}
