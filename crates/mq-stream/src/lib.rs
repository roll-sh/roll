//! roll queues RabbitMQ Stream protocol server (RabbitMQ's port 5552).
//!
//! Streams are the broker's `x-queue-type: stream` queues, so the stream protocol, AMQP 0-9-1
//! and AMQP 1.0 read and write the same logs. Messages published here are stored as AMQP 1.0
//! (as RabbitMQ does) next to a pre-converted 0-9-1 view; deliveries are whole osiris chunks.
//!
//! ```no_run
//! # async fn f(broker: std::sync::Arc<mq_core::broker::Broker>) -> anyhow::Result<()> {
//! let config = mq_stream::Config::new("0.0.0.0:5552".parse()?);
//! mq_stream::serve(broker, config).await
//! # }
//! ```

mod amqp10;
mod conn;
mod manage;
mod proto;
mod sac;
mod store;

use anyhow::Context;
use mq_core::broker::{Broker, Event};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::net::TcpListener;
use tracing::{debug, info};

/// Stream listener settings.
#[derive(Clone, Debug)]
pub struct Config {
    /// Address to bind (`serve`) or that the listener was bound to (reported as the listener).
    pub listen: SocketAddr,
    /// Host clients are told to connect to (`Open` and `Metadata`). Empty: `localhost`.
    pub advertised_host: String,
    /// Port clients are told to connect to. 0: the listener's port.
    pub advertised_port: u16,
    /// Largest frame the server accepts and proposes in `Tune` (RabbitMQ: 1 MiB).
    pub frame_max: u32,
    /// Heartbeat proposed in `Tune`, seconds (RabbitMQ: 60).
    pub heartbeat: u32,
    /// Unconfirmed messages per connection before reading publishes pauses.
    pub max_unconfirmed: usize,
}

impl Config {
    /// RabbitMQ's defaults on `listen`, advertising `localhost`.
    pub fn new(listen: SocketAddr) -> Self {
        Config { listen, advertised_host: String::new(), advertised_port: 0, frame_max: 1 << 20, heartbeat: 60, max_unconfirmed: 100_000 }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config::new(SocketAddr::from(([0, 0, 0, 0], 5552)))
    }
}

/// Listener-wide state shared by connections.
struct Server {
    broker: Arc<Broker>,
    config: Config,
    sac: sac::Coordinator,
    store: Arc<store::Store>,
}

/// Bind `config.listen` and serve stream connections forever.
pub async fn serve(broker: Arc<Broker>, config: Config) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.listen).await.with_context(|| format!("bind {}", config.listen))?;
    serve_listener(broker, listener, config).await
}

/// Serve stream connections on an already bound listener forever.
pub async fn serve_listener(broker: Arc<Broker>, listener: TcpListener, mut config: Config) -> anyhow::Result<()> {
    let port = listener.local_addr()?.port();
    if config.advertised_port == 0 {
        config.advertised_port = port;
    }
    if config.advertised_host.is_empty() {
        config.advertised_host = "localhost".into();
    }
    info!(addr = %config.listen, port, "stream listening");
    broker.listeners.write().push(mq_core::broker::Listener { protocol: "stream".into(), port, ip_address: config.listen.ip().to_string() });
    let store = store::Store::open(broker.meta.clone()).await;
    let server = Arc::new(Server { broker: broker.clone(), config, sac: sac::Coordinator::default(), store: store.clone() });

    // Stored offsets and publisher sequences go with their stream.
    let mut events = broker.events.subscribe();
    let weak = Arc::downgrade(&store);
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(Event::QueueDeleted { vhost, name }) => match weak.upgrade() {
                    Some(store) => store.stream_deleted(&vhost, &name).await,
                    None => return,
                },
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            }
        }
    });

    loop {
        let (sock, peer) = listener.accept().await?;
        if broker.draining.load(Ordering::Relaxed) {
            continue;
        }
        let _ = sock.set_nodelay(true);
        let local = sock.local_addr().unwrap_or(server.config.listen);
        let server = server.clone();
        tokio::spawn(async move {
            if let Err(e) = conn::run(server, sock, peer, local).await {
                debug!(%peer, error = %e, "stream connection ended");
            }
        });
    }
}
