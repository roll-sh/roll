//! roll queues RabbitMQ-compatible management HTTP API, definitions and Prometheus metrics.
//!
//! The API follows RabbitMQ 4's management plugin (paths, JSON shapes, status codes and error
//! bodies) closely enough that `rabbitmqadmin`, the Terraform provider, `rabbit-hole`,
//! `pyrabbit2` and monitoring tools work unmodified. Prometheus metrics are served on the same
//! router under `/metrics` with the `rabbitmq_prometheus` plugin's names and labels.

#![recursion_limit = "256"]

mod api;
mod format;
mod http;
mod metrics;
mod stats;
mod sys;

use axum::Router;
use axum::response::Html;
use axum::routing::{any, get};
use mq_core::Broker;
use mq_core::broker::Listener;
use parking_lot::RwLock;
use serde_json::Value;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{info, warn};

/// Returns live status objects (shovels or federation links) in RabbitMQ's JSON shape.
pub type StatusProvider = Arc<dyn Fn() -> Vec<Value> + Send + Sync>;

/// Restarts a dynamic shovel (`vhost`, `name`); false if there is no such shovel.
pub type RestartHook = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    /// Linked from `GET /` (the roll dashboard).
    pub dashboard_url: Option<String>,
    /// How often message rates are sampled (RabbitMQ uses 5 s).
    pub sample_interval: Duration,
    /// Directory whose free space is reported as `disk_free` (the broker's data directory).
    pub data_dir: PathBuf,
    /// Largest accepted request body (definitions imports can be big).
    pub max_body_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: SocketAddr::from(([127, 0, 0, 1], 55673)),
            dashboard_url: None,
            sample_interval: Duration::from_secs(5),
            data_dir: PathBuf::from("."),
            max_body_bytes: 64 << 20,
        }
    }
}

/// Management API state: the broker, the rate sampler and pluggable status providers.
pub struct Mgmt {
    pub broker: Arc<Broker>,
    pub config: Config,
    pub(crate) stats: stats::Stats,
    shovels: RwLock<Option<StatusProvider>>,
    shovel_restart: RwLock<Option<RestartHook>>,
    federation: RwLock<Option<StatusProvider>>,
    /// Unix ms of the last API or metrics request; the sampler idles without them.
    last_used_ms: AtomicU64,
}

/// Without requests for this long the sampler stops tracking rates (RabbitMQ keeps 10 minutes
/// of history, so nothing it would show is lost).
const IDLE_AFTER_MS: u64 = 600_000;

impl Mgmt {
    /// Create the state and, when called inside a Tokio runtime, start the rate sampler.
    pub fn new(broker: Arc<Broker>, config: Config) -> Arc<Self> {
        let m = Arc::new(Mgmt {
            stats: stats::Stats::new(config.sample_interval),
            broker,
            config,
            shovels: RwLock::new(None),
            shovel_restart: RwLock::new(None),
            federation: RwLock::new(None),
            last_used_ms: AtomicU64::new(0),
        });
        m.sample();
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(run_sampler(Arc::downgrade(&m)));
        }
        m
    }

    /// Serve `/api/shovels` from this provider (`[]` until one is set).
    pub fn set_shovel_status(&self, f: StatusProvider) {
        *self.shovels.write() = Some(f);
    }

    /// Handle `DELETE /api/shovels/vhost/{vhost}/{name}/restart` with this hook (404 until one
    /// is set).
    pub fn set_shovel_restart(&self, f: RestartHook) {
        *self.shovel_restart.write() = Some(f);
    }

    /// Serve `/api/federation-links` from this provider (`[]` until one is set).
    pub fn set_federation_status(&self, f: StatusProvider) {
        *self.federation.write() = Some(f);
    }

    /// Take a rate sample now (the sampler does this every `sample_interval`).
    pub fn sample(&self) {
        self.stats.sample(&self.broker);
    }

    /// Note an API or metrics request. The first one after an idle spell samples right away, so
    /// rates are back one interval later.
    pub(crate) fn touch(&self) {
        self.broker.note_request();
        let now = mq_core::now_ms();
        let prev = self.last_used_ms.swap(now, Ordering::Relaxed);
        if now.saturating_sub(prev) >= IDLE_AFTER_MS {
            self.stats.resume(&self.broker);
        }
    }

    fn in_use(&self) -> bool {
        mq_core::now_ms().saturating_sub(self.last_used_ms.load(Ordering::Relaxed)) < IDLE_AFTER_MS
    }

    pub(crate) fn shovel_status(&self) -> Vec<Value> {
        let f = self.shovels.read().clone();
        f.map(|f| f()).unwrap_or_default()
    }

    /// Restart a shovel through the hook; false when there is no hook or no such shovel.
    pub(crate) fn restart_shovel(&self, vhost: &str, name: &str) -> bool {
        let f = self.shovel_restart.read().clone();
        f.is_some_and(|f| f(vhost, name))
    }

    pub(crate) fn federation_status(&self) -> Vec<Value> {
        let f = self.federation.read().clone();
        f.map(|f| f()).unwrap_or_default()
    }

    pub(crate) fn node(&self) -> &str {
        &self.broker.config.node_name
    }
}

async fn run_sampler(m: Weak<Mgmt>) {
    let (interval, broker) = match m.upgrade() {
        Some(m) => (m.config.sample_interval, Arc::downgrade(&m.broker)),
        None => return,
    };
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        {
            let Some(m) = m.upgrade() else { return };
            if m.in_use() {
                m.sample();
            } else {
                m.stats.sample_vhosts(&m.broker);
            }
        }
        // Counters can't move while the broker is quiet: the sample above has the last of them.
        if !Broker::wait_while_quiet(&broker).await {
            return;
        }
    }
}

/// The management API and metrics router, state applied.
pub fn router(mgmt: Arc<Mgmt>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/metrics", get(metrics::aggregated))
        .route("/metrics/per-object", get(metrics::per_object))
        .route("/metrics/detailed", get(metrics::detailed))
        .route("/api", any(api::handle))
        .route("/api/", any(api::handle))
        .route("/api/{*rest}", any(api::handle))
        .with_state(mgmt)
}

/// Bind `config.listen` and serve until the listener fails.
/// Watch memory and free disk like RabbitMQ's alarms: above the memory high watermark (60% of
/// the memory available, container limits included) or below 50 MB of free disk, publishers are
/// blocked (`connection.blocked`) until it clears. The memory alarm needs Linux, where current
/// RSS is known.
pub fn watch_alarms(broker: Arc<Broker>, data_dir: std::path::PathBuf) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(&broker);
    drop(broker);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // Nothing publishes while the broker is quiet; the first client brings a check.
            if !Broker::wait_while_quiet(&weak).await {
                return;
            }
            tick.tick().await;
            let Some(broker) = weak.upgrade() else { return };
            if cfg!(target_os = "linux") {
                let (used, limit) = (sys::rss_bytes(), format::mem_limit());
                if limit > 0 {
                    let was = broker.mem_alarm.load(std::sync::atomic::Ordering::Acquire);
                    // A little hysteresis so a process sitting at the watermark doesn't flap.
                    let on = if was { used as f64 > limit as f64 * 0.95 } else { used > limit };
                    if on != was {
                        if on {
                            warn!(used, limit, "memory alarm: blocking publishers");
                        } else {
                            info!(used, limit, "memory alarm cleared");
                        }
                        broker.set_alarm(false, on);
                    }
                }
            }
            let dir = data_dir.clone();
            let free = tokio::task::spawn_blocking(move || sys::disk_free(&dir)).await.unwrap_or(0);
            if free > 0 {
                let was = broker.disk_alarm.load(std::sync::atomic::Ordering::Acquire);
                let on = if was { free < format::DISK_FREE_LIMIT * 2 } else { free < format::DISK_FREE_LIMIT };
                if on != was {
                    if on {
                        warn!(free, limit = format::DISK_FREE_LIMIT, "disk alarm: blocking publishers");
                    } else {
                        info!(free, "disk alarm cleared");
                    }
                    broker.set_alarm(true, on);
                }
            }
        }
    })
}

pub async fn serve(broker: Arc<Broker>, config: Config) -> anyhow::Result<()> {
    let listener = TcpListener::bind(config.listen).await.map_err(|e| anyhow::anyhow!("bind {}: {e}", config.listen))?;
    serve_listener(broker, listener, config).await
}

/// Serve on an already bound listener.
pub async fn serve_listener(broker: Arc<Broker>, listener: TcpListener, config: Config) -> anyhow::Result<()> {
    serve_mgmt(Mgmt::new(broker, config), listener).await
}

/// Serve existing state (lets the caller install status providers first).
pub async fn serve_mgmt(mgmt: Arc<Mgmt>, listener: TcpListener) -> anyhow::Result<()> {
    let addr = listener.local_addr()?;
    info!(%addr, "management api listening");
    {
        let mut l = mgmt.broker.listeners.write();
        for protocol in ["http", "http/prometheus"] {
            l.push(Listener { protocol: protocol.into(), port: addr.port(), ip_address: mgmt.config.listen.ip().to_string() });
        }
    }
    let app = router(mgmt).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index(axum::extract::State(m): axum::extract::State<Arc<Mgmt>>) -> Html<String> {
    let dash = match &m.config.dashboard_url {
        Some(u) => format!(r#"<p>Manage queues, rates and costs in the <a href="{0}">roll dashboard</a>.</p>"#, http::html_escape(u)),
        None => "<p>Manage queues, rates and costs in the roll dashboard.</p>".to_string(),
    };
    Html(format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>roll queues</title>
<style>
:root{{color-scheme:dark light;--bg:#000;--fg:#ededed;--muted:#8f8f8f;--line:#1f1f1f;--accent:#4c8dff}}
@media (prefers-color-scheme:light){{:root{{--bg:#fff;--fg:#111;--muted:#666;--line:#eaeaea}}}}
body{{margin:0;background:var(--bg);color:var(--fg);font:14px/1.6 "Geist",ui-sans-serif,system-ui,sans-serif}}
main{{max-width:560px;margin:15vh auto;padding:0 24px}}
h1{{font-size:28px;font-weight:600;letter-spacing:-0.04em;margin:0 0 8px}}
p{{color:var(--muted)}} a{{color:var(--accent);text-decoration:none}}
ul{{list-style:none;padding:0;border-top:1px solid var(--line)}} li{{padding:10px 0;border-bottom:1px solid var(--line)}}
code{{font-family:"Geist Mono",ui-monospace,monospace;font-size:13px}}
</style></head>
<body><main>
<h1>roll queues</h1>
<p>RabbitMQ-compatible management API for node <code>{node}</code>.</p>
{dash}
<ul>
<li><a href="api/overview"><code>/api/overview</code></a></li>
<li><a href="api/queues"><code>/api/queues</code></a></li>
<li><a href="metrics"><code>/metrics</code></a></li>
</ul>
</main></body></html>
"#,
        node = http::html_escape(m.node()),
    ))
}
