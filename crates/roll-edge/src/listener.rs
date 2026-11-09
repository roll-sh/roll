//! The accept loop every public TCP listener shares: the PROXY header (when the NLB sends one),
//! then the TLS handshake, both under a deadline, then the product's handler. Each follows the
//! node's [`Drain`]: it stops accepting when the drain says so, and a product protocol's
//! connections still open when the edge closes are closed ([`crate::drain`] has the order).

use crate::drain::Drain;
use crate::proxy_protocol;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::server::TlsStream;
use tokio_rustls::{LazyConfigAcceptor, TlsAcceptor};
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
    /// The client presented a certificate the config's verifier accepted (only configs with a
    /// client verifier ask for one).
    pub client_verified: bool,
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
    let accepted = accepted(&stream, peer, local);
    Ok((stream, accepted))
}

fn accepted(stream: &TlsStream<TcpStream>, peer: SocketAddr, local: SocketAddr) -> Accepted {
    let conn = stream.get_ref().1;
    Accepted {
        peer,
        local,
        sni: conn.server_name().map(str::to_ascii_lowercase),
        alpn: conn.alpn_protocol().map(<[u8]>::to_vec),
        client_verified: conn.peer_certificates().is_some_and(|c| !c.is_empty()),
    }
}

/// Finish a TLS handshake with the config `pick` chooses for the ClientHello's server name.
pub async fn handshake_by_name<P>(sock: TcpStream, peer: SocketAddr, pick: &P) -> std::io::Result<(TlsStream<TcpStream>, Accepted)>
where
    P: Fn(Option<&str>) -> Arc<ServerConfig>,
{
    let local = sock.local_addr()?;
    let start = LazyConfigAcceptor::new(Default::default(), sock).await?;
    let config = pick(start.client_hello().server_name());
    let stream = start.into_stream(config).await?;
    let accepted = accepted(&stream, peer, local);
    Ok((stream, accepted))
}

/// Accept until the drain stops it: each connection gets its PROXY header read and its TLS
/// handshake done, then `handler` runs on it in its own task, until it ends or the edge closes
/// the connections left ([`Drain::close`]).
pub async fn serve_tls<F, Fut>(listener: TcpListener, tls: Arc<ServerConfig>, opts: ListenOptions, drain: Drain, handler: F) -> std::io::Result<()>
where
    F: Fn(TlsStream<TcpStream>, Accepted) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let acceptor = TlsAcceptor::from(tls);
    serve_tcp(listener, opts, drain.clone(), move |sock, peer| {
        let (acceptor, handler, drain) = (acceptor.clone(), handler.clone(), drain.clone());
        async move {
            match tokio::time::timeout(opts.handshake_timeout, handshake(sock, peer, &acceptor)).await {
                Ok(Ok((stream, accepted))) => {
                    if drain.until_closing(handler(stream, accepted)).await.is_none() {
                        debug!(%peer, "edge: connection closed by the drain");
                    }
                }
                Ok(Err(e)) => debug!(%peer, error = %e, "edge: TLS handshake failed"),
                Err(_) => debug!(%peer, "edge: TLS handshake timed out"),
            }
        }
    })
    .await
}

/// Accept until the drain stops it ([`Drain::stop_accepting`]; then the socket is closed, so
/// new connections are refused, and this never returns), read each connection's PROXY header
/// (under the handshake deadline) and hand the socket and the client's address to `handler` in
/// its own task. Connections already accepted are the handler's to end.
pub async fn serve_tcp<F, Fut>(listener: TcpListener, opts: ListenOptions, drain: Drain, handler: F) -> std::io::Result<()>
where
    F: Fn(TcpStream, SocketAddr) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let listening = drain.listening();
    let refusing = drain.refusing();
    tokio::pin!(refusing);
    loop {
        let accepted = tokio::select! {
            biased;
            _ = &mut refusing => break,
            r = listener.accept() => r,
        };
        let (mut sock, peer) = match accepted {
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
    let addr = listener.local_addr().ok();
    drop(listener);
    drop(listening);
    debug!(?addr, "edge: listener closed (draining)");
    // Ending would read as the listener failing: whoever runs it stops on its own schedule.
    std::future::pending().await
}
