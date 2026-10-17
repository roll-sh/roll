//! Postgres wire-protocol proxy.
//!
//! Clients connect here instead of to a database. The proxy reads the startup packet,
//! resolves the target endpoint (`options=endpoint=<id>`), asks the [`Router`] to wake it
//! (starting a scaled-to-zero compute if needed), then relays bytes in both directions.
//! Authentication is end-to-end between client and Postgres; the proxy only watches the
//! handshake long enough to learn the backend's cancel key so `pg_cancel` keeps working.

pub mod protocol;

use bytes::BytesMut;
use protocol::{Startup, error_response, read_message, read_startup, take_endpoint_option, write_all};
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncWriteExt, copy};
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

pub struct Proxy<R: Router> {
    router: Arc<R>,
    config: ProxyConfig,
    cancel_map: Arc<Mutex<HashMap<CancelKey, SocketAddr>>>,
}

impl<R: Router> Proxy<R> {
    pub fn new(router: Arc<R>, config: ProxyConfig) -> Self {
        Self { router, config, cancel_map: Arc::default() }
    }

    pub async fn serve(self) -> anyhow::Result<()> {
        let listener = TcpListener::bind(self.config.listen).await?;
        info!(addr = %self.config.listen, "postgres proxy listening");
        let this = Arc::new(self);
        loop {
            let (socket, peer) = listener.accept().await?;
            let this = this.clone();
            tokio::spawn(async move {
                if let Err(e) = this.handle(socket).await {
                    debug!(%peer, error = %e, "connection ended with error");
                }
            });
        }
    }

    async fn handle(&self, mut client: TcpStream) -> anyhow::Result<()> {
        client.set_nodelay(true)?;
        let (version, mut params) = loop {
            match read_startup(&mut client).await? {
                // TLS / GSS encryption are terminated at the edge (not yet); tell libpq to continue in plaintext.
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
        let Some(endpoint) = endpoint.or_else(|| self.config.default_endpoint.clone()) else {
            let msg = "no endpoint specified: add ?options=endpoint%3D<endpoint_id> to the connection string";
            write_all(&mut client, &error_response("08004", msg)).await?;
            return Ok(());
        };

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
        let result = self.relay(client, backend, addr, &mut cancel_key).await;
        if let Some(key) = cancel_key {
            self.cancel_map.lock().unwrap().remove(&key);
        }
        result
    }

    async fn relay(
        &self,
        client: TcpStream,
        backend: TcpStream,
        addr: SocketAddr,
        cancel_key: &mut Option<CancelKey>,
    ) -> anyhow::Result<()> {
        let (mut client_r, mut client_w) = client.into_split();
        let (mut backend_r, mut backend_w) = backend.into_split();

        let client_to_backend = async {
            let _ = copy(&mut client_r, &mut backend_w).await;
            let _ = backend_w.shutdown().await;
        };

        let backend_to_client = async {
            // Watch the handshake until ReadyForQuery to capture BackendKeyData, then splice.
            let mut buf = BytesMut::with_capacity(1024);
            loop {
                buf.clear();
                let tag = match read_message(&mut backend_r, &mut buf).await {
                    Ok(t) => t,
                    Err(_) => return,
                };
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

struct ConnGuard<'a, R: Router> {
    router: &'a R,
    endpoint: &'a str,
}

impl<R: Router> Drop for ConnGuard<'_, R> {
    fn drop(&mut self) {
        self.router.connection_closed(self.endpoint);
    }
}
