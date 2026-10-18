//! MQTT over WebSocket (RabbitMQ's Web MQTT plugin): binary frames on `ws_path`, subprotocol
//! `mqtt`, bridged to the regular connection handler through an in-memory pipe.

use crate::{Config, Stream, serve_connection};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use mq_core::Broker;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::{self, Message};
use tracing::{debug, info};

/// Accept MQTT-over-WebSocket connections on an already bound listener.
pub async fn serve_ws_listener(broker: Arc<Broker>, listener: TcpListener, config: Config) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    info!(%addr, path = %config.ws_path, "web mqtt listening");
    broker.listeners.write().push(mq_core::broker::Listener { protocol: "http/web-mqtt".into(), port: addr.port(), ip_address: addr.ip().to_string() });
    let config = Arc::new(config);
    loop {
        let (socket, peer) = listener.accept().await?;
        let _ = socket.set_nodelay(true);
        let local = socket.local_addr().unwrap_or(addr);
        let (broker, config) = (broker.clone(), config.clone());
        tokio::spawn(async move {
            if let Err(e) = serve_ws_connection(broker, socket, peer, local, false, config).await {
                debug!(%peer, error = %e, "web mqtt connection ended");
            }
        });
    }
}

/// Complete the WebSocket handshake on `stream`, then serve MQTT over it.
pub async fn serve_ws_connection<S: Stream>(broker: Arc<Broker>, stream: S, peer: SocketAddr, local: SocketAddr, tls: bool, config: Arc<Config>) -> anyhow::Result<()> {
    let path = config.ws_path.clone();
    // The error type is fixed by tungstenite's handshake callback.
    #[allow(clippy::result_large_err)]
    let callback = move |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
        if req.uri().path() != path {
            let mut e = ErrorResponse::new(Some("not found".into()));
            *e.status_mut() = StatusCode::NOT_FOUND;
            return Err(e);
        }
        let offered = req.headers().get_all("sec-websocket-protocol").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).map(str::trim);
        let mut chosen = None;
        for p in offered {
            if p == "mqtt" || (p == "mqttv3.1" && chosen.is_none()) {
                chosen = Some(p.to_string());
            }
        }
        if let Some(p) = chosen.and_then(|p| HeaderValue::from_str(&p).ok()) {
            resp.headers_mut().insert("sec-websocket-protocol", p);
        }
        Ok(resp)
    };
    let ws = tokio::time::timeout(config.login_timeout, tokio_tungstenite::accept_hdr_async(stream, callback)).await??;
    let (mut tx, mut rx) = ws.split();
    let (ours, theirs) = tokio::io::duplex(64 * 1024);
    let (mut from_session, mut to_session) = tokio::io::split(ours);

    let up = tokio::spawn(async move {
        while let Some(m) = rx.next().await {
            match m? {
                Message::Binary(b) => to_session.write_all(&b).await?,
                Message::Close(_) => break,
                Message::Text(_) => anyhow::bail!("text frames are not MQTT"),
                _ => {}
            }
        }
        to_session.shutdown().await?;
        anyhow::Ok(())
    });
    let down = tokio::spawn(async move {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = from_session.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            tx.send(Message::Binary(Bytes::copy_from_slice(&buf[..n]))).await?;
        }
        match tx.close().await {
            Ok(()) | Err(tungstenite::Error::ConnectionClosed) => {}
            Err(e) => return Err(e.into()),
        }
        anyhow::Ok(())
    });
    let r = serve_connection(broker, theirs, peer, local, tls, config).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), down).await;
    up.abort();
    r
}
