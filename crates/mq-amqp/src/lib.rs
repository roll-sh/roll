//! AMQP 0-9-1 server for roll queues, wire-compatible with RabbitMQ so unmodified clients
//! (pika, amqplib, amqp091-go, the Java/.NET clients, Bunny, lapin, …) work by changing only
//! the connection URL.
//!
//! Each connection runs a reader (frames → channel commands), a writer (coalesced socket
//! writes, heartbeats) and one task per channel, so a channel blocked on a busy queue never
//! stalls the others.

mod channel;
mod conn;
pub mod method;
pub mod reply_to;

pub use conn::{Handoff, ServerConfig, serve, serve_connection, serve_listener};

use tokio::io::{AsyncRead, AsyncWrite};

/// Any bidirectional byte stream (TCP, TLS, a proxy's socket).
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}
