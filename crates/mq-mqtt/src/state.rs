//! State shared by every MQTT connection of a broker (across listeners): retained messages,
//! persisted in the metadata store under kind [`RETAINED_KIND`], and the client-id registry
//! used for session takeover.

use crate::packet::Props;
use crate::topic;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use mq_core::Broker;
use mq_core::meta;
use mq_core::queue::ConnId;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{Notify, OnceCell, watch};
use tracing::warn;

/// Metadata kind of retained messages; key `vhost \x1f topic`.
pub const RETAINED_KIND: &str = "mqtt_retained";

pub(crate) struct BrokerState {
    broker: Weak<Broker>,
    retained: Mutex<HashMap<String, Arc<VhostRetained>>>,
    sessions: Mutex<HashMap<(String, String), Arc<SessionCtl>>>,
}

static STATES: Mutex<Vec<Arc<BrokerState>>> = Mutex::new(Vec::new());

/// The shared state of `broker` (created on first use).
pub(crate) fn of(broker: &Arc<Broker>) -> Arc<BrokerState> {
    let mut all = STATES.lock();
    all.retain(|s| s.broker.strong_count() > 0);
    if let Some(s) = all.iter().find(|s| s.broker.as_ptr() == Arc::as_ptr(broker)) {
        return s.clone();
    }
    let s = Arc::new(BrokerState { broker: Arc::downgrade(broker), retained: Mutex::default(), sessions: Mutex::default() });
    all.push(s.clone());
    s
}

// ── retained messages ──

/// A retained application message.
#[derive(Clone, Debug, PartialEq)]
pub struct Retained {
    pub topic: String,
    pub qos: u8,
    pub payload: Bytes,
    /// Application properties (payload format, content type, response topic, correlation
    /// data, user properties).
    pub props: Props,
    pub expires_at_ms: Option<u64>,
}

impl Retained {
    fn to_json(&self, vhost: &str) -> Value {
        let p = &self.props;
        json!({
            "vhost": vhost,
            "topic": self.topic,
            "qos": self.qos,
            "payload": B64.encode(&self.payload),
            "expires_at_ms": self.expires_at_ms,
            "props": {
                "payload_format": p.payload_format,
                "content_type": p.content_type,
                "response_topic": p.response_topic,
                "correlation_data": p.correlation_data.as_ref().map(|c| B64.encode(c)),
                "user": p.user,
            },
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        let p = &v["props"];
        let props = Props {
            payload_format: p["payload_format"].as_u64().map(|x| x as u8),
            content_type: p["content_type"].as_str().map(String::from),
            response_topic: p["response_topic"].as_str().map(String::from),
            correlation_data: p["correlation_data"].as_str().and_then(|s| B64.decode(s).ok()).map(Bytes::from),
            user: serde_json::from_value(p["user"].clone()).unwrap_or_default(),
            ..Default::default()
        };
        Some(Retained {
            topic: v["topic"].as_str()?.to_string(),
            qos: v["qos"].as_u64().unwrap_or(0).min(1) as u8,
            payload: Bytes::from(B64.decode(v["payload"].as_str()?).ok()?),
            props,
            expires_at_ms: v["expires_at_ms"].as_u64(),
        })
    }
}

#[derive(Default)]
struct VhostRetained {
    loaded: OnceCell<()>,
    msgs: RwLock<BTreeMap<String, Retained>>,
}

impl BrokerState {
    fn broker(&self) -> Option<Arc<Broker>> {
        self.broker.upgrade()
    }

    async fn retained_for(&self, vhost: &str) -> anyhow::Result<Arc<VhostRetained>> {
        let vr = self.retained.lock().entry(vhost.to_string()).or_default().clone();
        let broker = self.broker().ok_or_else(|| anyhow::anyhow!("broker stopped"))?;
        vr.loaded
            .get_or_try_init(|| async {
                let prefix = format!("{vhost}\u{1f}");
                for (k, v) in broker.meta.list(RETAINED_KIND).await? {
                    if k.starts_with(&prefix)
                        && let Some(r) = Retained::from_json(&v)
                    {
                        vr.msgs.write().insert(r.topic.clone(), r);
                    }
                }
                anyhow::Ok(())
            })
            .await?;
        Ok(vr)
    }

    /// Store (or, with an empty payload, clear) the retained message of a topic.
    pub async fn retain(&self, vhost: &str, msg: Retained) -> anyhow::Result<()> {
        let vr = self.retained_for(vhost).await?;
        let broker = self.broker().ok_or_else(|| anyhow::anyhow!("broker stopped"))?;
        let key = meta::key(&[vhost, &msg.topic]);
        if msg.payload.is_empty() {
            vr.msgs.write().remove(&msg.topic);
            broker.meta.delete(RETAINED_KIND, &key).await?;
        } else {
            broker.meta.put(RETAINED_KIND, &key, &msg.to_json(vhost)).await?;
            vr.msgs.write().insert(msg.topic.clone(), msg);
        }
        Ok(())
    }

    /// Unexpired retained messages whose topic matches `filter`.
    pub async fn retained_matching(&self, vhost: &str, filter: &str, now_ms: u64) -> Vec<Retained> {
        let vr = match self.retained_for(vhost).await {
            Ok(vr) => vr,
            Err(e) => {
                warn!(vhost, error = %e, "failed to load retained messages");
                return vec![];
            }
        };
        let (live, expired): (Vec<Retained>, Vec<Retained>) = {
            let m = vr.msgs.read();
            let hits: Vec<Retained> = if topic::valid_name(filter) {
                m.get(filter).cloned().into_iter().collect()
            } else {
                m.values().filter(|r| topic::matches(filter, &r.topic)).cloned().collect()
            };
            hits.into_iter().partition(|r| r.expires_at_ms.is_none_or(|e| e > now_ms))
        };
        for r in expired {
            let _ = self.retain(vhost, Retained { payload: Bytes::new(), ..r }).await;
        }
        live
    }

    // ── client ids ──

    /// Register a session for `client_id`, returning the one it replaces (if any).
    pub fn register(&self, vhost: &str, client_id: &str, ctl: Arc<SessionCtl>) -> Option<Arc<SessionCtl>> {
        self.sessions.lock().insert((vhost.to_string(), client_id.to_string()), ctl)
    }

    /// Forget a session (only if it is still the registered one).
    pub fn unregister(&self, vhost: &str, client_id: &str, conn: ConnId) {
        let mut s = self.sessions.lock();
        let key = (vhost.to_string(), client_id.to_string());
        if s.get(&key).is_some_and(|c| c.conn == conn) {
            s.remove(&key);
        }
    }
}

/// Lets a newer connection with the same client id take a session over.
pub(crate) struct SessionCtl {
    pub conn: ConnId,
    pub kick: Notify,
    pub kicked: AtomicBool,
    /// Whether the replaced connection publishes its will.
    pub send_will: AtomicBool,
    pub done: watch::Sender<bool>,
}

impl SessionCtl {
    pub fn new(conn: ConnId) -> Arc<Self> {
        Arc::new(SessionCtl { conn, kick: Notify::new(), kicked: AtomicBool::new(false), send_will: AtomicBool::new(true), done: watch::channel(false).0 })
    }

    /// Ask the session to close and wait (bounded) until it has cleaned up.
    pub async fn take_over(&self, send_will: bool) {
        self.send_will.store(send_will, Ordering::Release);
        self.kicked.store(true, Ordering::Release);
        self.kick.notify_one();
        let mut rx = self.done.subscribe();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), rx.wait_for(|d| *d)).await;
    }

    pub fn finished(&self) {
        self.done.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_json_roundtrip() {
        let r = Retained {
            topic: "a/b".into(),
            qos: 1,
            payload: Bytes::from_static(b"\x00\x01hi"),
            props: Props {
                payload_format: Some(1),
                content_type: Some("text/plain".into()),
                correlation_data: Some(Bytes::from_static(b"\xff")),
                user: vec![("k".into(), "v".into())],
                ..Default::default()
            },
            expires_at_ms: Some(123),
        };
        assert_eq!(Retained::from_json(&r.to_json("/")), Some(r));
    }
}
