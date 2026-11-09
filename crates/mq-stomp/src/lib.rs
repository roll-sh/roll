//! STOMP 1.0, 1.1 and 1.2 server for roll queues, compatible with RabbitMQ's STOMP plugin:
//! the same destinations (`/exchange`, `/queue`, `/amq/queue`, `/topic`, `/temp-queue`,
//! `/reply-queue`), header ↔ AMQP property mapping, ack modes, transactions, receipts (sent
//! only once a message is durable) and error texts, so unmodified STOMP clients work by
//! changing only the connection URL. STOMP over WebSocket is served by [`serve_ws`].
//!
//! Each connection runs one session task (frames in order, like RabbitMQ's single channel per
//! STOMP connection) and a writer task that coalesces frames and sends heart-beats.

mod conn;
mod dest;
mod frame;
mod props;
mod ws;

pub use conn::{serve, serve_connection, serve_listener};
pub use ws::{serve_ws, serve_ws_listener};

use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncWrite};

/// Any bidirectional byte stream (TCP, TLS, a proxy's socket, a WebSocket adapter).
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

/// STOMP listener settings. The defaults match RabbitMQ's `rabbitmq_stomp` application.
#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    /// STOMP over WebSocket listener for [`serve_ws`] (RabbitMQ Web-STOMP uses port 15674).
    pub ws_listen: Option<SocketAddr>,
    /// WebSocket endpoint path.
    pub ws_path: String,
    /// Vhost used when CONNECT has no `host` header (`None`: the broker's default vhost).
    pub default_vhost: Option<String>,
    /// Credentials used when CONNECT has no `login` header (RabbitMQ's `stomp.default_user`).
    pub default_login: Option<(String, String)>,
    /// Accept frames before CONNECT, logging in with the default credentials.
    pub implicit_connect: bool,
    /// Largest frame accepted once authenticated (RabbitMQ: 4 MiB; 64 KiB before login).
    pub max_frame_size: usize,
    pub max_headers: usize,
    pub max_header_length: usize,
    /// Exchange behind `/topic/` destinations.
    pub default_topic_exchange: String,
    /// Whether NACK requeues when the frame has no `requeue` header.
    pub default_nack_requeue: bool,
    /// Send an EOL after each frame's NUL, like RabbitMQ.
    pub trailing_lf: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: SocketAddr::from(([127, 0, 0, 1], 56613)),
            ws_listen: None,
            ws_path: "/ws".into(),
            default_vhost: None,
            default_login: Some(("guest".into(), "guest".into())),
            implicit_connect: false,
            max_frame_size: 4 << 20,
            max_headers: 100,
            max_header_length: 100 * 1024,
            default_topic_exchange: "amq.topic".into(),
            default_nack_requeue: true,
            trailing_lf: true,
        }
    }
}
