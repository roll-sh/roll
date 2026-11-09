//! Policies and operator policies.

use crate::auth::RegexCache;
use crate::queue::QueueType;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Policy {
    pub vhost: String,
    pub name: String,
    pub pattern: String,
    #[serde(rename = "apply-to", default = "default_apply_to")]
    pub apply_to: String,
    pub definition: Value,
    #[serde(default)]
    pub priority: i64,
}

fn default_apply_to() -> String {
    "all".into()
}

/// What a policy is being matched against.
#[derive(Clone, Copy, Debug)]
pub enum Target {
    Exchange,
    Queue(QueueType),
}

impl Policy {
    pub fn applies(&self, target: Target, name: &str, re: &RegexCache) -> bool {
        let kind_ok = matches!(
            (self.apply_to.as_str(), target),
            ("all", _)
                | ("exchanges", Target::Exchange)
                | ("queues", Target::Queue(_))
                | ("classic_queues", Target::Queue(QueueType::Classic))
                | ("quorum_queues", Target::Queue(QueueType::Quorum))
                | ("streams", Target::Queue(QueueType::Stream))
        );
        kind_ok && re.matches(&self.pattern, name)
    }
}

/// The winning policy: highest priority, then name (deterministic across nodes).
pub fn pick<'a>(policies: &'a [Policy], vhost: &str, target: Target, name: &str, re: &RegexCache) -> Option<&'a Policy> {
    policies
        .iter()
        .filter(|p| p.vhost == vhost && p.applies(target, name, re))
        .max_by(|a, b| a.priority.cmp(&b.priority).then_with(|| b.name.cmp(&a.name)))
}

/// Keys operator policies may set (RabbitMQ's allow-list).
pub const OPERATOR_KEYS: &[&str] =
    &["expires", "message-ttl", "max-length", "max-length-bytes", "max-in-memory-length", "max-in-memory-bytes", "delivery-limit", "queue-version", "overflow", "target-group-size"];

/// Whether a policy key is obsolete in RabbitMQ 4: classic queue mirroring (`ha-*`, removed in
/// 4.0) and `queue-master-locator` (replaced by `queue-leader-locator`).
pub fn is_obsolete_key(key: &str) -> bool {
    key.starts_with("ha-") || key == "queue-master-locator"
}

/// Remove obsolete keys (see [`is_obsolete_key`]) from a definition; returns the keys removed.
pub fn strip_obsolete(def: &mut Value) -> Vec<String> {
    let Some(obj) = def.as_object_mut() else { return Vec::new() };
    let dropped: Vec<String> = obj.keys().filter(|k| is_obsolete_key(k)).cloned().collect();
    for k in &dropped {
        obj.remove(k);
    }
    dropped
}

/// Validate a policy definition like RabbitMQ does (unknown keys are rejected).
pub fn validate_definition(def: &Value, operator: bool) -> Result<(), String> {
    let Some(obj) = def.as_object() else {
        return Err("definition must be an object".into());
    };
    if obj.is_empty() {
        return Err("definition must not be empty".into());
    }
    const KNOWN: &[&str] = &[
        "alternate-exchange",
        "dead-letter-exchange",
        "dead-letter-routing-key",
        "dead-letter-strategy",
        "delivery-limit",
        "expires",
        "federation-upstream",
        "federation-upstream-set",
        "ha-mode",
        "ha-params",
        "ha-sync-mode",
        "ha-promote-on-shutdown",
        "ha-promote-on-failure",
        "initial-cluster-size",
        "max-age",
        "max-length",
        "max-length-bytes",
        "max-in-memory-length",
        "max-in-memory-bytes",
        "message-ttl",
        "overflow",
        "queue-leader-locator",
        "queue-mode",
        "queue-master-locator",
        "queue-version",
        "single-active-consumer",
        "stream-max-segment-size-bytes",
        "target-group-size",
        "consumer-timeout",
        "stream-filter-size-bytes",
    ];
    for (k, v) in obj {
        if operator && !OPERATOR_KEYS.contains(&k.as_str()) {
            return Err(format!("[{{<<\"{k}\">>,{v}}}] are not recognised operator policy keys"));
        }
        if !KNOWN.contains(&k.as_str()) {
            return Err(format!("Validation failed\n\n[{{<<\"{k}\">>,{v}}}] are not recognised policy settings\n"));
        }
        let numeric = ["delivery-limit", "expires", "max-length", "max-length-bytes", "message-ttl", "consumer-timeout", "stream-max-segment-size-bytes"];
        if numeric.contains(&k.as_str()) && v.as_i64().is_none_or(|n| n < 0) {
            return Err(format!("Validation failed\n\n{v} is not a valid {k} value\n"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn picks_highest_priority() {
        let re = RegexCache::default();
        let p = |name: &str, pattern: &str, prio: i64, apply: &str| Policy {
            vhost: "/".into(),
            name: name.into(),
            pattern: pattern.into(),
            apply_to: apply.into(),
            definition: json!({"max-length": 1}),
            priority: prio,
        };
        let ps = vec![p("a", "^q", 0, "queues"), p("b", "^q\\.", 5, "queues"), p("c", ".*", 10, "exchanges")];
        assert_eq!(pick(&ps, "/", Target::Queue(QueueType::Classic), "q.1", &re).unwrap().name, "b");
        assert_eq!(pick(&ps, "/", Target::Queue(QueueType::Classic), "qq", &re).unwrap().name, "a");
        assert_eq!(pick(&ps, "/", Target::Exchange, "q.1", &re).unwrap().name, "c");
        assert!(pick(&ps, "other", Target::Exchange, "q.1", &re).is_none());
    }

    #[test]
    fn validates() {
        assert!(validate_definition(&json!({"max-length": 10}), true).is_ok());
        assert!(validate_definition(&json!({"dead-letter-exchange": "x"}), true).is_err());
        assert!(validate_definition(&json!({"bogus": 1}), false).is_err());
        assert!(validate_definition(&json!({"max-length": -1}), false).is_err());
    }

    #[test]
    fn strips_classic_mirroring_keys() {
        let mut d = json!({"ha-mode": "all", "ha-sync-batch-size": 10, "queue-master-locator": "min-masters", "max-length": 5});
        let mut dropped = strip_obsolete(&mut d);
        dropped.sort();
        assert_eq!(dropped, ["ha-mode", "ha-sync-batch-size", "queue-master-locator"]);
        assert_eq!(d, json!({"max-length": 5}));
        assert!(strip_obsolete(&mut json!("x")).is_empty());
    }
}
