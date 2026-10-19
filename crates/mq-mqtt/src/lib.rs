//! roll queues MQTT 3.1, 3.1.1 and 5.0 server, compatible with RabbitMQ 4's MQTT plugin.
//!
//! MQTT is a front end to the same engine as AMQP: PUBLISHes go to a topic exchange
//! (`amq.topic`, topic `/` ↔ routing key `.`, `+` ↔ `*`), and each session subscribes through
//! its own queues `mqtt-subscription-<client id>qos0` and `…qos1`, bound with the subscription's
//! topic filters. AMQP and MQTT clients therefore interoperate in both directions, sessions
//! survive reconnects (and broker restarts) as durable queues with `x-expires`, and QoS 1
//! PUBACKs are only sent once the message is durable in every queue it was routed to.
//! Retained messages live in the metadata store (kind [`RETAINED_KIND`]).

mod convert;
pub mod packet;
mod session;
mod state;
pub mod topic;
mod ws;

pub use session::{queue_name, will_queue_name};
pub use state::RETAINED_KIND;
pub use ws::{serve_ws_connection, serve_ws_listener};

use anyhow::Context;
use mq_core::Broker;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tracing::{debug, info};

/// Any bidirectional byte stream (TCP, TLS, a proxy's socket, a WebSocket bridge).
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

/// MQTT listener settings. Defaults follow RabbitMQ's `rabbitmq_mqtt` plugin, except that
/// anonymous connections are off unless enabled.
#[derive(Clone, Debug)]
pub struct Config {
    /// MQTT over TCP.
    pub listen: SocketAddr,
    /// MQTT over WebSocket (`ws://host:port{ws_path}`, subprotocol `mqtt`), if set.
    pub ws_listen: Option<SocketAddr>,
    pub ws_path: String,
    /// Topic exchange MQTT publishes to and subscribes on (`mqtt.exchange`).
    pub exchange: String,
    /// Vhost for usernames without a `vhost:` prefix (`mqtt.vhost`); `None` = broker default.
    pub default_vhost: Option<String>,
    /// Connections without credentials log in as the broker's default user
    /// (`mqtt.allow_anonymous`).
    pub allow_anonymous: bool,
    /// Treat `:` in usernames literally instead of as `vhost:user`.
    pub ignore_colons_in_username: bool,
    /// Upper bound for session expiry; also the expiry of v3 persistent sessions
    /// (`mqtt.max_session_expiry_interval_seconds`). `u32::MAX` = never.
    pub max_session_expiry_interval_secs: u32,
    /// QoS 1 messages in flight to a client (`mqtt.prefetch`), capped by v5 Receive Maximum.
    pub prefetch: u16,
    pub max_packet_size_unauthenticated: u32,
    pub max_packet_size_authenticated: u32,
    /// Topic aliases accepted from and sent to v5 clients.
    pub topic_alias_maximum: u16,
    /// Close the connection when a publish or subscribe is not authorised
    /// (`mqtt.disconnect_on_unauthorized`).
    pub disconnect_on_unauthorized: bool,
    /// Sparkplug B topic mapping (`mqtt.sparkplug`).
    pub sparkplug: bool,
    /// `x-queue-type` of persistent QoS 1 session queues (`mqtt.durable_queue_type`);
    /// `None` uses the vhost default.
    pub durable_queue_type: Option<String>,
    /// Time allowed for the CONNECT packet.
    pub login_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: SocketAddr::from(([127, 0, 0, 1], 51883)),
            ws_listen: None,
            ws_path: "/ws".into(),
            exchange: "amq.topic".into(),
            default_vhost: None,
            allow_anonymous: false,
            ignore_colons_in_username: false,
            max_session_expiry_interval_secs: 86_400,
            prefetch: 10,
            max_packet_size_unauthenticated: 65_536,
            max_packet_size_authenticated: 16 << 20,
            topic_alias_maximum: 16,
            disconnect_on_unauthorized: true,
            sparkplug: false,
            durable_queue_type: None,
            login_timeout: Duration::from_secs(10),
        }
    }
}

/// Accept MQTT connections forever (and MQTT over WebSocket if `ws_listen` is set).
pub async fn serve(broker: Arc<Broker>, config: Config) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.listen).await.with_context(|| format!("bind {}", config.listen))?;
    match config.ws_listen {
        None => serve_listener(broker, listener, config).await,
        Some(addr) => {
            let ws = TcpListener::bind(addr).await.with_context(|| format!("bind {addr}"))?;
            tokio::try_join!(serve_listener(broker.clone(), listener, config.clone()), serve_ws_listener(broker, ws, config)).map(|_| ())
        }
    }
}

/// Accept MQTT connections on an already bound listener.
pub async fn serve_listener(broker: Arc<Broker>, listener: TcpListener, config: Config) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    info!(%addr, "mqtt listening");
    broker.listeners.write().push(mq_core::broker::Listener { protocol: "mqtt".into(), port: addr.port(), ip_address: addr.ip().to_string() });
    let config = Arc::new(config);
    loop {
        let (socket, peer) = listener.accept().await?;
        let _ = socket.set_nodelay(true);
        let local = socket.local_addr().unwrap_or(addr);
        let (broker, config) = (broker.clone(), config.clone());
        tokio::spawn(async move {
            if let Err(e) = serve_connection(broker, socket, peer, local, false, config).await {
                debug!(%peer, error = %e, "mqtt connection ended");
            }
        });
    }
}

/// Serve one MQTT connection whose transport is already established (TCP, TLS, proxied).
pub async fn serve_connection<S: Stream>(broker: Arc<Broker>, stream: S, peer: SocketAddr, local: SocketAddr, tls: bool, config: Arc<Config>) -> anyhow::Result<()> {
    session::run(broker, stream, peer, local, tls, config).await
}
