//! A small async CQL client (feature `client`), for `roll nosql cqlsh` and the differential
//! test harness.
//!
//! [`Client::connect`] opens TCP (optionally TLS), sends OPTIONS, and negotiates the version:
//! it starts at the configured version (v5 by default) and steps down while the server
//! answers with a protocol-version error, reconnecting each time (ScyllaDB closes the
//! connection on an unsupported version). It then sends STARTUP with the requested
//! compression, answers AUTHENTICATE with SASL PLAIN, and switches to v5 segments when the
//! version calls for it.
//!
//! Requests are multiplexed over stream ids; one background task reads responses and routes
//! them, and pushes EVENTs (stream -1) to the receiver [`Client::register`] returns. A timed
//! out request keeps its stream id reserved until the late answer arrives, so ids are never
//! reused while a response is still due.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::stream::{SplitSink, StreamExt};
use futures::SinkExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_rustls::rustls;
use tokio_util::codec::Framed;

use crate::codec::{ClientCodec, CodecError};
use crate::compress::Compression;
use crate::error::DecodeError;
use crate::error_body::{ErrorBody, code};
use crate::event::{Event, EventType};
use crate::frame::Frame;
use crate::request::{Batch, Execute, Prepare, Query, QueryParams, Request, RequestFrame, Startup, startup_key};
use crate::response::{Response, ResponseFrame};
use crate::result::{Prepared, ResultBody, Rows};
use crate::supported::Supported;
use crate::version::{Ctx, Dialect, Features, Version};

/// TLS settings.
#[derive(Clone)]
pub struct TlsConfig {
    pub config: Arc<rustls::ClientConfig>,
    /// The SNI name and the name the certificate must match.
    pub server_name: String,
}

impl TlsConfig {
    /// Verifies the server against the platform's root certificates.
    pub fn native_roots(server_name: impl Into<String>) -> Result<Self, ClientError> {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        Self::with_roots(roots, server_name)
    }

    pub fn with_roots(roots: rustls::RootCertStore, server_name: impl Into<String>) -> Result<Self, ClientError> {
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| ClientError::Tls(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(TlsConfig { config: Arc::new(config), server_name: server_name.into() })
    }
}

impl std::fmt::Debug for TlsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConfig").field("server_name", &self.server_name).finish()
    }
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// `host:port`.
    pub addr: String,
    /// The version to start negotiating from.
    pub max_version: Version,
    /// Pin the version: fail instead of stepping down.
    pub exact_version: bool,
    pub compression: Option<Compression>,
    /// SASL PLAIN user and password.
    pub credentials: Option<(String, String)>,
    pub tls: Option<TlsConfig>,
    pub connect_timeout: Duration,
    /// Per request; `None` waits forever.
    pub request_timeout: Option<Duration>,
    /// Extra STARTUP options (DRIVER_NAME etc.).
    pub startup: Vec<(String, String)>,
    /// Turn on the ScyllaDB extensions the server offers (metadata ids on v4, rate-limit
    /// errors).
    pub scylla_extensions: bool,
    pub max_in_flight: usize,
}

impl ClientConfig {
    pub fn new(addr: impl Into<String>) -> Self {
        ClientConfig {
            addr: addr.into(),
            max_version: Version::V5,
            exact_version: false,
            compression: None,
            credentials: None,
            tls: None,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Some(Duration::from_secs(30)),
            startup: vec![
                (startup_key::DRIVER_NAME.into(), "roll nosql-proto".into()),
                (startup_key::DRIVER_VERSION.into(), env!("CARGO_PKG_VERSION").into()),
            ],
            scylla_extensions: false,
            max_in_flight: 1024,
        }
    }

    /// Exactly this version, no negotiation.
    pub fn version(mut self, v: Version) -> Self {
        self.max_version = v;
        self.exact_version = true;
        self
    }

    pub fn compression(mut self, c: Option<Compression>) -> Self {
        self.compression = c;
        self
    }

    pub fn credentials(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.credentials = Some((user.into(), password.into()));
        self
    }

    pub fn tls(mut self, tls: TlsConfig) -> Self {
        self.tls = Some(tls);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("malformed response: {0}")]
    Decode(#[from] DecodeError),
    /// The server answered with an ERROR.
    #[error("{0}")]
    Server(ErrorBody),
    #[error("unexpected response: {0}")]
    Unexpected(String),
    #[error("authentication: {0}")]
    Auth(String),
    #[error("TLS: {0}")]
    Tls(String),
    #[error("request timed out")]
    Timeout,
    #[error("connection closed")]
    Closed,
    #[error("all stream ids are in use")]
    Busy,
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

type Conn = Framed<Box<dyn Io>, ClientCodec>;

type Reply = oneshot::Sender<Result<ResponseFrame, ClientError>>;

#[derive(Default)]
struct Streams {
    next: i16,
    free: Vec<i16>,
    pending: HashMap<i16, Reply>,
    closed: bool,
}

struct Shared {
    ctx: Ctx,
    streams: Mutex<Streams>,
    events: Mutex<Option<mpsc::UnboundedSender<Event>>>,
}

impl Shared {
    fn fail_all(&self) {
        let mut s = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        s.closed = true;
        for (_, tx) in s.pending.drain() {
            let _ = tx.send(Err(ClientError::Closed));
        }
    }
}

struct Reader(tokio::task::JoinHandle<()>);

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A connection. Cheap to clone; all clones share it.
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
    writer: Arc<tokio::sync::Mutex<SplitSink<Conn, Frame>>>,
    slots: Arc<Semaphore>,
    timeout: Option<Duration>,
    supported: Arc<Supported>,
    _reader: Arc<Reader>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("ctx", &self.shared.ctx).finish()
    }
}

/// One request/response exchange during the handshake (stream 0).
async fn exchange(conn: &mut Conn, ctx: Ctx, req: Request) -> Result<ResponseFrame, ClientError> {
    conn.send(RequestFrame::new(0, req).to_frame(ctx)).await?;
    let f = conn.next().await.ok_or(ClientError::Closed)??;
    Ok(ResponseFrame::decode(ctx, &f)?)
}

fn version_rejected(e: &ErrorBody) -> bool {
    e.code == code::PROTOCOL_ERROR && (e.message.contains("protocol version") || e.message.contains("Beta version"))
}

enum Attempt {
    Done(Client),
    Downgrade,
}

impl Client {
    pub async fn connect(cfg: ClientConfig) -> Result<Client, ClientError> {
        let mut v = cfg.max_version;
        loop {
            match Self::try_connect(&cfg, v).await? {
                Attempt::Done(c) => return Ok(c),
                Attempt::Downgrade => {
                    v = match v {
                        Version::V6 => Version::V5,
                        Version::V5 => Version::V4,
                        Version::V4 => Version::V3,
                        Version::V3 => return Err(ClientError::Unexpected("no common protocol version".into())),
                    }
                }
            }
        }
    }

    async fn open(cfg: &ClientConfig) -> Result<Box<dyn Io>, ClientError> {
        let tcp = tokio::time::timeout(cfg.connect_timeout, TcpStream::connect(&cfg.addr))
            .await
            .map_err(|_| ClientError::Timeout)??;
        tcp.set_nodelay(true)?;
        let Some(tls) = &cfg.tls else { return Ok(Box::new(tcp)) };
        let name = rustls::pki_types::ServerName::try_from(tls.server_name.clone()).map_err(|e| ClientError::Tls(e.to_string()))?;
        let stream = tokio_rustls::TlsConnector::from(tls.config.clone()).connect(name, tcp).await?;
        Ok(Box::new(stream))
    }

    async fn try_connect(cfg: &ClientConfig, version: Version) -> Result<Attempt, ClientError> {
        let mut conn: Conn = Framed::new(Self::open(cfg).await?, ClientCodec::new());
        let mut ctx = Ctx::new(version);
        let supported = match exchange(&mut conn, ctx, Request::Options).await {
            Ok(ResponseFrame { response: Response::Supported(s), .. }) => s,
            Ok(ResponseFrame { response: Response::Error(e), .. }) if version_rejected(&e) && !cfg.exact_version => {
                return Ok(Attempt::Downgrade);
            }
            Ok(ResponseFrame { response: Response::Error(e), .. }) => return Err(ClientError::Server(e)),
            Ok(other) => return Err(ClientError::Unexpected(format!("{:?} to OPTIONS", other.response.opcode()))),
            // ScyllaDB answers an unsupported version and hangs up; a closed connection
            // without an answer means the same.
            Err(ClientError::Closed) if !cfg.exact_version => return Ok(Attempt::Downgrade),
            Err(e) => return Err(e),
        };
        if supported.has("SCYLLA_SHARD") {
            ctx.dialect = Dialect::Scylla;
        }
        let mut startup = Startup::new("3.0.0");
        startup.options.extend(cfg.startup.iter().cloned());
        if let Some(c) = cfg.compression {
            startup.options.push((startup_key::COMPRESSION.into(), c.name().into()));
        }
        let mut features = Features::default();
        if cfg.scylla_extensions {
            let offered = supported.scylla_features();
            if offered.metadata_id {
                startup.options.push((startup_key::SCYLLA_USE_METADATA_ID.into(), String::new()));
                features.metadata_id = true;
            }
            if let Some(mark) = supported.get(startup_key::SCYLLA_LWT_ADD_METADATA_MARK).and_then(|v| v.first()) {
                startup.options.push((startup_key::SCYLLA_LWT_ADD_METADATA_MARK.into(), mark.clone()));
            }
            if let Some(c) = offered.rate_limit_code {
                startup.options.push((startup_key::SCYLLA_RATE_LIMIT_ERROR.into(), String::new()));
                features.rate_limit_code = Some(c);
            }
        }
        ctx.features = features;
        // v3/v4: the answer to STARTUP is already compressed (STARTUP itself never is).
        if !version.uses_segments() {
            conn.codec_mut().set_compression(cfg.compression);
        }
        let mut resp = exchange(&mut conn, ctx, Request::Startup(startup)).await?;
        if version.uses_segments() {
            conn.codec_mut().start_segments(cfg.compression == Some(Compression::Lz4));
        }
        loop {
            match resp.response {
                Response::Ready | Response::AuthSuccess(_) => break,
                Response::Authenticate(_) => {
                    let Some((user, pass)) = &cfg.credentials else {
                        return Err(ClientError::Auth("the server requires authentication".into()));
                    };
                    let mut token = Vec::with_capacity(user.len() + pass.len() + 2);
                    token.push(0);
                    token.extend_from_slice(user.as_bytes());
                    token.push(0);
                    token.extend_from_slice(pass.as_bytes());
                    resp = exchange(&mut conn, ctx, Request::AuthResponse(Some(Bytes::from(token)))).await?;
                }
                Response::AuthChallenge(_) => return Err(ClientError::Auth("unexpected SASL challenge for PLAIN".into())),
                Response::Error(e) if version_rejected(&e) && !cfg.exact_version => return Ok(Attempt::Downgrade),
                Response::Error(e) => return Err(ClientError::Server(e)),
                other => return Err(ClientError::Unexpected(format!("{:?} to STARTUP", other.opcode()))),
            }
        }
        let (sink, mut stream) = conn.split();
        let shared = Arc::new(Shared {
            ctx,
            streams: Mutex::new(Streams { next: 1, ..Default::default() }),
            events: Mutex::new(None),
        });
        let rs = shared.clone();
        let task = tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                let Ok(frame) = item else { break };
                let decoded = ResponseFrame::decode(rs.ctx, &frame).map_err(ClientError::from);
                if frame.stream < 0 {
                    if let Ok(ResponseFrame { response: Response::Event(e), .. }) = decoded {
                        let tx = rs.events.lock().unwrap_or_else(|e| e.into_inner()).clone();
                        if let Some(tx) = tx {
                            let _ = tx.send(e);
                        }
                    }
                    continue;
                }
                let tx = {
                    let mut s = rs.streams.lock().unwrap_or_else(|e| e.into_inner());
                    let tx = s.pending.remove(&frame.stream);
                    if tx.is_some() {
                        s.free.push(frame.stream);
                    }
                    tx
                };
                if let Some(tx) = tx {
                    let _ = tx.send(decoded);
                }
            }
            rs.fail_all();
            *rs.events.lock().unwrap_or_else(|e| e.into_inner()) = None;
        });
        Ok(Attempt::Done(Client {
            shared,
            writer: Arc::new(tokio::sync::Mutex::new(sink)),
            slots: Arc::new(Semaphore::new(cfg.max_in_flight.clamp(1, i16::MAX as usize))),
            timeout: cfg.request_timeout,
            supported: Arc::new(supported),
            _reader: Arc::new(Reader(task)),
        }))
    }

    pub fn ctx(&self) -> Ctx {
        self.shared.ctx
    }

    pub fn version(&self) -> Version {
        self.shared.ctx.version
    }

    /// What OPTIONS returned.
    pub fn supported(&self) -> &Supported {
        &self.supported
    }

    /// Sends a request with its envelope (the stream id is assigned here) and returns the
    /// raw response, ERRORs included.
    pub async fn send(&self, mut req: RequestFrame) -> Result<ResponseFrame, ClientError> {
        let _slot = self.slots.acquire().await.map_err(|_| ClientError::Closed)?;
        let (tx, rx) = oneshot::channel();
        let stream = {
            let mut s = self.shared.streams.lock().unwrap_or_else(|e| e.into_inner());
            if s.closed {
                return Err(ClientError::Closed);
            }
            let id = match s.free.pop() {
                Some(id) => id,
                None if s.next < i16::MAX => {
                    s.next += 1;
                    s.next - 1
                }
                None => return Err(ClientError::Busy),
            };
            s.pending.insert(id, tx);
            id
        };
        req.stream = stream;
        let frame = req.to_frame(self.shared.ctx);
        if let Err(e) = self.writer.lock().await.send(frame).await {
            let mut s = self.shared.streams.lock().unwrap_or_else(|e| e.into_inner());
            if s.pending.remove(&stream).is_some() {
                s.free.push(stream);
            }
            return Err(e.into());
        }
        let got = match self.timeout {
            Some(t) => tokio::time::timeout(t, rx).await.map_err(|_| ClientError::Timeout)?,
            None => rx.await,
        };
        got.map_err(|_| ClientError::Closed)?
    }

    /// Sends a request; an ERROR becomes [`ClientError::Server`].
    pub async fn request(&self, req: Request) -> Result<Response, ClientError> {
        match self.send(RequestFrame::new(0, req)).await?.response {
            Response::Error(e) => Err(ClientError::Server(e)),
            r => Ok(r),
        }
    }

    async fn result(&self, req: Request) -> Result<ResultBody, ClientError> {
        match self.request(req).await? {
            Response::Result(r) => Ok(r),
            other => Err(ClientError::Unexpected(format!("{:?}", other.opcode()))),
        }
    }

    pub async fn query(&self, cql: &str, params: QueryParams) -> Result<ResultBody, ClientError> {
        self.result(Request::Query(Query { query: cql.into(), params })).await
    }

    pub async fn prepare(&self, cql: &str) -> Result<Prepared, ClientError> {
        self.prepare_in(cql, None).await
    }

    /// PREPARE with v5's per-request keyspace.
    pub async fn prepare_in(&self, cql: &str, keyspace: Option<&str>) -> Result<Prepared, ClientError> {
        match self.result(Request::Prepare(Prepare { query: cql.into(), keyspace: keyspace.map(Into::into) })).await? {
            ResultBody::Prepared(p) => Ok(p),
            other => Err(ClientError::Unexpected(format!("result kind {} to PREPARE", other.kind()))),
        }
    }

    pub async fn execute(&self, p: &Prepared, params: QueryParams) -> Result<ResultBody, ClientError> {
        self.result(Request::Execute(Execute {
            id: p.id.clone(),
            result_metadata_id: p.result_metadata_id.clone(),
            params,
            tablet_version_block: None,
        }))
        .await
    }

    pub async fn batch(&self, b: Batch) -> Result<ResultBody, ClientError> {
        self.result(Request::Batch(b)).await
    }

    /// Pages through a query (`params.page_size` sets the page size).
    pub fn pages(&self, cql: &str, params: QueryParams) -> Pager {
        Pager { client: self.clone(), target: Target::Query(cql.into()), params, done: false }
    }

    /// Pages through a prepared statement.
    pub fn pages_prepared(&self, p: &Prepared, params: QueryParams) -> Pager {
        Pager { client: self.clone(), target: Target::Prepared(p.clone()), params, done: false }
    }

    /// REGISTERs for events; they arrive on the returned channel (replacing any earlier one).
    pub async fn register(&self, events: &[EventType]) -> Result<mpsc::UnboundedReceiver<Event>, ClientError> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.shared.events.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        match self.request(Request::Register(events.iter().map(|e| e.name().to_string()).collect())).await? {
            Response::Ready => Ok(rx),
            other => Err(ClientError::Unexpected(format!("{:?} to REGISTER", other.opcode()))),
        }
    }
}

enum Target {
    Query(String),
    Prepared(Prepared),
}

/// Iterates the pages of a result.
pub struct Pager {
    client: Client,
    target: Target,
    params: QueryParams,
    done: bool,
}

impl Pager {
    /// The next page, or `None` after the last.
    pub async fn next_page(&mut self) -> Result<Option<Rows>, ClientError> {
        if self.done {
            return Ok(None);
        }
        let r = match &self.target {
            Target::Query(q) => self.client.query(q, self.params.clone()).await?,
            Target::Prepared(p) => self.client.execute(p, self.params.clone()).await?,
        };
        let ResultBody::Rows(rows) = r else {
            self.done = true;
            return Err(ClientError::Unexpected(format!("result kind {} while paging", r.kind())));
        };
        self.params.paging_state = rows.metadata.paging_state.clone();
        self.done = self.params.paging_state.is_none();
        Ok(Some(rows))
    }

    /// Every remaining row's cells.
    pub async fn collect(mut self) -> Result<Vec<Vec<Option<Bytes>>>, ClientError> {
        let mut out = vec![];
        while let Some(page) = self.next_page().await? {
            out.extend(page.to_vec());
        }
        Ok(out)
    }
}
