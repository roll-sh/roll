//! roll queues control plane: the roll API the dashboard and CLI use (`/v1/queues/...`),
//! live rates, metering and cost estimates, and scale-to-zero of idle vhosts.

pub mod api;
pub mod sampler;
pub mod usage;

use mq_core::Broker;
use parking_lot::RwLock;
use sampler::Sampler;
use serde_json::Value;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tracing::{debug, warn};
use usage::Meter;

/// Addresses clients use, shown in the dashboard and written into connection strings.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Endpoints {
    pub amqp: Option<String>,
    pub amqps: Option<String>,
    pub mqtt: Option<String>,
    pub stomp: Option<String>,
    pub stream: Option<String>,
    pub http: Option<String>,
    pub management: Option<String>,
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
/// Starts a migration from a RabbitMQ management API (`POST /v1/queues/migrate` body).
pub type MigrateFn = Arc<dyn Fn(Value) -> BoxFuture<anyhow::Result<Value>> + Send + Sync>;
/// Status of running migrations (`GET /v1/queues/migrate`).
pub type StatusFn = Arc<dyn Fn() -> Value + Send + Sync>;
/// Stops a migration: removes its shovels and the source credentials they hold
/// (`DELETE /v1/queues/migrate`).
pub type StopFn = Arc<dyn Fn() -> BoxFuture<anyhow::Result<Value>> + Send + Sync>;

/// Live migration, provided by `mq-shovel` (see [`QueuesControl::set_migrator`]).
#[derive(Clone)]
pub struct Migrator {
    pub start: MigrateFn,
    pub status: StatusFn,
    pub stop: StopFn,
}

#[derive(Clone, Debug, Default)]
pub struct ControlConfig {
    /// Where metering data lives (`None`: in memory).
    pub usage_db: Option<PathBuf>,
    /// Unload vhosts idle for this long; 0 disables scale-to-zero.
    pub idle_suspend_ms: u64,
    /// Shown in the overview.
    pub storage_backend: String,
    pub metadata_backend: String,
}

pub struct QueuesControl {
    pub broker: Arc<Broker>,
    pub endpoints: Endpoints,
    pub config: ControlConfig,
    pub sampler: Arc<Sampler>,
    pub meter: Meter,
    migrate: RwLock<Option<Migrator>>,
}

impl QueuesControl {
    pub fn new(broker: Arc<Broker>, endpoints: Endpoints) -> Arc<Self> {
        Self::with_config(broker, endpoints, ControlConfig::default()).expect("in-memory meter")
    }

    pub fn with_config(broker: Arc<Broker>, endpoints: Endpoints, config: ControlConfig) -> anyhow::Result<Arc<Self>> {
        let meter = Meter::open(config.usage_db.as_deref())?;
        Ok(Arc::new(QueuesControl { broker, endpoints, config, sampler: Sampler::new(), meter, migrate: RwLock::new(None) }))
    }

    /// Plug in live migration (provided by `mq-shovel`).
    pub fn set_migrator(&self, m: Migrator) {
        *self.migrate.write() = Some(m);
    }

    pub(crate) fn migrator(&self) -> Option<Migrator> {
        self.migrate.read().clone()
    }

    /// Run the sampler, metering and the idle-vhost reaper until the broker stops.
    pub fn spawn_background(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let c = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(sampler::STEP_S));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last = std::time::Instant::now();
            let mut n = 0u64;
            loop {
                tick.tick().await;
                let dt = last.elapsed().as_secs_f64();
                last = std::time::Instant::now();
                let delta = c.sampler.sample(&c.broker);
                if n > 0 {
                    c.meter.record(mq_core::now_ms() / 1000, dt, &delta);
                }
                n += 1;
                if n.is_multiple_of(12) {
                    if let Err(e) = c.meter.flush() {
                        warn!(error = %e, "metering flush failed");
                    }
                    c.reap_idle().await;
                }
                // Quiet: nothing moves until a client comes, so park. Stored bytes don't change
                // meanwhile and the next sample meters the whole gap. Loaded vhosts still go to
                // sleep on time: wake once for the next one due.
                if c.broker.is_quiet() {
                    if let Err(e) = c.meter.flush() {
                        warn!(error = %e, "metering flush failed");
                    }
                    let weak = Arc::downgrade(&c.broker);
                    let client = Broker::wait_while_quiet(&weak);
                    match c.next_suspend_in() {
                        None => {
                            client.await;
                        }
                        Some(due) => tokio::select! {
                            _ = client => {}
                            _ = tokio::time::sleep(due) => c.reap_idle().await,
                        },
                    }
                }
            }
        })
    }

    /// How long until the next loaded vhost is idle long enough to be put to sleep (`None`: no
    /// vhost is loaded, or vhosts never sleep).
    fn next_suspend_in(&self) -> Option<Duration> {
        let idle = self.config.idle_suspend_ms;
        if idle == 0 {
            return None;
        }
        let now = mq_core::now_ms();
        let due = self.broker.vhosts().iter().map(|vh| (vh.stats.last_active_ms.load(Ordering::Relaxed) + idle).saturating_sub(now)).min()?;
        // A little slack so the vhost is past its idle time when the reaper looks.
        Some(Duration::from_millis(due + 100))
    }

    /// Scale idle vhosts to zero: no connections, nothing on a timer, idle for the configured time.
    pub async fn reap_idle(&self) {
        let idle = self.config.idle_suspend_ms;
        if idle == 0 {
            return;
        }
        let now = mq_core::now_ms();
        for vh in self.broker.vhosts() {
            let last = vh.stats.last_active_ms.load(Ordering::Relaxed);
            if now.saturating_sub(last) < idle || vh.stats.connections.load(Ordering::Relaxed) != 0 {
                continue;
            }
            // What it holds is still reported while it sleeps.
            self.sampler.remember(&vh);
            if self.broker.suspend_vhost(&vh.name).await {
                debug!(vhost = %vh.name, "idle vhost suspended");
            }
        }
    }

    /// Flush metering (shutdown).
    pub fn flush(&self) {
        if let Err(e) = self.meter.flush() {
            warn!(error = %e, "metering flush failed");
        }
    }
}
