//! Postgres wire-protocol proxy.
//!
//! Clients connect here instead of to a database. The proxy reads the startup packet,
//! resolves the target endpoint (`options=endpoint=<id>`), asks the [`Router`] to wake it
//! (starting a scaled-to-zero compute if needed), then relays bytes in both directions.
//! Authentication is end-to-end between client and Postgres; the proxy only watches the
//! handshake long enough to learn the backend's cancel key so `pg_cancel` keeps working. The
//! exception is a decoy role ([`decoy`]): the proxy refuses its login itself, as Postgres would.

pub mod decoy;
pub mod protocol;

use bytes::BytesMut;
use protocol::{Startup, error_response, read_message, read_startup, take_endpoint_option, write_all};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, copy};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

#[derive(Debug)]
pub enum RouteError {
    /// No such endpoint (or it was deleted).
    NotFound(String),
    /// The endpoint exists but could not be started.
    Unavailable(String),
}

/// Resolves endpoints to live compute addresses. Implemented by the control plane.
pub trait Router: Send + Sync + 'static {
    /// Return the address of a running compute for `endpoint`, starting it if suspended.
    fn wake(&self, endpoint: &str) -> impl Future<Output = Result<SocketAddr, RouteError>> + Send;
    /// Whether `user` is a decoy role at `endpoint` (roll secrets' Postgres decoys). Asked on
    /// every login before [`Router::wake`], so it's one indexed read (it may wait, once per
    /// compute image, while the image's answers are learned). A decoy's login is told to whoever
    /// listens (`peer`: the client), and the proxy refuses it without a compute when it knows the
    /// image's [`decoy::Answers`].
    fn decoy(&self, endpoint: &str, user: &str, peer: Option<SocketAddr>) -> impl Future<Output = Option<decoy::Decoy>> + Send {
        let _ = (endpoint, user, peer);
        async { None }
    }
    fn connection_opened(&self, endpoint: &str);
    fn connection_closed(&self, endpoint: &str);
}

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    /// Endpoint used when a client doesn't name one (handy for tools that can't set `options`).
    pub default_endpoint: Option<String>,
}

type CancelKey = (i32, Vec<u8>);

/// How long an endpoint's compute takes to answer a startup when it's up, until one is measured.
const WARM_DEFAULT: Duration = Duration::from_millis(2);
/// A first answer slower than this came from a compute that was started for it: not a sample.
const WARM_MAX: Duration = Duration::from_millis(250);

/// What the proxy saw of an endpoint's real logins: a decoy's refusal copies them.
#[derive(Default)]
struct Seen {
    /// From asking to wake a running compute to its first answer, smoothed.
    warm: Option<Duration>,
}

pub struct Proxy<R: Router> {
    router: Arc<R>,
    config: ProxyConfig,
    cancel_map: Arc<Mutex<HashMap<CancelKey, SocketAddr>>>,
    /// By endpoint, for endpoints that woke (so only real ones).
    seen: Mutex<HashMap<String, Seen>>,
}

impl<R: Router> Proxy<R> {
    pub fn new(router: Arc<R>, config: ProxyConfig) -> Self {
        Self { router, config, cancel_map: Arc::default(), seen: Mutex::default() }
    }

    pub async fn serve(self) -> anyhow::Result<()> {
        Arc::new(self).serve_shared().await
    }

    /// [`Proxy::serve`] for a proxy that also serves connections from elsewhere (the edge).
    pub async fn serve_shared(self: Arc<Self>) -> anyhow::Result<()> {
        let listener = TcpListener::bind(self.config.listen).await?;
        info!(addr = %self.config.listen, "postgres proxy listening");
        let this = self;
        loop {
            let (socket, peer) = listener.accept().await?;
            let this = this.clone();
            tokio::spawn(async move {
                if let Err(e) = this.handle(socket, peer).await {
                    debug!(%peer, error = %e, "connection ended with error");
                }
            });
        }
    }

    async fn handle(&self, client: TcpStream, peer: SocketAddr) -> anyhow::Result<()> {
        client.set_nodelay(true)?;
        self.serve_connection(client, None, Some(peer)).await
    }

    /// Serve a connection whose transport is already set up (the edge's TLS stream, a splice).
    /// `named`: the endpoint the connection's host named (`<id>.pg.<region>.<domain>` in its
    /// TLS server name); a client may name one in `options=endpoint=` instead, but not another.
    /// `peer`: the client's address (after a PROXY header), for a decoy's alert.
    pub async fn serve_connection<S>(&self, mut client: S, named: Option<String>, peer: Option<SocketAddr>) -> anyhow::Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let (version, mut params) = loop {
            match read_startup(&mut client).await? {
                // Plain listener: no TLS here (roll-edge terminates it in front). Inside TLS: already encrypted.
                Startup::Ssl | Startup::GssEnc => write_all(&mut client, b"N").await?,
                Startup::Cancel { pid, key } => return self.forward_cancel(pid, key).await,
                Startup::Params { version, params } => break (version, params),
            }
        };

        let mut endpoint = None;
        if let Some(i) = params.iter().position(|(k, _)| k == "options") {
            let (ep, rest) = take_endpoint_option(&params[i].1);
            endpoint = ep;
            if rest.is_empty() {
                params.remove(i);
            } else {
                params[i].1 = rest;
            }
        }
        if let (Some(named), Some(asked)) = (&named, &endpoint)
            && named != asked
        {
            let msg = format!("the host names endpoint {named} but options name {asked}");
            write_all(&mut client, &error_response("08004", &msg)).await?;
            return Ok(());
        }
        let Some(endpoint) = endpoint.or(named).or_else(|| self.config.default_endpoint.clone()) else {
            let msg = "no endpoint specified: add ?options=endpoint%3D<endpoint_id> to the connection string";
            write_all(&mut client, &error_response("08004", msg)).await?;
            return Ok(());
        };

        // Postgres cuts a longer name to 63 bytes, and no decoy's name is that long.
        let user = params.iter().find(|(k, _)| k == "user").map(|(_, v)| v.as_str()).filter(|u| !u.is_empty() && u.len() < 64);
        // A decoy whose image's answers roll doesn't know goes to Postgres, told all the same.
        if let Some(user) = user
            && let Some(d) = self.router.decoy(&endpoint, user, peer).await
            && let Some(answers) = &d.answers
        {
            let warm = self.seen.lock().unwrap().get(&endpoint).and_then(|s| s.warm).unwrap_or(WARM_DEFAULT);
            decoy::refuse(&mut client, version, &params, user, &d, warm, answers).await?;
            return Ok(());
        }

        let asked = Instant::now();
        let addr = match self.router.wake(&endpoint).await {
            Ok(addr) => addr,
            Err(RouteError::NotFound(m)) => {
                write_all(&mut client, &error_response("3D000", &m)).await?;
                return Ok(());
            }
            Err(RouteError::Unavailable(m)) => {
                write_all(&mut client, &error_response("57P03", &m)).await?;
                return Ok(());
            }
        };

        let mut backend = match TcpStream::connect(addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!(%endpoint, %addr, error = %e, "compute unreachable");
                let msg = format!("endpoint {endpoint} is not reachable");
                write_all(&mut client, &error_response("57P03", &msg)).await?;
                return Ok(());
            }
        };
        backend.set_nodelay(true)?;
        write_all(&mut backend, &protocol::encode_startup(version, &params)).await?;

        self.router.connection_opened(&endpoint);
        let _guard = ConnGuard { router: self.router.as_ref(), endpoint: &endpoint };
        let mut cancel_key = None;
        let login = Login { endpoint: &endpoint, asked };
        let result = self.relay(client, backend, addr, &mut cancel_key, login).await;
        if let Some(key) = cancel_key {
            self.cancel_map.lock().unwrap().remove(&key);
        }
        result
    }

    async fn relay<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        client: S,
        backend: TcpStream,
        addr: SocketAddr,
        cancel_key: &mut Option<CancelKey>,
        login: Login<'_>,
    ) -> anyhow::Result<()> {
        let (mut client_r, mut client_w) = tokio::io::split(client);
        let (mut backend_r, mut backend_w) = backend.into_split();

        let client_to_backend = async {
            let _ = copy(&mut client_r, &mut backend_w).await;
            let _ = backend_w.shutdown().await;
        };

        let backend_to_client = async {
            // Watch the handshake until ReadyForQuery to capture BackendKeyData, then splice.
            let mut buf = BytesMut::with_capacity(1024);
            let mut first = true;
            loop {
                buf.clear();
                let tag = match read_message(&mut backend_r, &mut buf).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                if std::mem::take(&mut first) {
                    self.saw_answer(&login);
                }
                if tag == b'K' && buf.len() >= 13 {
                    let pid = i32::from_be_bytes(buf[5..9].try_into().unwrap());
                    let key = (pid, buf[9..].to_vec());
                    self.cancel_map.lock().unwrap().insert(key.clone(), addr);
                    *cancel_key = Some(key);
                }
                if client_w.write_all(&buf).await.is_err() {
                    return;
                }
                if tag == b'Z' || tag == b'E' {
                    break;
                }
            }
            let _ = client_w.flush().await;
            let _ = copy(&mut backend_r, &mut client_w).await;
            let _ = client_w.shutdown().await;
        };

        // Whichever side finishes first ends the session.
        tokio::select! {
            _ = client_to_backend => {}
            _ = backend_to_client => {}
        }
        Ok(())
    }

    /// A compute answered a startup: how long it took, when it was up already.
    fn saw_answer(&self, login: &Login<'_>) {
        let took = login.asked.elapsed();
        if took >= WARM_MAX {
            return;
        }
        let mut seen = self.seen.lock().unwrap();
        match seen.get_mut(login.endpoint) {
            Some(s) => s.warm = Some(s.warm.map_or(took, |w| (w * 7 + took) / 8)),
            None => drop(seen.insert(login.endpoint.to_string(), Seen { warm: Some(took) })),
        }
    }

    async fn forward_cancel(&self, pid: i32, key: Vec<u8>) -> anyhow::Result<()> {
        let addr = self.cancel_map.lock().unwrap().get(&(pid, key.clone())).copied();
        // Unknown keys are dropped silently, exactly like Postgres does.
        if let Some(addr) = addr {
            let mut s = TcpStream::connect(addr).await?;
            write_all(&mut s, &protocol::encode_cancel(pid, &key)).await?;
        }
        Ok(())
    }
}

/// A login the proxy relays.
struct Login<'a> {
    endpoint: &'a str,
    /// When the proxy asked to wake its compute.
    asked: Instant,
}

struct ConnGuard<'a, R: Router> {
    router: &'a R,
    endpoint: &'a str,
}

impl<R: Router> Drop for ConnGuard<'_, R> {
    fn drop(&mut self) {
        self.router.connection_closed(self.endpoint);
    }
}
