//! Postgres' way into TLS: an `SSLRequest` answered with `S` and then a handshake, or a
//! handshake straight away (`sslnegotiation=direct`, PG 17+, which must negotiate ALPN
//! `postgresql`). `GSSENCRequest` is declined with `N`, as libpq expects. Startup packets in the
//! clear are refused: everything on the edge is TLS. Cancel requests may still come in the clear
//! (libpq before 17 sends them so); they carry only the key Postgres gave the session.

use crate::hosts::{Host, Hostnames};
use crate::listener::{self, Accepted, ListenOptions};
use crate::stream::Prefixed;
use bytes::{BufMut, BytesMut};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::server::TlsStream;
use tracing::debug;

pub const SSL_REQUEST: u32 = 80877103;
pub const GSSENC_REQUEST: u32 = 80877104;
pub const CANCEL_REQUEST: u32 = 80877102;
/// The ALPN protocol Postgres registered.
pub const ALPN: &[u8] = b"postgresql";

// Built once per connection and taken apart right away.
#[allow(clippy::large_enum_variant)]
pub enum Negotiated {
    /// TLS is up; the startup packet comes next, inside it.
    Tls(TlsStream<TcpStream>, Accepted),
    /// A CancelRequest in the clear, its bytes put back in front.
    Cancel(Prefixed<TcpStream>),
}

/// Take a fresh connection (its PROXY header already read) to TLS. `Ok(None)`: the client was
/// told why it can't go on, or left.
pub async fn negotiate(mut sock: TcpStream, peer: SocketAddr, tls: &TlsAcceptor) -> io::Result<Option<Negotiated>> {
    // At most one GSSENCRequest before the SSLRequest, as libpq does with gssencmode=prefer.
    for _ in 0..2 {
        let mut first = [0u8; 1];
        if sock.peek(&mut first).await? == 0 {
            return Ok(None);
        }
        if first[0] == 0x16 {
            let (stream, accepted) = listener::handshake(sock, peer, tls).await?;
            // As Postgres: a direct handshake without ALPN could be another protocol's client.
            if accepted.alpn.as_deref() != Some(ALPN) {
                return Ok(None);
            }
            return Ok(Some(Negotiated::Tls(stream, accepted)));
        }
        let mut head = [0u8; 8];
        sock.read_exact(&mut head).await?;
        let len = u32::from_be_bytes(head[..4].try_into().unwrap());
        let code = u32::from_be_bytes(head[4..].try_into().unwrap());
        match (len, code) {
            (8, SSL_REQUEST) => {
                sock.write_all(b"S").await?;
                let (stream, accepted) = listener::handshake(sock, peer, tls).await?;
                return Ok(Some(Negotiated::Tls(stream, accepted)));
            }
            (8, GSSENC_REQUEST) => sock.write_all(b"N").await?,
            (16, CANCEL_REQUEST) => return Ok(Some(Negotiated::Cancel(Prefixed::new(head.to_vec(), sock)))),
            _ => {
                let msg = "roll postgres requires TLS: connect with sslmode=require";
                let _ = sock.write_all(&error_response("08P01", msg)).await;
                return Ok(None);
            }
        }
    }
    Ok(None)
}

/// The Postgres listener: TLS either way in, then the proxy, with the endpoint the server name
/// names (`<id>.pg.roll.sh`); clients without SNI name it in `options=endpoint=<id>`.
pub async fn serve<R: pg_proxy::Router>(listener: TcpListener, tls: Arc<ServerConfig>, opts: ListenOptions, hosts: Hostnames, proxy: Arc<pg_proxy::Proxy<R>>) -> io::Result<()> {
    let acceptor = TlsAcceptor::from(tls);
    let hosts = Arc::new(hosts);
    listener::serve_tcp(listener, opts, move |sock, peer| {
        let (acceptor, hosts, proxy) = (acceptor.clone(), hosts.clone(), proxy.clone());
        async move {
            let result = match tokio::time::timeout(opts.handshake_timeout, negotiate(sock, peer, &acceptor)).await {
                Ok(Ok(Some(Negotiated::Tls(stream, accepted)))) => {
                    let named = accepted.sni.as_deref().and_then(|s| match hosts.classify(s) {
                        Host::Pg(id) => Some(id),
                        _ => None,
                    });
                    proxy.serve_connection(stream, named).await
                }
                Ok(Ok(Some(Negotiated::Cancel(stream)))) => proxy.serve_connection(stream, None).await,
                Ok(Ok(None)) => Ok(()),
                Ok(Err(e)) => Err(e.into()),
                Err(_) => Ok(()),
            };
            if let Err(e) = result {
                debug!(%peer, error = %e, "edge: postgres connection ended with an error");
            }
        }
    })
    .await
}

/// A FATAL ErrorResponse.
pub fn error_response(code: &str, message: &str) -> BytesMut {
    let mut fields = BytesMut::new();
    for (tag, value) in [(b'S', "FATAL"), (b'V', "FATAL"), (b'C', code), (b'M', message)] {
        fields.put_u8(tag);
        fields.put_slice(value.as_bytes());
        fields.put_u8(0);
    }
    fields.put_u8(0);
    let mut out = BytesMut::with_capacity(fields.len() + 5);
    out.put_u8(b'E');
    out.put_i32(fields.len() as i32 + 4);
    out.extend_from_slice(&fields);
    out
}
