//! The accept loop every public TCP listener shares: the PROXY header (when the NLB sends one),
//! then the TLS handshake, both under a deadline, then the product's handler.

use crate::proxy_protocol;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, warn};

/// How a connection arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// The client: the PROXY header's source, else the socket's peer.
    pub peer: SocketAddr,
    /// This node's address it reached.
    pub local: SocketAddr,
    /// The TLS server name, lowercase.
    pub sni: Option<String>,
    /// The protocol ALPN settled on.
    pub alpn: Option<Vec<u8>>,
}

#[derive(Clone, Copy, Debug)]
pub struct ListenOptions {
    /// Every connection starts with a PROXY header (behind the NLB).
    pub proxy_protocol: bool,
    /// For the PROXY header and the TLS handshake together.
    pub handshake_timeout: Duration,
}

impl Default for ListenOptions {
    fn default() -> Self {
        Self { proxy_protocol: false, handshake_timeout: Duration::from_secs(10) }
    }
}

/// The client's address: read the PROXY header if one is expected. A `LOCAL` header (the NLB's
/// health check) leaves the socket's peer.
pub async fn client_addr(sock: &mut TcpStream, peer: SocketAddr, proxy_protocol: bool) -> std::io::Result<SocketAddr> {
    if !proxy_protocol {
        return Ok(peer);
    }
    Ok(proxy_protocol::read(sock).await?.source.unwrap_or(peer))
}

/// Finish a TLS handshake on an accepted socket.
pub async fn handshake(sock: TcpStream, peer: SocketAddr, tls: &TlsAcceptor) -> std::io::Result<(TlsStream<TcpStream>, Accepted)> {
    let local = sock.local_addr()?;
    let stream = tls.accept(sock).await?;
    let conn = stream.get_ref().1;
    let accepted = Accepted { peer, local, sni: conn.server_name().map(str::to_ascii_lowercase), alpn: conn.alpn_protocol().map(<[u8]>::to_vec) };
    Ok((stream, accepted))
}

/// Accept forever: each connection gets its PROXY header read and its TLS handshake done, then
/// `handler` runs on it in its own task.
pub async fn serve_tls<F, Fut>(listener: TcpListener, tls: Arc<ServerConfig>, opts: ListenOptions, handler: F) -> std::io::Result<()>
where
    F: Fn(TlsStream<TcpStream>, Accepted) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let acceptor = TlsAcceptor::from(tls);
    serve_tcp(listener, opts, move |sock, peer| {
        let acceptor = acceptor.clone();
        let handler = handler.clone();
        async move {
            match tokio::time::timeout(opts.handshake_timeout, handshake(sock, peer, &acceptor)).await {
                Ok(Ok((stream, accepted))) => handler(stream, accepted).await,
                Ok(Err(e)) => debug!(%peer, error = %e, "edge: TLS handshake failed"),
                Err(_) => debug!(%peer, "edge: TLS handshake timed out"),
            }
        }
    })
    .await
}

/// Accept forever, read each connection's PROXY header (under the handshake deadline) and hand
/// the socket and the client's address to `handler` in its own task.
pub async fn serve_tcp<F, Fut>(listener: TcpListener, opts: ListenOptions, handler: F) -> std::io::Result<()>
where
    F: Fn(TcpStream, SocketAddr) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        let (mut sock, peer) = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                // Out of descriptors and the like: back off instead of spinning.
                warn!(error = %e, "edge: accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = sock.set_nodelay(true);
        let handler = handler.clone();
        tokio::spawn(async move {
            let client = match tokio::time::timeout(opts.handshake_timeout, client_addr(&mut sock, peer, opts.proxy_protocol)).await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => return debug!(%peer, error = %e, "edge: bad PROXY header"),
                Err(_) => return debug!(%peer, "edge: no PROXY header in time"),
            };
            handler(sock, client).await;
        });
    }
}
