//! A database's `settings`: the `cassandra.yaml` / `scylla.yaml` options that change what
//! clients see, as JSON overrides on top of the reference image's defaults for the dialect
//! (`nosql_engine::Settings::defaults`).
//!
//! Keys are the engine's field names, plus the spellings of both configuration files
//! (`batch_size_warn_threshold_in_kb`, `enable_user_defined_functions`, `smp`, …). Values may be
//! JSON numbers and booleans or strings (`"10"`, `"5KiB"`, `"true"`), as `roll nosql create
//! --setting k=v` sends them. Unknown keys and bad values are rejected, so a typo never passes
//! silently.
//!
//! `memtable_space` (also `memtable_heap_space`, `memtable_heap_space_in_mb`) is the database's
//! memtable cap — its plan's share of the node's memtable budget. It goes to storage
//! ([`memtable_space`]), not to the engine.

use nosql_engine::{DbConfig, Settings};
use serde_json::{Map, Value};

/// Every accepted key (canonical names first), for errors and `GET /v1/nosql`.
pub const KEYS: &[&str] = &[
    "batch_size_warn_threshold",
    "batch_size_fail_threshold",
    "unlogged_batch_across_partitions_warn_threshold",
    "tombstone_warn_threshold",
    "tombstone_failure_threshold",
    "materialized_views_enabled",
    "sasi_indexes_enabled",
    "user_defined_functions_enabled",
    "compact_storage_enabled",
    "drop_compact_storage_enabled",
    "max_partition_key_restrictions_per_query",
    "max_clustering_key_restrictions_per_query",
    "minimum_replication_factor_warn_threshold",
    "tablets_by_default",
    "allow_filtering_enabled",
    "authenticator",
    "auth_certificate_role_queries",
    "authorizer",
    "network_authorizer",
    "permissions_validity",
    "roles_validity",
    "credentials_validity",
    "auth_bcrypt_log2_rounds",
    "role_password_update_min_interval",
    "shards",
    "num_tokens",
    "memtable_space",
];

/// The smallest memtable cap a database may have.
pub const MIN_MEMTABLE_SPACE: u64 = 1 << 20;

/// The database's memtable cap from its settings (`None`: the node's default).
pub fn memtable_space(overrides: &Map<String, Value>) -> Result<Option<u64>, String> {
    let mut out = None;
    for (k, v) in overrides {
        let k = k.trim();
        let bytes = match k {
            "memtable_space" | "memtable_heap_space" => bytes_of(k, v, 1)?,
            "memtable_heap_space_in_mb" => bytes_of(k, v, 1 << 20)?,
            _ => continue,
        };
        if bytes < MIN_MEMTABLE_SPACE {
            return Err(format!("{k} must be at least 1MiB"));
        }
        out = Some(bytes);
    }
    Ok(out)
}

fn bool_of(k: &str, v: &Value) -> Result<bool, String> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "on" | "1" | "enabled" => Ok(true),
            "false" | "no" | "off" | "0" | "disabled" => Ok(false),
            _ => Err(format!("{k}: expected true or false, got {s:?}")),
        },
        Value::Number(n) if n.as_u64() == Some(0) || n.as_u64() == Some(1) => Ok(n.as_u64() == Some(1)),
        _ => Err(format!("{k}: expected true or false, got {v}")),
    }
}

fn u64_of(k: &str, v: &Value) -> Result<u64, String> {
    match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| format!("{k}: expected a non-negative integer, got {n}")),
        Value::String(s) => s.trim().parse().map_err(|_| format!("{k}: expected a non-negative integer, got {s:?}")),
        _ => Err(format!("{k}: expected a non-negative integer, got {v}")),
    }
}

/// A size in bytes: a number of `unit` bytes, or a string with a unit (`5KiB`, `1MiB`, `512B`).
fn bytes_of(k: &str, v: &Value, unit: u64) -> Result<u64, String> {
    if let Value::String(s) = v {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (n, u) = s.split_at(split);
        if !u.is_empty() {
            let n: u64 = n.parse().map_err(|_| format!("{k}: bad size {s:?}"))?;
            let mult = match u.trim().to_ascii_lowercase().as_str() {
                "b" => 1,
                "kib" | "kb" | "k" => 1 << 10,
                "mib" | "mb" | "m" => 1 << 20,
                "gib" | "gb" | "g" => 1 << 30,
                _ => return Err(format!("{k}: bad size unit in {s:?} (B, KiB, MiB or GiB)")),
            };
            return n.checked_mul(mult).ok_or_else(|| format!("{k}: {s:?} is too large"));
        }
    }
    u64_of(k, v)?.checked_mul(unit).ok_or_else(|| format!("{k}: too large"))
}

/// A duration in milliseconds: a number of `unit` ms, or a string with a unit (`2s`, `500ms`).
fn ms_of(k: &str, v: &Value, unit: u64) -> Result<u64, String> {
    if let Value::String(s) = v {
        let s = s.trim();
        let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        let (n, u) = s.split_at(split);
        if !u.is_empty() {
            let n: u64 = n.parse().map_err(|_| format!("{k}: bad duration {s:?}"))?;
            let mult = match u.trim() {
                "ms" => 1,
                "s" => 1000,
                "m" => 60_000,
                "h" => 3_600_000,
                _ => return Err(format!("{k}: bad duration unit in {s:?} (ms, s, m or h)")),
            };
            return n.checked_mul(mult).ok_or_else(|| format!("{k}: {s:?} is too large"));
        }
    }
    u64_of(k, v)?.checked_mul(unit).ok_or_else(|| format!("{k}: too large"))
}

fn ranged(k: &str, v: u64, lo: u64, hi: u64) -> Result<u64, String> {
    if (lo..=hi).contains(&v) { Ok(v) } else { Err(format!("{k} must be between {lo} and {hi}")) }
}

fn class_of(k: &str, v: &Value, known: &[&str]) -> Result<String, String> {
    let Value::String(s) = v else { return Err(format!("{k}: expected a class name, got {v}")) };
    let short = s.rsplit('.').next().unwrap_or(s);
    if known.contains(&short) { Ok(short.to_string()) } else { Err(format!("{k}: unknown class {s:?} ({})", known.join(", "))) }
}

/// An `authenticator` class of the database's dialect (the certificate authenticators are each
/// reference's own: `nosql_engine::auth::certificate`).
fn authenticator_of(k: &str, v: &Value, d: nosql_engine::Dialect) -> Result<String, String> {
    use nosql_engine::auth::certificate::Authenticator;
    let known = [
        "AllowAllAuthenticator",
        "PasswordAuthenticator",
        "MutualTlsAuthenticator",
        "MutualTlsWithPasswordFallbackAuthenticator",
        "CertificateAuthenticator",
        "CertificateOrPasswordAuthenticator",
    ];
    let c = class_of(k, v, &known)?;
    if !Authenticator::of(&c).of_dialect(d) {
        let other = if d.is_scylla() { "Cassandra" } else { "ScyllaDB" };
        return Err(format!("{k}: {c} is {other}'s; this database's dialect has no such authenticator"));
    }
    Ok(c)
}

/// ScyllaDB's `auth_certificate_role_queries`: a list of `{source, query}` (`SUBJECT` or
/// `ALTNAME`, a regular expression with a capture group), also as JSON text or one
/// `SOURCE:query` string.
fn role_queries_of(k: &str, v: &Value) -> Result<Vec<nosql_engine::auth::certificate::RoleQuery>, String> {
    use nosql_engine::auth::certificate::RoleQuery;
    let items = match v {
        Value::Array(a) => a.clone(),
        Value::String(s) if s.trim_start().starts_with('[') => serde_json::from_str(s).map_err(|e| format!("{k}: {e}"))?,
        Value::String(s) => {
            let (src, q) = s.split_once(':').ok_or_else(|| format!("{k}: expected SOURCE:query, got {s:?}"))?;
            return Ok(vec![RoleQuery::parse(src, q).map_err(|e| format!("{k}: {e}"))?]);
        }
        _ => return Err(format!("{k}: expected a list of {{source, query}}, got {v}")),
    };
    let mut out = Vec::new();
    for it in &items {
        let (Some(src), Some(q)) = (it.get("source").and_then(Value::as_str), it.get("query").and_then(Value::as_str)) else {
            return Err(format!("{k}: expected {{\"source\": ..., \"query\": ...}}, got {it}"));
        };
        out.push(RoleQuery::parse(src, q).map_err(|e| format!("{k}: {e}"))?);
    }
    if out.is_empty() {
        return Err(format!("{k}: expected at least one query"));
    }
    Ok(out)
}

/// Applies one setting; `Err` names the problem.
fn apply_one(cfg: &mut DbConfig, key: &str, v: &Value) -> Result<(), String> {
    let dialect = cfg.dialect;
    let s: &mut Settings = &mut cfg.settings;
    match key {
        "batch_size_warn_threshold" => s.batch_size_warn_threshold = bytes_of(key, v, 1)?,
        "batch_size_warn_threshold_in_kb" => s.batch_size_warn_threshold = bytes_of(key, v, 1024)?,
        "batch_size_fail_threshold" => s.batch_size_fail_threshold = bytes_of(key, v, 1)?,
        "batch_size_fail_threshold_in_kb" => s.batch_size_fail_threshold = bytes_of(key, v, 1024)?,
        "unlogged_batch_across_partitions_warn_threshold" => s.unlogged_batch_across_partitions_warn_threshold = u64_of(key, v)?,
        "tombstone_warn_threshold" => s.tombstone_warn_threshold = u64_of(key, v)?,
        "tombstone_failure_threshold" => s.tombstone_failure_threshold = u64_of(key, v)?,
        "materialized_views_enabled" | "enable_materialized_views" => s.materialized_views_enabled = bool_of(key, v)?,
        "sasi_indexes_enabled" | "enable_sasi_indexes" => s.sasi_indexes_enabled = bool_of(key, v)?,
        "user_defined_functions_enabled" | "enable_user_defined_functions" => s.user_defined_functions_enabled = bool_of(key, v)?,
        "compact_storage_enabled" | "enable_create_table_with_compact_storage" => s.compact_storage_enabled = bool_of(key, v)?,
        "drop_compact_storage_enabled" => s.drop_compact_storage_enabled = bool_of(key, v)?,
        "max_partition_key_restrictions_per_query" => s.max_partition_key_restrictions_per_query = u64_of(key, v)?,
        "max_clustering_key_restrictions_per_query" => s.max_clustering_key_restrictions_per_query = u64_of(key, v)?,
        "minimum_replication_factor_warn_threshold" => s.minimum_replication_factor_warn_threshold = ranged(key, u64_of(key, v)?, 0, u32::MAX as u64)? as i64,
        "tablets_by_default" | "tablets_mode_for_new_keyspaces" | "enable_tablets" => s.tablets_by_default = bool_of(key, v)?,
        "allow_filtering_enabled" => s.allow_filtering_enabled = bool_of(key, v)?,
        "authenticator" => s.authenticator = authenticator_of(key, v, dialect)?,
        "auth_certificate_role_queries" => s.scylla.auth_certificate_role_queries = role_queries_of(key, v)?,
        "authorizer" => s.authorizer = class_of(key, v, &["AllowAllAuthorizer", "CassandraAuthorizer"])?,
        "network_authorizer" => s.network_authorizer = class_of(key, v, &["AllowAllNetworkAuthorizer", "CassandraNetworkAuthorizer"])?,
        "permissions_validity" | "permissions_validity_in_ms" => s.permissions_validity_ms = ms_of(key, v, 1)?,
        "roles_validity" | "roles_validity_in_ms" => s.roles_validity_ms = ms_of(key, v, 1)?,
        "credentials_validity" | "credentials_validity_in_ms" => s.credentials_validity_ms = ms_of(key, v, 1)?,
        "auth_bcrypt_log2_rounds" | "auth_bcrypt_gensalt_log2_rounds" => s.auth_bcrypt_log2_rounds = ranged(key, u64_of(key, v)?, 4, 30)? as u32,
        "role_password_update_min_interval" | "role_password_update_min_interval_in_ms" => s.role_password_update_min_interval_ms = ms_of(key, v, 1)?,
        "shards" | "smp" => s.shards = ranged(key, u64_of(key, v)?, 1, 256)? as u32,
        "num_tokens" => cfg.num_tokens = ranged(key, u64_of(key, v)?, 1, 1536)? as u32,
        // Storage's ([`memtable_space`]).
        "memtable_space" | "memtable_heap_space" | "memtable_heap_space_in_mb" => {}
        _ => return Err(format!("unknown setting {key} (known: {})", KEYS.join(", "))),
    }
    Ok(())
}

/// Applies `overrides` to `cfg`.
pub fn apply(cfg: &mut DbConfig, overrides: &Map<String, Value>) -> Result<(), String> {
    for (k, v) in overrides {
        apply_one(cfg, k.trim(), v)?;
    }
    memtable_space(overrides)?;
    if cfg.settings.batch_size_fail_threshold < cfg.settings.batch_size_warn_threshold {
        return Err("batch_size_fail_threshold must not be below batch_size_warn_threshold".into());
    }
    if cfg.settings.tombstone_failure_threshold < cfg.settings.tombstone_warn_threshold {
        return Err("tombstone_failure_threshold must not be below tombstone_warn_threshold".into());
    }
    Ok(())
}

/// Merges a PATCH's `settings` into the stored overrides: `null` removes a key.
pub fn merge(stored: &Map<String, Value>, patch: &Map<String, Value>) -> Map<String, Value> {
    let mut out = stored.clone();
    for (k, v) in patch {
        if v.is_null() {
            out.remove(k);
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// The effective settings of a dialect's defaults plus overrides, as JSON (for `GET /v1/nosql`).
pub fn effective(cfg: &DbConfig) -> Value {
    let s = &cfg.settings;
    serde_json::json!({
        "batch_size_warn_threshold": s.batch_size_warn_threshold,
        "batch_size_fail_threshold": s.batch_size_fail_threshold,
        "unlogged_batch_across_partitions_warn_threshold": s.unlogged_batch_across_partitions_warn_threshold,
        "tombstone_warn_threshold": s.tombstone_warn_threshold,
        "tombstone_failure_threshold": s.tombstone_failure_threshold,
        "materialized_views_enabled": s.materialized_views_enabled,
        "sasi_indexes_enabled": s.sasi_indexes_enabled,
        "user_defined_functions_enabled": s.user_defined_functions_enabled,
        "compact_storage_enabled": s.compact_storage_enabled,
        "drop_compact_storage_enabled": s.drop_compact_storage_enabled,
        "max_partition_key_restrictions_per_query": s.max_partition_key_restrictions_per_query,
        "max_clustering_key_restrictions_per_query": s.max_clustering_key_restrictions_per_query,
        "minimum_replication_factor_warn_threshold": s.minimum_replication_factor_warn_threshold,
        "tablets_by_default": s.tablets_by_default,
        "allow_filtering_enabled": s.allow_filtering_enabled,
        "authenticator": s.authenticator,
        "auth_certificate_role_queries": s.scylla.auth_certificate_role_queries.iter().map(|q| serde_json::json!({
            "source": match q.source { nosql_engine::auth::certificate::Source::Subject => "SUBJECT", nosql_engine::auth::certificate::Source::AltName => "ALTNAME" },
            "query": q.query,
        })).collect::<Vec<_>>(),
        "authorizer": s.authorizer,
        "network_authorizer": s.network_authorizer,
        "permissions_validity": s.permissions_validity_ms,
        "roles_validity": s.roles_validity_ms,
        "credentials_validity": s.credentials_validity_ms,
        "auth_bcrypt_log2_rounds": s.auth_bcrypt_log2_rounds,
        "role_password_update_min_interval": s.role_password_update_min_interval_ms,
        "shards": s.shards,
        "num_tokens": cfg.num_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosql_engine::Dialect;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn spellings_and_units() {
        let mut c = DbConfig::new("n", Dialect::Cassandra);
        apply(
            &mut c,
            &obj(json!({
                "batch_size_warn_threshold_in_kb": 10,
                "batch_size_fail_threshold": "1MiB",
                "enable_user_defined_functions": "true",
                "tombstone_warn_threshold": "5",
                "authenticator": "org.apache.cassandra.auth.PasswordAuthenticator",
                "smp": 4,
                "num_tokens": 8,
                "permissions_validity": "2s",
            })),
        )
        .unwrap();
        assert_eq!(c.settings.batch_size_warn_threshold, 10 * 1024);
        assert_eq!(c.settings.batch_size_fail_threshold, 1 << 20);
        assert!(c.settings.user_defined_functions_enabled);
        assert_eq!(c.settings.tombstone_warn_threshold, 5);
        assert_eq!(c.settings.authenticator, "PasswordAuthenticator");
        assert_eq!(c.settings.shards, 4);
        assert_eq!(c.num_tokens, 8);
        assert_eq!(c.settings.permissions_validity_ms, 2000);
        assert_eq!(effective(&c)["num_tokens"], 8);
    }

    #[test]
    fn memtable_space_spellings() {
        let space = |v: Value| memtable_space(&obj(v));
        assert_eq!(space(json!({})), Ok(None));
        assert_eq!(space(json!({"memtable_space": "64MiB"})), Ok(Some(64 << 20)));
        assert_eq!(space(json!({"memtable_space": 2 << 20})), Ok(Some(2 << 20)));
        assert_eq!(space(json!({"memtable_heap_space": "1GiB"})), Ok(Some(1 << 30)));
        assert_eq!(space(json!({"memtable_heap_space_in_mb": 32})), Ok(Some(32 << 20)));
        assert_eq!(space(json!({"memtable_space": "100KiB"})), Err("memtable_space must be at least 1MiB".to_string()));
        // Validated with the other settings, and not an engine setting.
        let mut c = DbConfig::new("n", Dialect::Scylla);
        let before = c.clone();
        apply(&mut c, &obj(json!({"memtable_space": "16MiB"}))).unwrap();
        assert_eq!(c, before);
        assert!(apply(&mut c, &obj(json!({"memtable_space": "nope"}))).unwrap_err().contains("memtable_space"));
    }

    #[test]
    fn rejects() {
        let bad = |v: Value| {
            let mut c = DbConfig::new("n", Dialect::Scylla);
            apply(&mut c, &obj(v)).unwrap_err()
        };
        assert!(bad(json!({"nope": 1})).starts_with("unknown setting nope"));
        assert_eq!(bad(json!({"tombstone_warn_threshold": -1})), "tombstone_warn_threshold: expected a non-negative integer, got -1");
        assert_eq!(bad(json!({"materialized_views_enabled": "maybe"})), "materialized_views_enabled: expected true or false, got \"maybe\"");
        assert!(bad(json!({"authenticator": "LdapAuthenticator"})).starts_with("authenticator: unknown class"));
        assert_eq!(bad(json!({"shards": 0})), "shards must be between 1 and 256");
        assert!(bad(json!({"batch_size_fail_threshold": "1XB"})).contains("bad size unit"));
        assert_eq!(bad(json!({"batch_size_fail_threshold": 1, "batch_size_warn_threshold": 2})), "batch_size_fail_threshold must not be below batch_size_warn_threshold");
        assert_eq!(bad(json!({"tombstone_failure_threshold": 1})), "tombstone_failure_threshold must not be below tombstone_warn_threshold");
    }

    #[test]
    fn certificate_authenticators() {
        use nosql_engine::auth::certificate::{RoleQuery, Source};
        let set = |d: Dialect, v: Value| {
            let mut c = DbConfig::new("n", d);
            apply(&mut c, &obj(v)).map(|_| c)
        };
        let c = set(Dialect::Cassandra, json!({"authenticator": "org.apache.cassandra.auth.MutualTlsAuthenticator"})).unwrap();
        assert_eq!(c.settings.authenticator, "MutualTlsAuthenticator");
        let c = set(Dialect::Cassandra, json!({"authenticator": "MutualTlsWithPasswordFallbackAuthenticator"})).unwrap();
        assert_eq!(c.settings.authenticator, "MutualTlsWithPasswordFallbackAuthenticator");
        let c = set(Dialect::Scylla, json!({"authenticator": "com.scylladb.auth.CertificateAuthenticator"})).unwrap();
        assert_eq!(c.settings.authenticator, "CertificateAuthenticator");
        assert_eq!(c.settings.scylla.auth_certificate_role_queries, RoleQuery::defaults());
        assert_eq!(
            set(Dialect::Scylla, json!({"authenticator": "MutualTlsAuthenticator"})).unwrap_err(),
            "authenticator: MutualTlsAuthenticator is Cassandra's; this database's dialect has no such authenticator"
        );
        assert_eq!(
            set(Dialect::Cassandra, json!({"authenticator": "CertificateOrPasswordAuthenticator"})).unwrap_err(),
            "authenticator: CertificateOrPasswordAuthenticator is ScyllaDB's; this database's dialect has no such authenticator"
        );
        // Role queries: a list, its JSON text, or one SOURCE:query.
        let q = |v: Value| set(Dialect::Scylla, json!({ "auth_certificate_role_queries": v })).map(|c| c.settings.scylla.auth_certificate_role_queries);
        let two = vec![
            RoleQuery { source: Source::AltName, query: "EMAIL=([^@,]+)@".into() },
            RoleQuery { source: Source::Subject, query: "CN=([^,\\s]+)".into() },
        ];
        assert_eq!(q(json!([{"source": "ALTNAME", "query": "EMAIL=([^@,]+)@"}, {"source": "subject", "query": "CN=([^,\\s]+)"}])).unwrap(), two);
        assert_eq!(q(json!(r#"[{"source": "ALTNAME", "query": "EMAIL=([^@,]+)@"}, {"source": "SUBJECT", "query": "CN=([^,\\s]+)"}]"#)).unwrap(), two);
        assert_eq!(q(json!("SUBJECT:OU=([^,]+)")).unwrap(), vec![RoleQuery { source: Source::Subject, query: "OU=([^,]+)".into() }]);
        assert_eq!(q(json!("issuer:CN=(x)")).unwrap_err(), "auth_certificate_role_queries: unknown certificate source \"ISSUER\" (SUBJECT or ALTNAME)");
        assert_eq!(q(json!("nocolon")).unwrap_err(), "auth_certificate_role_queries: expected SOURCE:query, got \"nocolon\"");
        assert_eq!(q(json!([{"source": "SUBJECT", "query": "CN=.*"}])).unwrap_err(), "auth_certificate_role_queries: role query \"CN=.*\" has no capture group");
        assert!(q(json!([{"query": "x"}])).unwrap_err().starts_with("auth_certificate_role_queries: expected {\"source\""));
        assert_eq!(q(json!([])).unwrap_err(), "auth_certificate_role_queries: expected at least one query");
        assert!(q(json!(3)).unwrap_err().starts_with("auth_certificate_role_queries: expected a list"));
    }

    #[test]
    fn merging() {
        let stored = obj(json!({"a": 1, "b": 2}));
        let m = merge(&stored, &obj(json!({"a": null, "c": 3})));
        assert_eq!(Value::Object(m), json!({"b": 2, "c": 3}));
    }
}
