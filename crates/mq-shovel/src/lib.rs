//! roll queues shovels, federation and live migration from RabbitMQ.
//!
//! [`Shovels`] follows the broker's runtime parameters and policies and runs a worker per
//! dynamic shovel (component `shovel`) and per federation link (`federation-upstream`,
//! `federation-upstream-set` and the matching policy keys). Endpoints are either the local
//! broker (`amqp://` without a host, driven in-process through `mq-core`) or remote AMQP 0-9-1
//! brokers (`amqp://` / `amqps://`, via lapin). [`migrate`] imports a RabbitMQ's definitions
//! and moves its messages with shovels.

mod convert;
pub mod endpoint;
pub mod federation;
pub mod migrate;
pub mod pump;
pub mod shovel;
pub mod status;
pub mod uri;

pub use federation::{validate_upstream, validate_upstream_set};
pub use migrate::{Imported, MIGRATION_PREFIX, MigrateReport, MigrateRequest, migrate, migrate_body, stop_migration, stop_migration_body};
pub use shovel::validate as validate_shovel;

use federation::{LinkKey, UpstreamDef};
use mq_core::Broker;
use mq_core::broker::Event;
use parking_lot::Mutex;
use serde_json::{Value, json};
use shovel::ShovelDef;
use status::{LinkStatus, State};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

/// Validate a runtime parameter of one of the components this crate handles (`shovel`,
/// `federation-upstream`, `federation-upstream-set`); other components pass.
pub fn validate_parameter(component: &str, value: &Value) -> Result<(), String> {
    match component {
        "shovel" => validate_shovel(value),
        "federation-upstream" => validate_upstream(value),
        "federation-upstream-set" => validate_upstream_set(value),
        _ => Ok(()),
    }
}

struct Running {
    def: Value,
    status: Arc<LinkStatus>,
    task: JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Runs the broker's shovels and federation links.
pub struct Shovels {
    broker: Arc<Broker>,
    shovels: Mutex<BTreeMap<(String, String), Running>>,
    links: Mutex<BTreeMap<LinkKey, Running>>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

impl Shovels {
    /// Start the shovels and federation links defined now, and follow parameter and policy
    /// changes from then on. Must be called inside a Tokio runtime.
    pub fn start(broker: Arc<Broker>) -> Arc<Shovels> {
        let me = Arc::new(Shovels { broker: broker.clone(), shovels: Mutex::default(), links: Mutex::default(), watcher: Mutex::default() });
        let mut events = broker.events.subscribe();
        me.reconcile();
        let weak: Weak<Shovels> = Arc::downgrade(&me);
        let watcher = tokio::spawn(async move {
            // Exchanges and queues appear without events, so links are also re-checked often,
            // but only while some vhost federates (otherwise the watcher just waits for events).
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let federating = weak.upgrade().is_some_and(|me| !me.broker.parameters(Some("federation-upstream"), None).is_empty());
                let ev = tokio::select! {
                    e = events.recv() => Some(e),
                    _ = tick.tick(), if federating => None,
                };
                let Some(me) = weak.upgrade() else { return };
                match ev {
                    Some(Ok(Event::ParameterChanged { component, .. })) if component == "shovel" => me.reconcile_shovels(),
                    Some(Ok(_)) | None => me.reconcile(),
                    Some(Err(RecvError::Lagged(_))) => me.reconcile(),
                    Some(Err(RecvError::Closed)) => return,
                }
            }
        });
        *me.watcher.lock() = Some(watcher);
        me
    }

    fn reconcile(&self) {
        self.reconcile_shovels();
        self.reconcile_links();
    }

    fn reconcile_shovels(&self) {
        let desired: BTreeMap<(String, String), Value> =
            self.broker.parameters(Some("shovel"), None).into_iter().map(|(_, vhost, name, v)| ((vhost, name), v)).collect();
        let mut map = self.shovels.lock();
        map.retain(|k, r| desired.get(k) == Some(&r.def));
        for (k, def) in desired {
            if let std::collections::btree_map::Entry::Vacant(e) = map.entry(k) {
                let (vhost, name) = e.key().clone();
                e.insert(self.spawn_shovel(&vhost, &name, def));
            }
        }
    }

    fn reconcile_links(&self) {
        let desired = federation::desired(&self.broker);
        let mut map = self.links.lock();
        map.retain(|k, r| desired.get(k) == Some(&r.def));
        for (k, def) in desired {
            if let std::collections::btree_map::Entry::Vacant(e) = map.entry(k) {
                let key = e.key().clone();
                e.insert(self.spawn_link(key, def));
            }
        }
    }

    fn spawn_shovel(&self, vhost: &str, name: &str, def: Value) -> Running {
        let status = Arc::new(LinkStatus::default());
        let task = match ShovelDef::parse(&def) {
            Ok(parsed) => tokio::spawn(shovel::run(self.broker.clone(), vhost.into(), name.into(), parsed, status.clone())),
            Err(e) => {
                status.set(State::Terminated, Some(e));
                tokio::spawn(async {})
            }
        };
        Running { def, status, task }
    }

    fn spawn_link(&self, key: LinkKey, def: Value) -> Running {
        let status = Arc::new(LinkStatus::default());
        let task = match UpstreamDef::parse(&def) {
            Ok(parsed) => tokio::spawn(federation::run(self.broker.clone(), key, parsed, status.clone())),
            Err(e) => {
                status.set(State::Terminated, Some(e));
                tokio::spawn(async {})
            }
        };
        Running { def, status, task }
    }

    /// Restart a shovel (`DELETE /api/shovels/vhost/{vhost}/{name}/restart`). False if there
    /// is no such shovel.
    pub fn restart(&self, vhost: &str, name: &str) -> bool {
        let key = (vhost.to_string(), name.to_string());
        let mut map = self.shovels.lock();
        let Some(old) = map.remove(&key) else { return false };
        let def = old.def.clone();
        drop(old);
        let r = self.spawn_shovel(vhost, name, def);
        map.insert(key, r);
        true
    }

    /// Stop every worker and stop following changes.
    pub fn stop(&self) {
        if let Some(w) = self.watcher.lock().take() {
            w.abort();
        }
        self.shovels.lock().clear();
        self.links.lock().clear();
    }

    /// Dynamic shovels in `/api/shovels` format.
    pub fn status(&self) -> Vec<Value> {
        let node = &self.broker.config.node_name;
        self.shovels
            .lock()
            .iter()
            .map(|((vhost, name), r)| {
                let mut o = serde_json::Map::new();
                o.insert("node".into(), json!(node));
                o.insert("name".into(), json!(name));
                o.insert("vhost".into(), json!(vhost));
                o.insert("type".into(), json!("dynamic"));
                o.insert("state".into(), json!(r.status.state().as_str()));
                o.insert("timestamp".into(), json!(r.status.timestamp()));
                if let Some(reason) = r.status.reason() {
                    o.insert("reason".into(), json!(reason));
                }
                if r.status.state() == State::Running {
                    o.insert("blocked_status".into(), json!("running"));
                }
                if let Ok(def) = ShovelDef::parse(&r.def) {
                    def.describe(&mut o);
                }
                o.insert("forwarded".into(), json!(r.status.forwarded()));
                if let Some(n) = r.status.remaining() {
                    o.insert("remaining".into(), json!(n));
                }
                Value::Object(o)
            })
            .collect()
    }

    /// Federation links in `/api/federation-links` format.
    pub fn federation_status(&self) -> Vec<Value> {
        let node = &self.broker.config.node_name;
        self.links.lock().iter().map(|(k, r)| federation::link_json(node, k, &r.def, &r.status)).collect()
    }

    /// Migration shovels (`GET /v1/queues/migrate`): name, vhost, state, src, dest,
    /// transferred, remaining.
    pub fn migration_status(&self) -> Vec<Value> {
        self.shovels
            .lock()
            .iter()
            .filter(|((_, name), _)| name.starts_with(MIGRATION_PREFIX))
            .map(|((vhost, name), r)| {
                let def = ShovelDef::parse(&r.def).ok();
                let src = def.as_ref().map(|d| format!("{} @ {}", d.src_queue.as_deref().unwrap_or_default(), uri::redact(&d.src_uris[0])));
                let dest = def.as_ref().map(|d| d.dest_queue.clone().unwrap_or_default());
                json!({
                    "name": name,
                    "vhost": vhost,
                    "state": r.status.state().as_str(),
                    "reason": r.status.reason(),
                    "src": src,
                    "dest": dest,
                    "transferred": r.status.forwarded(),
                    "remaining": r.status.remaining(),
                })
            })
            .collect()
    }
}

impl Shovels {
    /// `GET /v1/queues/migrate` body: `{ shovels: [...] }`.
    pub fn migration_status_body(&self) -> Value {
        json!({ "shovels": self.migration_status() })
    }
}

impl Drop for Shovels {
    fn drop(&mut self) {
        self.stop();
    }
}
