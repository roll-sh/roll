//! Topic configs: the Kafka names, defaults and validation, and the typed view the log uses.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Long,
    Int,
    Short,
    Bool,
    Double,
    String,
    List,
}

impl Kind {
    /// `DescribeConfigs` config type codes (KIP-569).
    pub fn code(self) -> i8 {
        match self {
            Kind::Bool => 1,
            Kind::String => 2,
            Kind::Int => 3,
            Kind::Short => 4,
            Kind::Long => 5,
            Kind::Double => 6,
            Kind::List => 7,
        }
    }
}

pub struct Def {
    pub name: &'static str,
    pub kind: Kind,
    pub default: &'static str,
    pub doc: &'static str,
    /// Accepted for compatibility but has no effect (replication and flush knobs: durability
    /// comes from fsync and object storage).
    pub inert: bool,
}

const fn def(name: &'static str, kind: Kind, default: &'static str, doc: &'static str) -> Def {
    Def { name, kind, default, doc, inert: false }
}
const fn inert(name: &'static str, kind: Kind, default: &'static str) -> Def {
    Def { name, kind, default, doc: "Accepted for compatibility; has no effect on roll.", inert: true }
}

pub const MAX: &str = "9223372036854775807";

pub static DEFS: &[Def] = &[
    def("cleanup.policy", Kind::List, "delete", "delete, compact, or both: compact,delete"),
    def("compression.type", Kind::String, "producer", "Stored compression: producer keeps what the producer sent"),
    def("delete.retention.ms", Kind::Long, "86400000", "How long tombstones are kept in compacted topics"),
    inert("file.delete.delay.ms", Kind::Long, "60000"),
    inert("flush.messages", Kind::Long, MAX),
    inert("flush.ms", Kind::Long, MAX),
    inert("follower.replication.throttled.replicas", Kind::List, ""),
    def("index.interval.bytes", Kind::Int, "4096", "Bytes between sparse offset index entries"),
    inert("leader.replication.throttled.replicas", Kind::List, ""),
    def("local.retention.bytes", Kind::Long, "-2", "Bytes kept on local disk per partition (-2: the broker default)"),
    def("local.retention.ms", Kind::Long, "-2", "Time data is kept on local disk (-2: the broker default)"),
    def("max.compaction.lag.ms", Kind::Long, MAX, "Maximum time a record stays uncompacted"),
    def("max.message.bytes", Kind::Int, "1048588", "Largest record batch accepted"),
    def("message.timestamp.after.max.ms", Kind::Long, MAX, "Largest allowed timestamp ahead of broker time"),
    def("message.timestamp.before.max.ms", Kind::Long, MAX, "Largest allowed timestamp behind broker time"),
    def("message.timestamp.type", Kind::String, "CreateTime", "CreateTime or LogAppendTime"),
    def("min.cleanable.dirty.ratio", Kind::Double, "0.5", "Dirty ratio that triggers compaction"),
    def("min.compaction.lag.ms", Kind::Long, "0", "Minimum time a record stays uncompacted"),
    inert("min.insync.replicas", Kind::Int, "1"),
    inert("preallocate", Kind::Bool, "false"),
    inert("remote.log.copy.disable", Kind::Bool, "false"),
    inert("remote.log.delete.on.disable", Kind::Bool, "false"),
    inert("remote.storage.enable", Kind::Bool, "true"),
    def("retention.bytes", Kind::Long, "-1", "Bytes retained per partition (-1: unlimited)"),
    def("retention.ms", Kind::Long, "604800000", "Time records are retained (-1: forever)"),
    def("segment.bytes", Kind::Int, "67108864", "Local segment size"),
    inert("segment.index.bytes", Kind::Int, "10485760"),
    inert("segment.jitter.ms", Kind::Long, "0"),
    def("segment.ms", Kind::Long, "604800000", "Roll the local segment after this long"),
    inert("unclean.leader.election.enable", Kind::Bool, "false"),
    inert("message.downconversion.enable", Kind::Bool, "true"),
];

pub fn find(name: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.name == name)
}

/// Check a value for a config, returning the normalised value.
pub fn validate(name: &str, value: &str) -> Result<String, String> {
    let d = find(name).ok_or_else(|| format!("Unknown topic config name: {name}"))?;
    let v = value.trim();
    let bad = || format!("Invalid value {value} for configuration {name}");
    match d.kind {
        Kind::Long => {
            let n: i64 = v.parse().map_err(|_| bad())?;
            let min = match name {
                "retention.ms" | "retention.bytes" => -1,
                "local.retention.ms" | "local.retention.bytes" => -2,
                "segment.ms" => 1,
                _ => 0,
            };
            if n < min {
                return Err(bad());
            }
        }
        Kind::Int | Kind::Short => {
            let n: i32 = v.parse().map_err(|_| bad())?;
            let min = match name {
                "segment.bytes" => 1024,
                "max.message.bytes" | "min.insync.replicas" => 1,
                _ => 0,
            };
            if n < min {
                return Err(bad());
            }
        }
        Kind::Bool => {
            if !matches!(v.to_ascii_lowercase().as_str(), "true" | "false") {
                return Err(bad());
            }
            return Ok(v.to_ascii_lowercase());
        }
        Kind::Double => {
            let n: f64 = v.parse().map_err(|_| bad())?;
            if !(0.0..=1.0).contains(&n) {
                return Err(bad());
            }
        }
        Kind::String => match name {
            "compression.type" if !matches!(v, "producer" | "uncompressed" | "gzip" | "snappy" | "lz4" | "zstd") => {
                return Err(bad());
            }
            "message.timestamp.type" if !matches!(v, "CreateTime" | "LogAppendTime") => return Err(bad()),
            _ => {}
        },
        Kind::List => {
            if name == "cleanup.policy" {
                let parts: Vec<&str> = v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                if parts.is_empty() || parts.iter().any(|p| !matches!(*p, "delete" | "compact")) {
                    return Err(bad());
                }
                let mut parts = parts;
                parts.sort();
                parts.dedup();
                return Ok(parts.join(","));
            }
        }
    }
    Ok(v.to_string())
}

/// Broker-level defaults that topic configs fall back to.
#[derive(Clone, Debug)]
pub struct Defaults {
    pub retention_ms: i64,
    pub segment_bytes: i64,
    pub local_retention_ms: i64,
    pub local_retention_bytes: i64,
    pub max_message_bytes: i64,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            retention_ms: 604_800_000,
            segment_bytes: 64 << 20,
            local_retention_ms: 3_600_000,
            local_retention_bytes: 256 << 20,
            max_message_bytes: 1_048_588,
        }
    }
}

/// The effective, typed config of a topic.
#[derive(Clone, Debug, PartialEq)]
pub struct Effective {
    pub compact: bool,
    pub delete: bool,
    pub compression: String,
    pub delete_retention_ms: i64,
    pub index_interval_bytes: u64,
    pub local_retention_bytes: i64,
    pub local_retention_ms: i64,
    pub max_compaction_lag_ms: i64,
    pub max_message_bytes: usize,
    pub timestamp_after_ms: Option<i64>,
    pub timestamp_before_ms: Option<i64>,
    pub log_append_time: bool,
    pub min_cleanable_dirty_ratio: f64,
    pub min_compaction_lag_ms: i64,
    pub retention_bytes: i64,
    pub retention_ms: i64,
    pub segment_bytes: u64,
    pub segment_ms: i64,
}

/// The value of `name` in effect: the topic override, else the broker default.
pub fn value(overrides: &BTreeMap<String, String>, defaults: &Defaults, name: &str) -> (String, bool) {
    if let Some(v) = overrides.get(name) {
        return (v.clone(), false);
    }
    let v = match name {
        "retention.ms" => defaults.retention_ms.to_string(),
        "segment.bytes" => defaults.segment_bytes.to_string(),
        "max.message.bytes" => defaults.max_message_bytes.to_string(),
        _ => find(name).map(|d| d.default.to_string()).unwrap_or_default(),
    };
    (v, true)
}

pub fn effective(overrides: &BTreeMap<String, String>, defaults: &Defaults) -> Effective {
    let get = |n: &str| value(overrides, defaults, n).0;
    let long = |n: &str| get(n).parse::<i64>().unwrap_or(0);
    let policy = get("cleanup.policy");
    let limit = |n: &str| Some(long(n)).filter(|v| *v != i64::MAX);
    let local_ms = match long("local.retention.ms") {
        -2 => defaults.local_retention_ms,
        v => v,
    };
    let local_bytes = match long("local.retention.bytes") {
        -2 => defaults.local_retention_bytes,
        v => v,
    };
    Effective {
        compact: policy.contains("compact"),
        delete: policy.contains("delete"),
        compression: get("compression.type"),
        delete_retention_ms: long("delete.retention.ms"),
        index_interval_bytes: long("index.interval.bytes").max(1) as u64,
        local_retention_bytes: local_bytes,
        local_retention_ms: local_ms,
        max_compaction_lag_ms: long("max.compaction.lag.ms"),
        max_message_bytes: long("max.message.bytes").max(1) as usize,
        timestamp_after_ms: limit("message.timestamp.after.max.ms"),
        timestamp_before_ms: limit("message.timestamp.before.max.ms"),
        log_append_time: get("message.timestamp.type") == "LogAppendTime",
        min_cleanable_dirty_ratio: get("min.cleanable.dirty.ratio").parse().unwrap_or(0.5),
        min_compaction_lag_ms: long("min.compaction.lag.ms"),
        retention_bytes: long("retention.bytes"),
        retention_ms: long("retention.ms"),
        segment_bytes: long("segment.bytes").max(1024) as u64,
        segment_ms: long("segment.ms").max(1),
    }
}

/// Validate a full set of overrides (create, or alter with replacement).
pub fn validate_all(configs: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String> {
    configs.iter().map(|(k, v)| Ok((k.clone(), validate(k, v)?))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        assert_eq!(validate("cleanup.policy", "delete, compact").unwrap(), "compact,delete");
        assert!(validate("cleanup.policy", "nope").is_err());
        assert!(validate("retention.ms", "-1").is_ok());
        assert!(validate("retention.ms", "-5").is_err());
        assert!(validate("retention.ms", "abc").is_err());
        assert!(validate("segment.bytes", "100").is_err());
        assert!(validate("message.timestamp.type", "LogAppendTime").is_ok());
        assert!(validate("message.timestamp.type", "Wall").is_err());
        assert!(validate("unknown.config", "1").is_err());
        assert_eq!(validate("preallocate", "TRUE").unwrap(), "true");
        assert!(validate("min.cleanable.dirty.ratio", "1.5").is_err());
    }

    #[test]
    fn effective_values() {
        let d = Defaults::default();
        let e = effective(&BTreeMap::new(), &d);
        assert!(e.delete && !e.compact);
        assert_eq!(e.retention_ms, 604_800_000);
        assert_eq!(e.local_retention_bytes, d.local_retention_bytes);
        assert_eq!(e.timestamp_after_ms, None);
        let o: BTreeMap<String, String> = [
            ("cleanup.policy", "compact"),
            ("retention.ms", "1000"),
            ("message.timestamp.type", "LogAppendTime"),
            ("message.timestamp.after.max.ms", "3600000"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let e = effective(&o, &d);
        assert!(e.compact && !e.delete && e.log_append_time);
        assert_eq!((e.retention_ms, e.timestamp_after_ms), (1000, Some(3_600_000)));
        assert_eq!(value(&o, &d, "retention.ms"), ("1000".into(), false));
        assert_eq!(value(&o, &d, "segment.bytes"), ((64 << 20).to_string(), true));
    }
}
