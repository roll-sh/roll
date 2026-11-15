//! Cluster-scoped settings (`PUT/GET _cluster/settings`): validation against the registry of
//! Elasticsearch's settings ([`CLUSTER_SETTINGS`]) and affix settings (`logger.*`,
//! allocation filters, remote clusters), with Elasticsearch's error texts.
//!
//! Values are validated with the same typed rules as index settings. Settings that only take
//! effect on node start (`path.*`, `node.*`, …) are known but not updateable.

pub use super::cluster_settings_table::CLUSTER_SETTINGS;
pub use super::cluster_settings_table_os::OS_CLUSTER_SETTINGS;
use super::index_settings::{Dflt, IndexSetting, Scope, validate_value};
use super::settings::{SettingValue, Settings};
use crate::cluster::Dialect;
use crate::error::EsError;
use crate::units::parse_java_double;

/// The registry of a dialect.
pub fn registry(d: Dialect) -> &'static [IndexSetting] {
    match d {
        Dialect::Elasticsearch => CLUSTER_SETTINGS,
        Dialect::OpenSearch => OS_CLUSTER_SETTINGS,
    }
}

pub fn lookup(key: &str) -> Option<&'static IndexSetting> {
    lookup_in(Dialect::Elasticsearch, key)
}

pub fn lookup_in(d: Dialect, key: &str) -> Option<&'static IndexSetting> {
    let r = registry(d);
    r.binary_search_by(|s| s.key.cmp(key)).ok().map(|i| &r[i])
}

/// Group/affix settings any key below is accepted for (with a value check by kind).
#[derive(Clone, Copy, PartialEq)]
enum Affix {
    Str,
    LogLevel,
    List,
}

const AFFIXES: &[(&str, &str, Affix)] = &[
    ("logger.", "", Affix::LogLevel),
    ("cluster.routing.allocation.include.", "", Affix::Str),
    ("cluster.routing.allocation.exclude.", "", Affix::Str),
    ("cluster.routing.allocation.require.", "", Affix::Str),
    ("cluster.routing.allocation.awareness.force.", ".values", Affix::List),
    ("cluster.remote.", ".seeds", Affix::List),
    ("cluster.remote.", ".mode", Affix::Str),
    ("cluster.remote.", ".skip_unavailable", Affix::Str),
    ("cluster.remote.", ".transport.ping_schedule", Affix::Str),
    ("cluster.remote.", ".transport.compress", Affix::Str),
    ("cluster.remote.", ".proxy_address", Affix::Str),
    ("cluster.remote.", ".server_name", Affix::Str),
    ("cluster.remote.", ".node_connections", Affix::Str),
    ("cluster.remote.", ".proxy_socket_connections", Affix::Str),
    ("xpack.notification.", "", Affix::Str),
    ("cluster.metadata.", "", Affix::Str),
];

fn affix(key: &str) -> Option<Affix> {
    AFFIXES
        .iter()
        .find(|(p, s, _)| key.len() > p.len() + s.len() && key.starts_with(p) && key.ends_with(s) && (!s.is_empty() || !key[p.len()..].is_empty()))
        .map(|(_, _, a)| *a)
}

const LEVELS: &[&str] = &["OFF", "FATAL", "ERROR", "WARN", "INFO", "DEBUG", "TRACE", "ALL"];

/// Validates a `persistent` or `transient` update (`scope` names it in messages).
pub fn validate_update(scope: &str, update: &Settings) -> Result<(), EsError> {
    validate_update_in(Dialect::Elasticsearch, scope, update)
}

/// Validates an update against a dialect's registry. OpenSearch reports the registry's own
/// rejections as `settings_exception`.
pub fn validate_update_in(d: Dialect, scope: &str, update: &Settings) -> Result<(), EsError> {
    validate_update_inner(d, scope, update).map_err(|mut e| {
        let registry_rejection = e.reason().starts_with(&format!("{scope} setting [")) || e.reason().starts_with("final ");
        if d == Dialect::OpenSearch && e.kind == "illegal_argument_exception" && registry_rejection {
            e.kind = "settings_exception".into();
        }
        e
    })
}

fn validate_update_inner(d: Dialect, scope: &str, update: &Settings) -> Result<(), EsError> {
    for (k, v) in update.iter() {
        // `"cluster.*": null` resets every matching setting.
        if k.contains('*') && matches!(v, SettingValue::Null) {
            continue;
        }
        if k.starts_with("archived.") {
            if matches!(v, SettingValue::Null) {
                continue;
            }
            return Err(EsError::illegal_argument(format!("can't update [{k}]; archived settings can only be removed")));
        }
        match lookup_in(d, k) {
            Some(def) if def.scope == Scope::Static => {
                return Err(EsError::illegal_argument(format!("{scope} setting [{k}], not dynamically updateable")));
            }
            Some(def) if def.scope == Scope::Final => {
                return Err(EsError::illegal_argument(format!("final {scope} setting [{k}], not updateable")));
            }
            // OpenSearch's sensitive settings only change persistently.
            Some(def) if def.scope == Scope::Managed && scope == "transient" => {
                return Err(EsError::illegal_argument(format!("sensitive setting [{k}] must be updated using persistent settings")));
            }
            Some(def) => validate_value(def, v)?,
            None => match affix(k) {
                Some(Affix::LogLevel) => {
                    if let SettingValue::Str(s) = v
                        && !LEVELS.contains(&s.to_ascii_uppercase().as_str())
                    {
                        return Err(EsError::illegal_argument(format!("Unknown level constant [{}].", s.to_ascii_uppercase())));
                    }
                }
                Some(Affix::Str | Affix::List) => {}
                None => return Err(EsError::illegal_argument(format!("{scope} setting [{k}], not recognized"))),
            },
        }
    }
    Ok(())
}

const WATERMARKS: [(&str, &str); 3] = [
    ("cluster.routing.allocation.disk.watermark.low", "85%"),
    ("cluster.routing.allocation.disk.watermark.high", "90%"),
    ("cluster.routing.allocation.disk.watermark.flood_stage", "95%"),
];

/// `RatioValue.parseRatioValue(..).getAsPercent()`.
fn watermark_percent(s: &str) -> Option<f64> {
    match s.strip_suffix('%') {
        Some(p) => parse_java_double(p).filter(|v| (0.0..=100.0).contains(v)),
        None => parse_java_double(s).filter(|v| (0.0..=1.0).contains(v)).map(|r| r * 100.0),
    }
}

/// `DiskThresholdSettings`' watermark validator: low ≤ high ≤ flood stage, all percentages
/// (ratios) or all byte sizes (free space, so the order flips). `value` gives each watermark's
/// value after the update (`None`: the default); `updated` counts the watermarks the request
/// sets, each of whose validators fails the same way (all but the first as suppressed).
pub fn validate_watermarks(value: impl Fn(&str) -> Option<String>, updated: usize) -> Result<(), EsError> {
    if updated == 0 {
        return Ok(());
    }
    let [low, high, flood] = WATERMARKS.map(|(k, d)| value(k).unwrap_or_else(|| d.to_string()));
    let msg = match (watermark_percent(&low), watermark_percent(&high), watermark_percent(&flood)) {
        (Some(l), Some(h), Some(f)) => {
            if l > h {
                Some(format!("low disk watermark [{low}] more than high disk watermark [{high}]"))
            } else if h > f {
                Some(format!("high disk watermark [{high}] more than flood stage disk watermark [{flood}]"))
            } else {
                None
            }
        }
        _ => {
            let bytes = |s: &str, k: &str| crate::units::ByteSize::parse(s, k).ok().map(|b| b.bytes());
            match (bytes(&low, WATERMARKS[0].0), bytes(&high, WATERMARKS[1].0), bytes(&flood, WATERMARKS[2].0)) {
                (Some(l), Some(h), Some(f)) => {
                    if l < h {
                        Some(format!("low disk watermark [{low}] less than high disk watermark [{high}]"))
                    } else if h < f {
                        Some(format!("high disk watermark [{high}] less than flood stage disk watermark [{flood}]"))
                    } else {
                        None
                    }
                }
                _ => Some(format!(
                    "unable to consistently parse [{}={low}], [{}={high}], and [{}={flood}] as percentage or bytes",
                    WATERMARKS[0].0, WATERMARKS[1].0, WATERMARKS[2].0
                )),
            }
        }
    };
    match msg {
        None => Ok(()),
        Some(m) => {
            let e = EsError::illegal_argument(m.clone());
            Err(if updated > 1 { e.with_suppressed((1..updated).map(|_| EsError::illegal_argument(m.clone())).collect()) } else { e })
        }
    }
}

/// How many of the disk watermarks an update sets.
pub fn watermarks_in(update: &Settings) -> impl Iterator<Item = &'static str> + '_ {
    WATERMARKS.iter().map(|(k, _)| *k).filter(|k| update.contains(k))
}

/// Every registered default not in `set` (`include_defaults=true`), with the node's identity
/// in place of Elasticsearch's container values.
pub fn defaults_for(set: &Settings, cluster_name: &str, node_name: &str) -> Settings {
    defaults_for_in(Dialect::Elasticsearch, set, cluster_name, node_name)
}

pub fn defaults_for_in(d: Dialect, set: &Settings, cluster_name: &str, node_name: &str) -> Settings {
    let mut out = Settings::new();
    for def in registry(d) {
        if set.contains(def.key) {
            continue;
        }
        let v = match def.key {
            "cluster.name" => SettingValue::Str(cluster_name.to_string()),
            "node.name" | "node.external_id" => SettingValue::Str(node_name.to_string()),
            _ => match def.default {
                Dflt::Str(s) => SettingValue::Str(s.to_string()),
                Dflt::List(l) => SettingValue::List(l.iter().map(|s| s.to_string()).collect()),
                Dflt::Computed | Dflt::None => continue,
            },
        };
        out.insert(def.key, v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upd(pairs: &[(&str, Option<&str>)]) -> Settings {
        let mut s = Settings::new();
        for (k, v) in pairs {
            s.insert(*k, v.map(|v| SettingValue::Str(v.to_string())).unwrap_or(SettingValue::Null));
        }
        s
    }

    #[test]
    fn registry_sorted() {
        for w in CLUSTER_SETTINGS.windows(2) {
            assert!(w[0].key < w[1].key, "{} >= {}", w[0].key, w[1].key);
        }
    }

    #[test]
    fn texts_match_elasticsearch() {
        let e = validate_update("persistent", &upd(&[("esm.bogus", Some("1"))])).unwrap_err();
        assert_eq!(e.reason(), "persistent setting [esm.bogus], not recognized");
        let e = validate_update("persistent", &upd(&[("index.number_of_shards", Some("1"))])).unwrap_err();
        assert_eq!(e.reason(), "persistent setting [index.number_of_shards], not recognized");
        let e = validate_update("transient", &upd(&[("indices.recovery.max_bytes_per_sec", Some("abc"))])).unwrap_err();
        assert_eq!(e.reason(), "failed to parse setting [indices.recovery.max_bytes_per_sec] with value [abc] as a size in bytes: unit is missing or unrecognized");
        assert_eq!(e.caused_by.unwrap().kind, "parse_exception");
        let e = validate_update("persistent", &upd(&[("cluster.routing.allocation.enable", Some("bogus"))])).unwrap_err();
        assert_eq!(e.reason(), "No enum constant org.elasticsearch.cluster.routing.allocation.decider.EnableAllocationDecider.Allocation.BOGUS");
        assert!(validate_update("persistent", &upd(&[("search.default_search_timeout", Some("5m")), ("cluster.max_shards_per_node", Some("2000"))])).is_ok());
        assert!(validate_update("persistent", &upd(&[("logger.org.elasticsearch.x", Some("DEBUG"))])).is_ok());
        assert!(validate_update("persistent", &upd(&[("cluster.routing.allocation.exclude._name", Some("n1"))])).is_ok());
        assert!(validate_update("persistent", &upd(&[("search.default_search_timeout", None)])).is_ok());
        let e = validate_update("persistent", &upd(&[("path.data", Some("/x"))])).unwrap_err();
        assert_eq!(e.reason(), "persistent setting [path.data], not dynamically updateable");
    }
}
