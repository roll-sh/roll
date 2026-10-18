//! roll queues HTTP messaging API: publish, long-poll, SSE, WebSocket, webhooks.
//!
//! The API serverless and edge clients use when they can't hold an AMQP connection open.
//! Everything is versioned under `/v1`, speaks JSON and runs on the same engine as the AMQP
//! front end, so messages published or leased over HTTP interoperate with AMQP consumers on
//! the same queues (visibility timeouts return messages to them, rejects dead-letter, and
//! `x-delivery-count` keeps counting).
//!
//! - Auth: HTTP Basic with broker users (same permission checks as AMQP), or scoped bearer
//!   tokens (`/v1/tokens`).
//! - Publish: single or batch, waits for publisher confirms, `Idempotency-Key` replay and
//!   per-message id dedup.
//! - Pull: `receive` leases messages (long-poll) and hands out signed receipts for
//!   `ack`/`nack`/`visibility`.
//! - Push: Server-Sent Events, WebSocket and webhooks (signed, retried, dead-lettered).
//! - Topology: list, declare, purge and delete queues; declare and delete exchanges; bind.
//!
//! The user-facing reference is `docs/queues/http-api.md`.

mod auth;
mod error;
mod idempotency;
mod message;
mod publish;
mod pull;
mod push;
mod queues;
mod receipt;
mod tokens;
mod topology;
mod webhooks;

pub use error::ApiError;

use anyhow::Context;
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::routing::{get, post, put};
use mq_core::Broker;
use mq_core::queue::ChannelId;
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::watch;
use tower_http::cors::{AllowHeaders, Any, CorsLayer};
use tracing::{info, warn};

/// Metadata kinds owned by this crate (besides `token` and `webhook`).
pub(crate) mod kind {
    /// Crate-wide settings (the receipt signing key).
    pub const HTTP: &str = "http";
    /// Stored responses for `Idempotency-Key` replays.
    pub const IDEMPOTENCY: &str = "http_idempotency";
}

/// HTTP API settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    /// Public base URL (shown by `/v1/health`).
    pub public_url: Option<String>,
    /// Key that signs receipts. `None`: generated once and persisted in the metadata store, so
    /// every broker sharing it accepts the same receipts.
    pub receipt_secret: Option<Vec<u8>>,
    /// Most messages in one publish batch.
    pub max_batch: usize,
    /// Longest `receive` long-poll.
    pub max_wait_ms: u64,
    /// Visibility timeout when `receive` doesn't give one.
    pub default_visibility_ms: u64,
    /// Longest visibility timeout.
    pub max_visibility_ms: u64,
    /// How long an `Idempotency-Key` response is replayed.
    pub idempotency_ttl_ms: u64,
    /// Most stored `Idempotency-Key` responses.
    pub idempotency_max_entries: usize,
    /// Window in which a repeated per-message `id` is treated as a duplicate.
    pub dedup_window_ms: u64,
    /// SSE heartbeat comments and WebSocket pings.
    pub heartbeat: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: SocketAddr::from(([127, 0, 0, 1], 55680)),
            public_url: None,
            receipt_secret: None,
            max_batch: 500,
            max_wait_ms: 20_000,
            default_visibility_ms: 30_000,
            max_visibility_ms: 12 * 3600 * 1000,
            idempotency_ttl_ms: 24 * 3600 * 1000,
            idempotency_max_entries: 100_000,
            dedup_window_ms: 5 * 60 * 1000,
            heartbeat: Duration::from_secs(15),
        }
    }
}

/// The HTTP API: shared state behind the router, plus background work (webhooks, cleanup).
/// Background tasks stop when it is dropped or [`HttpApi::shutdown`] is called.
pub struct HttpApi {
    pub broker: Arc<Broker>,
    pub config: Config,
    receipt_key: Vec<u8>,
    pub(crate) tokens: tokens::TokenCache,
    pub(crate) idem: idempotency::IdemStore,
    pub(crate) dedup: publish::DedupWindow,
    pub(crate) sessions: RwLock<HashMap<ChannelId, Arc<push::Session>>>,
    pub(crate) webhooks: Mutex<HashMap<String, Arc<webhooks::Runner>>>,
    pub(crate) http: reqwest::Client,
    stop: watch::Sender<bool>,
}

impl std::fmt::Debug for HttpApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpApi").field("listen", &self.config.listen).finish()
    }
}

impl HttpApi {
    /// Load (or create) the receipt key, start the persisted webhooks and the cleanup and
    /// drain watchers.
    pub async fn start(broker: Arc<Broker>, config: Config) -> anyhow::Result<Arc<HttpApi>> {
        let receipt_key = match &config.receipt_secret {
            Some(k) if !k.is_empty() => k.clone(),
            _ => receipt::load_or_create_key(broker.meta.as_ref()).await?,
        };
        let http = reqwest::Client::builder()
            .user_agent(concat!("roll-webhooks/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("http client")?;
        let api = Arc::new(HttpApi {
            tokens: tokens::TokenCache::default(),
            idem: idempotency::IdemStore::new(broker.meta.clone(), config.idempotency_ttl_ms, config.idempotency_max_entries),
            dedup: publish::DedupWindow::default(),
            sessions: RwLock::default(),
            webhooks: Mutex::default(),
            http,
            stop: watch::channel(false).0,
            receipt_key,
            broker,
            config,
        });
        webhooks::restore(&api).await;
        tokio::spawn(follow_webhooks(Arc::downgrade(&api), api.broker.events.subscribe()));
        tokio::spawn(watch_drain(Arc::downgrade(&api)));
        tokio::spawn(idempotency::cleanup_loop(Arc::downgrade(&api)));
        Ok(api)
    }

    /// Stop background work and end streaming consumers (their messages are requeued).
    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }

    /// Whether the API is shutting down (explicitly, or because the broker is draining).
    pub fn is_stopping(&self) -> bool {
        *self.stop.borrow() || self.broker.draining.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn stop_rx(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }

    /// Resolves once [`HttpApi::shutdown`] is called.
    pub async fn stopped(&self) {
        push::stopped(&mut self.stop_rx()).await;
    }
}

impl Drop for HttpApi {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

/// Webhooks created, changed or deleted on other nodes (the cluster change feed).
async fn follow_webhooks(api: Weak<HttpApi>, mut events: tokio::sync::broadcast::Receiver<mq_core::broker::Event>) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        let event = events.recv().await;
        let Some(api) = api.upgrade() else { return };
        if api.is_stopping() {
            return;
        }
        match event {
            Ok(mq_core::broker::Event::MetaChanged { kind, key }) if kind == mq_core::meta::kind::WEBHOOK => webhooks::sync(&api, &key).await,
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => webhooks::sync_all(&api).await,
            Err(RecvError::Closed) => return,
        }
    }
}

/// The broker draining (shutdown, rolling upgrade) ends long-polls, streams and webhooks.
async fn watch_drain(api: Weak<HttpApi>) {
    let Some((broker, mut stop)) = api.upgrade().map(|a| (a.broker.clone(), a.stop_rx())) else { return };
    loop {
        let started = broker.drain_started.notified();
        tokio::pin!(started);
        started.as_mut().enable();
        if broker.draining.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(api) = api.upgrade() {
                info!("broker draining: stopping http consumers and webhooks");
                api.shutdown();
            }
            return;
        }
        tokio::select! {
            _ = started => {}
            _ = stop.wait_for(|s| *s) => return,
        }
    }
}

/// The `/v1` routes with state applied (CORS included).
pub fn router(api: Arc<HttpApi>) -> Router {
    let body_limit = (api.broker.config.max_message_size as usize).saturating_mul(2).max(32 << 20);
    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(AllowHeaders::mirror_request()).expose_headers(Any).max_age(Duration::from_secs(3600));
    let vq = "/v1/vhosts/{vhost}/queues/{queue}";
    Router::new()
        .route("/v1/health", get(queues::health))
        .route("/v1/tokens", post(tokens::create).get(tokens::list))
        .route("/v1/tokens/{id}", get(tokens::get_one).delete(tokens::delete))
        .route("/v1/vhosts/{vhost}/exchanges/{exchange}/publish", post(publish::to_exchange))
        .route("/v1/vhosts/{vhost}/exchanges/{exchange}", put(topology::declare).delete(topology::delete))
        .route("/v1/vhosts/{vhost}/bindings", post(topology::bind).delete(topology::unbind))
        .route("/v1/vhosts/{vhost}/queues", get(queues::list))
        .route(vq, put(queues::declare).get(queues::get_queue).delete(queues::delete))
        .route(&format!("{vq}/purge"), post(queues::purge))
        .route(&format!("{vq}/messages"), post(publish::to_queue))
        .route(&format!("{vq}/receive"), post(pull::receive))
        .route(&format!("{vq}/ack"), post(pull::ack))
        .route(&format!("{vq}/nack"), post(pull::nack))
        .route(&format!("{vq}/visibility"), post(pull::visibility))
        .route(&format!("{vq}/events"), get(push::events))
        .route(&format!("{vq}/ws"), get(push::websocket))
        .route("/v1/vhosts/{vhost}/webhooks", get(webhooks::list))
        .route("/v1/vhosts/{vhost}/webhooks/{name}", put(webhooks::put).get(webhooks::get_one).delete(webhooks::delete))
        .fallback(|| async { ApiError::not_found("no such endpoint") })
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(axum::middleware::from_fn_with_state(api.clone(), awake))
        .layer(cors)
        .with_state(api)
}

/// Requests keep the broker out of quiet mode while they run (long polls included) and for a
/// little while after (see [`mq_core::broker::QUIET_AFTER_MS`]).
async fn awake(State(api): State<Arc<HttpApi>>, req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let _awake = api.broker.awake_guard();
    api.broker.note_request();
    next.run(req).await
}

/// Bind `config.listen` and serve until [`HttpApi::shutdown`].
pub async fn serve(broker: Arc<Broker>, config: Config) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(config.listen).await.with_context(|| format!("bind {}", config.listen))?;
    serve_listener(broker, listener, config).await
}

/// Serve on an already bound listener.
pub async fn serve_listener(broker: Arc<Broker>, listener: tokio::net::TcpListener, config: Config) -> anyhow::Result<()> {
    let api = HttpApi::start(broker, config).await?;
    serve_api(api, listener).await
}

/// Serve a started [`HttpApi`] on a bound listener until it shuts down.
pub async fn serve_api(api: Arc<HttpApi>, listener: tokio::net::TcpListener) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    info!(%addr, "http messaging api listening");
    api.broker.listeners.write().push(mq_core::broker::Listener { protocol: "http".into(), port: addr.port(), ip_address: addr.ip().to_string() });
    let mut stop = api.stop_rx();
    let app = router(api).into_make_service_with_connect_info::<SocketAddr>();
    let r = axum::serve(listener, app).with_graceful_shutdown(async move { push::stopped(&mut stop).await }).await;
    if let Err(e) = &r {
        warn!(error = %e, "http messaging api stopped");
    }
    r.map_err(Into::into)
}
