//! Direct reply-to (`amq.rabbitmq.reply-to`): RPC replies go straight to the requester's
//! consumer without a declared queue. Each reply consumer gets a private transient queue that
//! is registered here instead of in the vhost, so only its pseudo-name can reach it.

use mq_core::queue::QueueRef;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::{LazyLock, Weak};

pub const PSEUDO_QUEUE: &str = "amq.rabbitmq.reply-to";
pub const PREFIX: &str = "amq.rabbitmq.reply-to.";

static REGISTRY: LazyLock<Mutex<HashMap<String, Weak<mq_core::queue::QueueHandle>>>> = LazyLock::new(Default::default);

pub fn register(key: &str, q: &QueueRef) {
    REGISTRY.lock().insert(key.to_string(), std::sync::Arc::downgrade(q));
}

pub fn unregister(key: &str) {
    REGISTRY.lock().remove(key);
}

pub fn lookup(routing_key: &str) -> Option<QueueRef> {
    let key = routing_key.strip_prefix(PREFIX)?;
    REGISTRY.lock().get(key)?.upgrade()
}
