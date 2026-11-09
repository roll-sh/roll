//! STOMP over WebSocket, like RabbitMQ's Web-STOMP plugin: WebSocket messages carry STOMP
//! frames (one or more per message), and the `v10.stomp`/`v11.stomp`/`v12.stomp`
//! subprotocols are negotiated.

use crate::Config;
use crate::conn::serve_connection;
use anyhow::Context as _;
use bytes::Bytes;
use futures_util::{Sink, Stream};
use mq_core::Broker;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes, http};
use tracing::{debug, info};

/// Accept STOMP-over-WebSocket connections on `config.ws_listen` forever.
pub async fn serve_ws(broker: Arc<Broker>, config: Config) -> anyhow::Result<()> {
    let addr = config.ws_listen.context("stomp ws_listen is not configured")?;
    let listener = TcpListener::bind(addr).await.with_context(|| format!("bind {addr}"))?;
    serve_ws_listener(broker, listener, config).await
}

/// Accept STOMP-over-WebSocket connections (HTTP upgrade on `config.ws_path`) on an already
/// bound listener.
#[allow(clippy::result_large_err)] // tungstenite's handshake callback signature
pub async fn serve_ws_listener(broker: Arc<Broker>, listener: TcpListener, config: Config) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    info!(%addr, path = %config.ws_path, "web-stomp listening");
    broker.listeners.write().push(mq_core::broker::Listener { protocol: "http/web-stomp".into(), port: addr.port(), ip_address: addr.ip().to_string() });
    let config = Arc::new(config);
    loop {
        let (socket, peer) = listener.accept().await?;
        let _ = socket.set_nodelay(true);
        let local = socket.local_addr().unwrap_or(addr);
        let (broker, config) = (broker.clone(), config.clone());
        tokio::spawn(async move {
            let path = config.ws_path.clone();
            let accept = tokio_tungstenite::accept_hdr_async(socket, move |req: &Request, resp: Response| handshake(&path, req, resp));
            let ws = match tokio::time::timeout(Duration::from_secs(10), accept).await {
                Ok(Ok(ws)) => ws,
                Ok(Err(e)) => return debug!(%peer, error = %e, "web-stomp handshake failed"),
                Err(_) => return debug!(%peer, "web-stomp handshake timed out"),
            };
            if let Err(e) = serve_connection(broker, WsIo { ws, rbuf: Bytes::new() }, peer, local, false, config).await {
                debug!(%peer, error = %e, "web-stomp connection ended");
            }
        });
    }
}

/// Check the path and pick the highest STOMP subprotocol the client offers.
#[allow(clippy::result_large_err)]
fn handshake(path: &str, req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
    if req.uri().path() != path {
        let mut e = ErrorResponse::new(Some("Not Found".into()));
        *e.status_mut() = http::StatusCode::NOT_FOUND;
        return Err(e);
    }
    let offered: Vec<String> = req
        .headers()
        .get_all(http::header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|p| p.trim().to_ascii_lowercase()))
        .collect();
    if let Some(p) = ["v12.stomp", "v11.stomp", "v10.stomp", "stomp"].into_iter().find(|p| offered.iter().any(|o| o == p))
        && let Ok(v) = http::HeaderValue::from_str(p)
    {
        resp.headers_mut().insert(http::header::SEC_WEBSOCKET_PROTOCOL, v);
    }
    Ok(resp)
}

/// A WebSocket as a byte stream: reads concatenate message payloads, each write is sent as
/// one message (text when it is valid UTF-8, like Web-STOMP's default).
struct WsIo<S> {
    ws: WebSocketStream<S>,
    rbuf: Bytes,
}

fn io_err(e: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(e)
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsIo<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if !this.rbuf.is_empty() {
                let n = this.rbuf.len().min(buf.remaining());
                buf.put_slice(&this.rbuf.split_to(n));
                return Poll::Ready(Ok(()));
            }
            match ready!(Pin::new(&mut this.ws).poll_next(cx)) {
                None | Some(Ok(Message::Close(_))) => return Poll::Ready(Ok(())),
                Some(Ok(m @ (Message::Text(_) | Message::Binary(_)))) => this.rbuf = m.into_data(),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Poll::Ready(Err(io_err(e))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsIo<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(Pin::new(&mut this.ws).poll_ready(cx)).map_err(io_err)?;
        let data = Bytes::copy_from_slice(buf);
        let msg = match Utf8Bytes::try_from(data.clone()) {
            Ok(t) => Message::Text(t),
            Err(_) => Message::Binary(data),
        };
        Pin::new(&mut this.ws).start_send(msg).map_err(io_err)?;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().ws).poll_flush(cx).map_err(io_err)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().ws).poll_close(cx).map_err(io_err)
    }
}
