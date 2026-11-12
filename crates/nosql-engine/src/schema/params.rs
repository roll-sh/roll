//! Table options (`WITH …`): defaults, parsing, validation and normalisation per dialect.
//!
//! Cassandra's side follows `cql3.statements.schema.TableAttributes`, `PropertyDefinitions`,
//! `schema.TableParams`, `CompactionParams`, `CompressionParams` and `CachingParams`
//! (Apache-2.0). ScyllaDB's side (defaults, which options exist, the error texts, how maps are
//! normalised) was collected from the reference image.

use std::collections::BTreeMap;

use bytes::Bytes;
use nosql_cql::ast::{LiteralKind, Property, PropertyValue};
use nosql_types::Dialect;

use crate::error::{Error, Result};
use crate::messages as msg;

/// Every table option both references store, with the dialect's representation.
#[derive(Clone, Debug, PartialEq)]
pub struct TableParams {
    pub bloom_filter_fp_chance: f64,
    pub caching: BTreeMap<String, String>,
    pub comment: String,
    pub compaction: BTreeMap<String, String>,
    pub compression: BTreeMap<String, String>,
    pub crc_check_chance: f64,
    pub default_time_to_live: i32,
    pub gc_grace_seconds: i32,
    pub max_index_interval: i32,
    pub min_index_interval: i32,
    pub memtable_flush_period_in_ms: i32,
    pub speculative_retry: String,
    /// Cassandra only.
    pub additional_write_policy: String,
    /// Cassandra only (`BLOCKING` / `NONE`).
    pub read_repair: String,
    /// Cassandra: written only when not default.
    pub memtable: Option<String>,
    pub allow_auto_snapshot: Option<bool>,
    pub incremental_backups: Option<bool>,
    /// Cassandra `cdc` (stored only when CDC is enabled in the yaml, which it isn't by default).
    pub cdc: Option<bool>,
    /// `extensions` (ScyllaDB keeps `tombstone_gc`, `cdc`, `paxos_grace_seconds`, … here).
    pub extensions: BTreeMap<String, Bytes>,
    /// ScyllaDB `tombstone_gc` options.
    pub tombstone_gc: BTreeMap<String, String>,
    /// ScyllaDB `cdc` options (`enabled`, `preimage`, …), empty when never set.
    pub scylla_cdc: BTreeMap<String, String>,
    /// ScyllaDB `paxos_grace_seconds`.
    pub paxos_grace_seconds: Option<i32>,
    /// ScyllaDB `per_partition_rate_limit`.
    pub per_partition_rate_limit: BTreeMap<String, String>,
    /// ScyllaDB `large_data_guardrails_enabled` (`system_schema.scylla_tables`; only `true`
    /// is stored, and DESCRIBE doesn't show it).
    pub large_data_guardrails: bool,
    /// ScyllaDB's per-table tablet hints (`tablets = {'min_tablet_count': …}`), as given
    /// (`system_schema.scylla_tables.tablets`).
    pub tablets: BTreeMap<String, String>,
}

impl Default for TableParams {
    fn default() -> Self {
        TableParams::defaults(Dialect::Scylla, false)
    }
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

const CASSANDRA_COMPRESS_PKG: &str = "org.apache.cassandra.io.compress.";

impl TableParams {
    /// A new table's options. `system` tables differ in a few defaults.
    pub fn defaults(dialect: Dialect, system: bool) -> Self {
        match dialect {
            Dialect::Scylla => TableParams {
                bloom_filter_fp_chance: 0.01,
                caching: map(&[("keys", "ALL"), ("rows_per_partition", "ALL")]),
                comment: String::new(),
                compaction: map(&[("class", "IncrementalCompactionStrategy")]),
                compression: if system {
                    map(&[(
                        "sstable_compression",
                        "org.apache.cassandra.io.compress.LZ4Compressor",
                    )])
                } else {
                    map(&[("sstable_compression", "LZ4WithDictsCompressor")])
                },
                crc_check_chance: 1.0,
                default_time_to_live: 0,
                gc_grace_seconds: 864_000,
                max_index_interval: 2048,
                min_index_interval: 128,
                memtable_flush_period_in_ms: 0,
                speculative_retry: "99.0PERCENTILE".into(),
                additional_write_policy: "99p".into(),
                read_repair: "BLOCKING".into(),
                memtable: None,
                allow_auto_snapshot: None,
                incremental_backups: None,
                cdc: None,
                extensions: BTreeMap::new(),
                tombstone_gc: if system {
                    BTreeMap::new()
                } else {
                    map(&[("mode", "repair"), ("propagation_delay_in_seconds", "3600")])
                },
                scylla_cdc: BTreeMap::new(),
                paxos_grace_seconds: None,
                per_partition_rate_limit: BTreeMap::new(),
                large_data_guardrails: false,
                tablets: BTreeMap::new(),
            },
            Dialect::Cassandra => TableParams {
                bloom_filter_fp_chance: 0.01,
                caching: map(&[("keys", "ALL"), ("rows_per_partition", "NONE")]),
                comment: String::new(),
                compaction: map(&[
                    (
                        "class",
                        "org.apache.cassandra.db.compaction.SizeTieredCompactionStrategy",
                    ),
                    ("max_threshold", "32"),
                    ("min_threshold", "4"),
                ]),
                compression: map(&[
                    ("chunk_length_in_kb", "16"),
                    ("class", "org.apache.cassandra.io.compress.LZ4Compressor"),
                ]),
                crc_check_chance: 1.0,
                default_time_to_live: 0,
                gc_grace_seconds: 864_000,
                max_index_interval: 2048,
                min_index_interval: 128,
                memtable_flush_period_in_ms: 0,
                speculative_retry: "99p".into(),
                additional_write_policy: "99p".into(),
                read_repair: "BLOCKING".into(),
                memtable: None,
                allow_auto_snapshot: None,
                incremental_backups: None,
                cdc: None,
                extensions: BTreeMap::new(),
                tombstone_gc: BTreeMap::new(),
                scylla_cdc: BTreeMap::new(),
                paxos_grace_seconds: None,
                per_partition_rate_limit: BTreeMap::new(),
                large_data_guardrails: false,
                tablets: BTreeMap::new(),
            },
        }
    }

    /// The `extensions` map as stored (ScyllaDB folds several options into it).
    pub fn extensions_map(&self, dialect: Dialect) -> BTreeMap<String, Bytes> {
        let mut out = self.extensions.clone();
        if dialect.is_scylla() {
            if !self.tombstone_gc.is_empty() {
                out.insert("tombstone_gc".into(), scylla_map_blob(&self.tombstone_gc));
            }
            if !self.scylla_cdc.is_empty() {
                out.insert("cdc".into(), scylla_map_blob(&self.scylla_cdc));
            }
            if let Some(p) = self.paxos_grace_seconds {
                out.insert(
                    "paxos_grace_seconds".into(),
                    Bytes::copy_from_slice(&p.to_le_bytes()),
                );
            }
        }
        out
    }

    /// Restores the options ScyllaDB keeps in `extensions` (inverse of [`Self::extensions_map`]).
    pub fn absorb_extensions(&mut self, dialect: Dialect) {
        if !dialect.is_scylla() {
            return;
        }
        if let Some(b) = self.extensions.remove("tombstone_gc") {
            self.tombstone_gc = parse_scylla_map_blob(&b).unwrap_or_default();
        }
        if let Some(b) = self.extensions.remove("cdc") {
            self.scylla_cdc = parse_scylla_map_blob(&b).unwrap_or_default();
        }
        if let Some(b) = self.extensions.remove("paxos_grace_seconds")
            && let Ok(a) = <[u8; 4]>::try_from(&b[..])
        {
            self.paxos_grace_seconds = Some(i32::from_le_bytes(a));
        }
    }

    /// The storage tombstone GC policy.
    pub fn gc_policy(&self) -> nosql_storage::options::GcPolicy {
        use nosql_storage::options::TombstoneGcMode as M;
        let mode = match self.tombstone_gc.get("mode").map(String::as_str) {
            Some("disabled") => M::Disabled,
            Some("immediate") => M::Immediate,
            Some("repair") => M::Repair,
            _ => M::Timeout,
        };
        nosql_storage::options::GcPolicy {
            mode,
            gc_grace_seconds: self.gc_grace_seconds.max(0) as i64,
        }
    }

    /// The compaction parameters storage runs (class name without package).
    pub fn compaction_params(&self) -> nosql_storage::compaction::CompactionParams {
        let class = self.compaction.get("class").cloned().unwrap_or_default();
        let short = class.rsplit('.').next().unwrap_or(&class).to_string();
        nosql_storage::compaction::CompactionParams {
            class: short,
            options: self
                .compaction
                .iter()
                .filter(|(k, _)| k.as_str() != "class")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }
}

/// ScyllaDB's serialisation of a `map<text, text>` extension: little-endian 32-bit count, then
/// each key and value as a little-endian 32-bit length and its bytes.
pub fn scylla_map_blob(m: &BTreeMap<String, String>) -> Bytes {
    let mut out = Vec::new();
    out.extend_from_slice(&(m.len() as u32).to_le_bytes());
    for (k, v) in m {
        out.extend_from_slice(&(k.len() as u32).to_le_bytes());
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        out.extend_from_slice(v.as_bytes());
    }
    Bytes::from(out)
}

pub fn parse_scylla_map_blob(b: &[u8]) -> Option<BTreeMap<String, String>> {
    let mut r = b;
    let mut take = |n: usize| -> Option<&[u8]> {
        if r.len() < n {
            return None;
        }
        let (h, t) = r.split_at(n);
        r = t;
        Some(h)
    };
    let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let mut out = BTreeMap::new();
    for _ in 0..n {
        let kl = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let k = String::from_utf8(take(kl)?.to_vec()).ok()?;
        let vl = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let v = String::from_utf8(take(vl)?.to_vec()).ok()?;
        out.insert(k, v);
    }
    Some(out)
}

/// The `WITH` properties of a statement, checked for duplicates and unknown names.
pub struct Properties<'p, 'a> {
    pub props: Vec<&'p Property<'a>>,
    dialect: Dialect,
}

impl<'p, 'a> Properties<'p, 'a> {
    /// Collects properties, rejecting duplicates and names not in `valid`.
    pub fn new(
        props: impl IntoIterator<Item = &'p Property<'a>>,
        valid: &[&str],
        dialect: Dialect,
    ) -> Result<Self> {
        let props: Vec<&Property> = props.into_iter().collect();
        let mut seen: Vec<String> = Vec::new();
        for p in &props {
            let name = p.name.name().to_string();
            if seen.contains(&name) {
                return Err(Error::syntax(msg::multiple_property_definitions(
                    dialect, &name,
                )));
            }
            seen.push(name);
        }
        // ScyllaDB validates its property map, which is sorted by name.
        let mut order: Vec<String> = props.iter().map(|p| p.name.name().to_string()).collect();
        if dialect.is_scylla() {
            order.sort();
        }
        if let Some(name) = order.iter().find(|n| !valid.contains(&n.as_str())) {
            return Err(Error::syntax(format!("Unknown property '{name}'")));
        }
        Ok(Properties { props, dialect })
    }

    pub fn get(&self, name: &str) -> Option<&PropertyValue<'a>> {
        self.props
            .iter()
            .find(|p| p.name.name() == name)
            .map(|p| &p.value)
    }

    pub fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// A simple value's text.
    pub fn string(&self, name: &str) -> Result<Option<String>> {
        match self.get(name) {
            None => Ok(None),
            Some(PropertyValue::Simple(l)) => Ok(Some(l.text.to_string())),
            Some(PropertyValue::Map(_)) => Err(Error::syntax(format!(
                "Invalid value for property '{name}'. It should be a string"
            ))),
        }
    }

    /// A map value (keys and values as their text). ScyllaDB's list values are rack lists of
    /// `replication` (marked, `crate::schema::replication::rack_list`) and refused elsewhere.
    pub fn map(&self, name: &str) -> Result<Option<BTreeMap<String, String>>> {
        match self.get(name) {
            None => Ok(None),
            Some(PropertyValue::Map(m)) => {
                let mut out = BTreeMap::new();
                for (k, v) in m {
                    let text = if v.kind == nosql_cql::ast::LiteralKind::List {
                        if name != "replication" {
                            let items: Vec<String> =
                                v.list_items().iter().map(|i| format!("\"{i}\"")).collect();
                            return Err(Error::syntax(format!(
                                "Invalid map value '[{}]' for key '{}'. It should be a simple string.",
                                items.join(", "),
                                k.text
                            )));
                        }
                        format!("{}{}", super::replication::LIST_MARK, v.text)
                    } else {
                        v.text.to_string()
                    };
                    out.insert(k.text.to_string(), text);
                }
                Ok(Some(out))
            }
            Some(PropertyValue::Simple(_)) => Err(Error::syntax(format!(
                "Invalid value for property '{name}'. It should be a map."
            ))),
        }
    }

    /// Like [`Self::map`], keeping each value's literal kind (ScyllaDB `cdc = {'enabled': true}`).
    pub fn raw_map(&self, name: &str) -> Option<Vec<(String, String, LiteralKind)>> {
        match self.get(name) {
            Some(PropertyValue::Map(m)) => Some(
                m.iter()
                    .map(|(k, v)| (k.text.to_string(), v.text.to_string(), v.kind))
                    .collect(),
            ),
            _ => None,
        }
    }

    pub fn int(&self, name: &str) -> Result<Option<i32>> {
        match self.string(name)? {
            None => Ok(None),
            Some(s) => s
                .trim()
                .parse::<i32>()
                .map(Some)
                .map_err(|_| Error::syntax(format!("Invalid integer value {s} for '{name}'"))),
        }
    }

    pub fn double(&self, name: &str) -> Result<Option<f64>> {
        match self.string(name)? {
            None => Ok(None),
            Some(s) => nosql_types::float::parse_java_double(s.trim())
                .map(Some)
                .ok_or_else(|| Error::syntax(format!("Invalid double value {s} for '{name}'"))),
        }
    }

    /// `PropertyDefinitions.parseBoolean`.
    pub fn boolean(&self, name: &str) -> Result<Option<bool>> {
        match self.string(name)? {
            None => Ok(None),
            Some(s) => parse_bool(name, &s).map(Some),
        }
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }
}

pub fn parse_bool(name: &str, s: &str) -> Result<bool> {
    match s.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Ok(true),
        "0" | "false" | "no" => Ok(false),
        _ => Err(Error::syntax(format!(
            "Invalid boolean value {s} for '{name}'. Positive values can be '1', 'true' or 'yes'. Negative values can be '0', 'false' or 'no'."
        ))),
    }
}

/// Table option names each dialect accepts (plus `id`).
pub fn valid_table_options(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::Cassandra => &[
            "allow_auto_snapshot",
            "bloom_filter_fp_chance",
            "caching",
            "comment",
            "compaction",
            "compression",
            "memtable",
            "default_time_to_live",
            "extensions",
            "gc_grace_seconds",
            "incremental_backups",
            "max_index_interval",
            "memtable_flush_period_in_ms",
            "min_index_interval",
            "speculative_retry",
            "additional_write_policy",
            "crc_check_chance",
            "cdc",
            "read_repair",
            "auto_repair",
            "id",
        ],
        Dialect::Scylla => &[
            "bloom_filter_fp_chance",
            "caching",
            "comment",
            "compaction",
            "compression",
            "crc_check_chance",
            "dclocal_read_repair_chance",
            "read_repair_chance",
            "default_time_to_live",
            "gc_grace_seconds",
            "max_index_interval",
            "min_index_interval",
            "memtable_flush_period_in_ms",
            "speculative_retry",
            "paxos_grace_seconds",
            "tombstone_gc",
            "cdc",
            "per_partition_rate_limit",
            "synchronous_updates",
            "tablets",
            "caching",
            "large_data_guardrails_enabled",
            "scylla_tags",
            "id",
        ],
    }
}

/// ScyllaDB's `synchronous_updates` of a view: the `system:synchronous_view_updates` tag of
/// `scylla_tags`, `true` for `true` (any case) or `1`, `false` for anything else.
pub fn set_synchronous_updates(params: &mut TableParams, value: &str) {
    let on = value.eq_ignore_ascii_case("true") || value == "1";
    let mut tags = params
        .extensions
        .get("scylla_tags")
        .and_then(|b| parse_scylla_map_blob(b))
        .unwrap_or_default();
    tags.insert(
        "system:synchronous_view_updates".to_string(),
        on.to_string(),
    );
    params
        .extensions
        .insert("scylla_tags".to_string(), scylla_map_blob(&tags));
}

/// Options that apply only to a materialized view (rejected on base tables).
fn check_view_only(p: &Properties, is_view: bool) -> Result<()> {
    if p.dialect().is_scylla() && p.has("synchronous_updates") && !is_view {
        return Err(Error::invalid(
            "The synchronous_updates option is only applicable to materialized views, not to base tables",
        ));
    }
    Ok(())
}

/// Applies `WITH` properties to `params` (new table or ALTER), validating as the dialect does.
pub fn apply(params: &mut TableParams, p: &Properties, is_view: bool) -> Result<()> {
    let d = p.dialect();
    check_view_only(p, is_view)?;
    // ScyllaDB's `scylla_tags` (as DESCRIBE prints them): the tag map, replaced whole.
    if d.is_scylla() {
        match p.get("scylla_tags") {
            Some(PropertyValue::Simple(_)) => {
                return Err(Error::server("Cannot create tags from string"));
            }
            Some(PropertyValue::Map(_)) => {
                let tags = p.map("scylla_tags")?.unwrap_or_default();
                params
                    .extensions
                    .insert("scylla_tags".to_string(), scylla_map_blob(&tags));
            }
            None => {}
        }
    }
    if d.is_scylla()
        && let Some(v) = p.string("synchronous_updates")?
    {
        set_synchronous_updates(params, &v);
    }
    if let Some(v) = p.double("bloom_filter_fp_chance")? {
        params.bloom_filter_fp_chance = v;
    }
    if let Some(m) = p.map("caching")? {
        params.caching = caching(d, m)?;
    }
    if let Some(s) = p.string("comment")? {
        params.comment = s;
    }
    if let Some(m) = p.map("compaction")? {
        params.compaction = super::compaction_options::compaction(d, m)?;
    }
    if let Some(m) = p.map("compression")? {
        params.compression = compression(d, m)?;
    }
    // ScyllaDB accepts but ignores crc_check_chance (it keeps reporting 1.0).
    if let Some(v) = p.double("crc_check_chance")?
        && !d.is_scylla()
    {
        params.crc_check_chance = v;
    }
    if let Some(v) = p.int("default_time_to_live")? {
        params.default_time_to_live = v;
    }
    if let Some(v) = p.int("gc_grace_seconds")? {
        params.gc_grace_seconds = v;
    }
    if let Some(v) = p.int("max_index_interval")? {
        params.max_index_interval = v;
        // Both references compare it with the statement's minimum (or the default), not the
        // table's.
        let min = p.int("min_index_interval")?.unwrap_or(128);
        if v < min && min >= 1 {
            return Err(Error::config(match d {
                Dialect::Scylla => "max_index_interval must be greater than min_index_interval".to_string(),
                Dialect::Cassandra => format!(
                    "max_index_interval must be greater than or equal to min_index_interval ({min}) (got {v})"
                ),
            }));
        }
    }
    if let Some(v) = p.int("min_index_interval")? {
        params.min_index_interval = v;
    }
    if let Some(v) = p.int("memtable_flush_period_in_ms")? {
        params.memtable_flush_period_in_ms = v;
    }
    if let Some(s) = p.string("speculative_retry")? {
        params.speculative_retry = speculative_retry(d, &s, "speculative_retry")?;
    }
    match d {
        Dialect::Cassandra => {
            if let Some(s) = p.string("additional_write_policy")? {
                params.additional_write_policy =
                    speculative_retry(d, &s, "additional_write_policy")?;
            }
            if let Some(s) = p.string("read_repair")? {
                params.read_repair = match s.to_ascii_uppercase().as_str() {
                    "BLOCKING" => "BLOCKING".into(),
                    "NONE" => "NONE".into(),
                    _ => {
                        return Err(Error::config(format!(
                            "Invalid value '{s}' for option 'read_repair'"
                        )));
                    }
                };
            }
            if let Some(s) = p.string("memtable")? {
                params.memtable = if s == "default" { None } else { Some(s) };
            }
            if let Some(b) = p.boolean("allow_auto_snapshot")? {
                params.allow_auto_snapshot = if b { None } else { Some(false) };
            }
            if let Some(b) = p.boolean("incremental_backups")? {
                params.incremental_backups = if b { None } else { Some(false) };
            }
            if p.has("cdc") {
                // Validated (it must be a boolean string) but not stored: CDC is disabled.
                if let Some(s) = p.string("cdc")? {
                    parse_bool("cdc", &s)?;
                }
            }
        }
        Dialect::Scylla => {
            if let Some(m) = p.map("tombstone_gc")? {
                let mode = m.get("mode").cloned().unwrap_or_else(|| "timeout".into());
                if !["timeout", "disabled", "immediate", "repair"].contains(&mode.as_str()) {
                    return Err(Error::config(format!("Invalid tombstone_gc mode: {mode}")));
                }
                let mut tgc = BTreeMap::new();
                tgc.insert("mode".to_string(), mode);
                tgc.insert(
                    "propagation_delay_in_seconds".to_string(),
                    m.get("propagation_delay_in_seconds")
                        .cloned()
                        .unwrap_or_else(|| "3600".into()),
                );
                params.tombstone_gc = tgc;
            }
            if p.has("cdc") {
                match p.raw_map("cdc") {
                    Some(entries) => {
                        let mut cdc = map(&[
                            ("delta", "full"),
                            ("enabled", "false"),
                            ("postimage", "false"),
                            ("preimage", "false"),
                            ("ttl", "86400"),
                        ]);
                        let mut given = BTreeMap::new();
                        for (k, v, _) in entries {
                            given.insert(k, v.to_ascii_lowercase());
                        }
                        crate::cdc::validate_options(&given)?;
                        cdc.extend(given);
                        params.scylla_cdc = cdc;
                    }
                    None => return Err(Error::server("Cannot create cdc info from string")),
                }
            }
            if let Some(v) = p.int("paxos_grace_seconds")? {
                params.paxos_grace_seconds = Some(v);
            }
            if let Some(m) = p.map("per_partition_rate_limit")? {
                params.per_partition_rate_limit = m;
            }
            if let Some(m) = p.map("tablets")? {
                params.tablets = tablet_hints(m)?;
            }
            // Any string: `true`, `yes` and `1` enable the guardrails, everything else disables
            // them.
            if let Some(v) = p.string("large_data_guardrails_enabled")? {
                params.large_data_guardrails =
                    matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1");
            }
        }
    }
    validate(params, d)
}

/// ScyllaDB's table `tablets` hints: known names only, no negative numbers (other values are
/// kept as given).
fn tablet_hints(m: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    for (k, v) in &m {
        if !matches!(
            k.as_str(),
            "min_tablet_count" | "min_per_shard_tablet_count" | "expected_data_size_in_gb" | "max_tablet_count"
        ) {
            return Err(Error::syntax(format!("Unknown tablet hint '{k}'")));
        }
        if v.trim().parse::<f64>().is_ok_and(|n| n < 0.0) {
            return Err(Error::config(format!("Invalid value '{v}' for {k}")));
        }
    }
    Ok(m)
}

/// The hints DESCRIBE shows (zeroes are no hint).
pub fn shown_tablet_hints(m: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    m.iter()
        .filter(|(_, v)| !matches!(v.trim().parse::<f64>(), Ok(n) if n == 0.0))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn caching(d: Dialect, m: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    let keys = m.get("keys").cloned();
    let rows = m.get("rows_per_partition").cloned();
    if d.is_scylla() {
        // `caching_options::from_map`: the entries in key order; `ALL` / `NONE` as written,
        // any integer for rows; `enabled` is ScyllaDB's own.
        for (k, v) in &m {
            match k.as_str() {
                "keys" if v != "ALL" && v != "NONE" => {
                    return Err(Error::config(format!("Invalid key value: {v}")));
                }
                "rows_per_partition" if v != "ALL" && v != "NONE" && v.parse::<i64>().is_err() => {
                    return Err(Error::config(format!("Invalid key value: {v}")));
                }
                "keys" | "rows_per_partition" | "enabled" => {}
                _ => return Err(Error::config(format!("Invalid caching option: {k}"))),
            }
        }
        let _ = (&keys, &rows);
        for (k, v) in m {
            // `'enabled': true` is the default, and not kept.
            if k == "enabled" && v.eq_ignore_ascii_case("true") {
                continue;
            }
            out.insert(k, v);
        }
        out.entry("keys".into()).or_insert_with(|| "ALL".into());
        out.entry("rows_per_partition".into())
            .or_insert_with(|| "ALL".into());
        return Ok(out);
    }
    let extra: Vec<&String> = m
        .keys()
        .filter(|k| *k != "keys" && *k != "rows_per_partition")
        .collect();
    if !extra.is_empty() {
        let names: Vec<&str> = extra.iter().map(|s| s.as_str()).collect();
        return Err(Error::config(format!(
            "Invalid caching sub-options [{}]: only 'keys' and 'rows_per_partition' are allowed",
            names.join(", ")
        )));
    }
    let k = match keys {
        None => "NONE".to_string(),
        Some(k) if k.eq_ignore_ascii_case("ALL") => "ALL".into(),
        Some(k) if k.eq_ignore_ascii_case("NONE") => "NONE".into(),
        Some(k) => {
            return Err(Error::config(format!(
                "Invalid value '{k}' for caching sub-option 'keys': only 'ALL' and 'NONE' are allowed"
            )));
        }
    };
    let r = match rows {
        None => "NONE".to_string(),
        Some(r) if r.eq_ignore_ascii_case("ALL") => "ALL".into(),
        Some(r) if r.eq_ignore_ascii_case("NONE") => "NONE".into(),
        Some(r) if !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()) => match r.parse::<i32>()
        {
            Ok(0) => "NONE".into(),
            Ok(i32::MAX) => "ALL".into(),
            Ok(n) => n.to_string(),
            Err(_) => return Err(Error::server(format!("For input string: \"{r}\""))),
        },
        Some(r) => {
            return Err(Error::config(format!(
                "Invalid value '{r}' for caching sub-option 'rows_per_partition': only 'ALL', 'NONE', and integer values are allowed"
            )));
        }
    };
    out.insert("keys".into(), k);
    out.insert("rows_per_partition".into(), r);
    Ok(out)
}

const KNOWN_COMPRESSORS: &[&str] = &[
    "LZ4Compressor",
    "SnappyCompressor",
    "DeflateCompressor",
    "ZstdCompressor",
    "NoopCompressor",
];

fn compression(d: Dialect, mut m: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    match d {
        Dialect::Scylla => {
            // `compression = {}`: no compression.
            if m.is_empty() {
                return Ok(BTreeMap::new());
            }
            let Some(class) = m.remove("sstable_compression") else {
                return Err(Error::config(
                    "Missing sub-option 'sstable_compression' for the 'compression' option.",
                ));
            };
            let short = class.rsplit('.').next().unwrap_or(&class).to_string();
            let full = match short.as_str() {
                "" => return Ok(BTreeMap::new()),
                "LZ4WithDictsCompressor" | "ZstdWithDictsCompressor" => short.clone(),
                s if KNOWN_COMPRESSORS.contains(&s) => format!("{CASSANDRA_COMPRESS_PKG}{s}"),
                _ => return Err(Error::server(format!("Unknown sstable_compression: {class}"))),
            };
            let zstd = short.starts_with("Zstd");
            if let Some(k) = m.keys().find(|k| {
                !matches!(k.as_str(), "chunk_length_in_kb" | "chunk_length_kb" | "crc_check_chance")
                    && !(zstd && k.as_str() == "compression_level")
            }) {
                return Err(Error::config(format!("Unknown compression option '{k}'.")));
            }
            let mut out = BTreeMap::new();
            let chunk = m.get("chunk_length_in_kb").or_else(|| m.get("chunk_length_kb"));
            if let Some(v) = chunk {
                // An integer (a fraction is truncated).
                let n = v
                    .trim()
                    .parse::<i64>()
                    .ok()
                    .or_else(|| v.trim().parse::<f64>().ok().filter(|f| f.is_finite()).map(|f| f as i64))
                    .ok_or_else(|| {
                        Error::syntax(format!("Invalid integer value {v} for chunk_length_in_kb"))
                    })?;
                if n <= 0 {
                    return Err(Error::config(
                        "Invalid negative or null for chunk_length_in_kb/chunk_length_kb",
                    ));
                }
                if n & (n - 1) != 0 {
                    return Err(Error::config(
                        "chunk_length_in_kb/chunk_length_kb must be a power of 2.",
                    ));
                }
                if n > 128 {
                    return Err(Error::config(
                        "chunk_length_in_kb/chunk_length_kb must be 128 or less.",
                    ));
                }
                out.insert("chunk_length_in_kb".to_string(), n.to_string());
            }
            if let Some(v) = m.get("crc_check_chance") {
                let f: f64 = v.trim().parse().map_err(|_| {
                    Error::syntax(format!("Invalid double value {v} for crc_check_chance"))
                })?;
                if !(0.0..=1.0).contains(&f) {
                    return Err(Error::config("crc_check_chance must be between 0.0 and 1.0."));
                }
                out.insert("crc_check_chance".to_string(), v.clone());
            }
            if let Some(v) = m.get("compression_level") {
                let n: i64 = v.trim().parse().map_err(|_| {
                    Error::syntax(format!("Invalid integer value {v} for compression_level"))
                })?;
                // ScyllaDB's message has its arguments out of order.
                if !(-131_072..=22).contains(&n) {
                    return Err(Error::config(format!(
                        "-131072 must be between 22 and compression_level, got {n}"
                    )));
                }
                out.insert("compression_level".to_string(), v.clone());
            }
            out.insert("sstable_compression".into(), full);
            Ok(out)
        }
        Dialect::Cassandra => {
            // `CompressionParams.fromMap`: `{}` or a false `enabled` (anything but `true`)
            // disables compression.
            if m.is_empty() {
                return Ok(map(&[("enabled", "false")]));
            }
            if let Some(e) = m.get("enabled")
                && !e.eq_ignore_ascii_case("true")
            {
                if m.len() > 1 {
                    return Err(Error::config(
                        "If the 'enabled' option is set to false no other options must be specified",
                    ));
                }
                return Ok(map(&[("enabled", "false")]));
            }
            let Some(class) = m.remove("class") else {
                return Err(Error::config(
                    "Missing sub-option 'class' for the 'compression' option.",
                ));
            };
            m.remove("enabled");
            let full = if class.contains('.') {
                class.clone()
            } else {
                format!("{CASSANDRA_COMPRESS_PKG}{class}")
            };
            let short = full.strip_prefix(CASSANDRA_COMPRESS_PKG).unwrap_or("");
            if !KNOWN_COMPRESSORS.contains(&short) {
                return Err(Error::config(format!(
                    "Could not create Compression for type {full}"
                )));
            }
            let chunk = m
                .remove("chunk_length_in_kb")
                .unwrap_or_else(|| "16".into());
            let n: i64 = chunk
                .trim()
                .parse()
                .map_err(|_| Error::config("Invalid value for chunk_length_in_kb"))?;
            if n <= 0 {
                return Err(Error::config("Invalid negative or null chunk_length_in_kb"));
            }
            if n & (n - 1) != 0 {
                return Err(Error::config("chunk_length_in_kb must be a power of 2"));
            }
            let mut ratio = None;
            if let Some(r) = m.remove("min_compress_ratio") {
                ratio = Some(r);
            }
            let allowed_extra: &[&str] = match short {
                "ZstdCompressor" => &["compression_level"],
                "LZ4Compressor" => &["lz4_compressor_type", "lz4_high_compressor_level"],
                _ => &[],
            };
            let unknown: Vec<&String> = m
                .keys()
                .filter(|k| !allowed_extra.contains(&k.as_str()))
                .collect();
            if !unknown.is_empty() {
                let names: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
                return Err(Error::config(format!(
                    "Unknown compression options {}",
                    names.join(", ")
                )));
            }
            let mut out = m;
            out.insert("class".into(), full);
            out.insert("chunk_length_in_kb".into(), n.to_string());
            if let Some(r) = ratio {
                out.insert("min_compress_ratio".into(), r);
            }
            Ok(out)
        }
    }
}

/// Normalises a `speculative_retry` / `additional_write_policy` value.
fn speculative_retry(d: Dialect, s: &str, option: &str) -> Result<String> {
    let upper = s.trim().to_ascii_uppercase();
    match d {
        Dialect::Scylla => {
            // ScyllaDB parses `<n>PERCENTILE`, `<n>ms`, `ALWAYS`, `NONE`; anything else fails while
            // slicing the string.
            if upper == "ALWAYS" || upper == "NONE" {
                return Ok(upper);
            }
            let convert =
                || Error::config(format!("cannot convert {upper} to speculative_retry\n"));
            if let Some(n) = upper.strip_suffix("PERCENTILE") {
                let v = n.parse::<f64>().map_err(|_| convert())?;
                if !(0.0..=100.0).contains(&v) {
                    return Err(Error::config(format!(
                        "Invalid value {upper} for PERCENTILE option '{option}': must be between [0.0 and 100.0]"
                    )));
                }
                return Ok(format!("{}PERCENTILE", scylla_decimal(v)));
            }
            if let Some(n) = upper.strip_suffix("MS") {
                let v = n.parse::<f64>().map_err(|_| convert())?;
                return Ok(format!("{v:.2}ms"));
            }
            Err(Error::server("sstring out of range"))
        }
        Dialect::Cassandra => {
            let bad = || Error::config(format!("Invalid value {s} for option '{option}'"));
            if upper == "ALWAYS" {
                return Ok("ALWAYS".into());
            }
            if upper == "NEVER" || upper == "NONE" {
                return Ok("NEVER".into());
            }
            let pct = upper
                .strip_suffix("PERCENTILE")
                .or_else(|| upper.strip_suffix('P'));
            // `[0-9]+\.?[0-9]*`: no sign.
            let number = |n: &str| -> Result<f64> {
                let digits = n.trim_end();
                let int = digits.split('.').next().unwrap_or("");
                if int.is_empty()
                    || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                    || digits.matches('.').count() > 1
                {
                    return Err(bad());
                }
                digits.parse().map_err(|_| bad())
            };
            if let Some(n) = pct {
                let v = number(n)?;
                if !(0.0..100.0).contains(&v) || v == 0.0 {
                    return Err(Error::config(format!(
                        "Invalid value {s} for PERCENTILE option '{option}': must be between (0.0 and 100.0)"
                    )));
                }
                return Ok(format!("{}p", java_trimmed(v)));
            }
            if let Some(n) = upper.strip_suffix("MS") {
                let v = number(n)?;
                return Ok(format!("{}ms", java_trimmed(v)));
            }
            Err(bad())
        }
    }
}

/// `99.0` → `99`, `99.5` → `99.5` (Cassandra prints percentiles without a trailing `.0`).
fn java_trimmed(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// ScyllaDB prints percentiles with one decimal at least (`99.0`).
fn scylla_decimal(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// The checks of `TableParams.validate` (Cassandra) and their ScyllaDB counterparts.
pub fn validate(p: &TableParams, d: Dialect) -> Result<()> {
    let min_fp = 6.71e-05;
    if p.bloom_filter_fp_chance <= min_fp || p.bloom_filter_fp_chance > 1.0 {
        return Err(Error::config(match d {
            Dialect::Scylla => format!(
                "bloom_filter_fp_chance must be larger than 6.71e-05 and less than or equal to 1.0 (got {})",
                scylla_number(p.bloom_filter_fp_chance)
            ),
            Dialect::Cassandra => format!(
                "bloom_filter_fp_chance must be larger than 6.71E-5 and less than or equal to 1.0 (got {})",
                nosql_types::float::java_double(p.bloom_filter_fp_chance)
            ),
        }));
    }
    if !d.is_scylla() && (p.crc_check_chance < 0.0 || p.crc_check_chance > 1.0) {
        return Err(Error::config(format!(
            "crc_check_chance must be larger than or equal to 0 and smaller than or equal to 1.0 (got {})",
            nosql_types::float::java_double(p.crc_check_chance)
        )));
    }
    if p.default_time_to_live < 0 {
        return Err(Error::config(match d {
            Dialect::Scylla => {
                "default_time_to_live cannot be smaller than 0, (default 0)".to_string()
            }
            Dialect::Cassandra => format!(
                "default_time_to_live must be greater than or equal to 0 (got {})",
                p.default_time_to_live
            ),
        }));
    }
    if !d.is_scylla() && p.default_time_to_live > crate::limits::MAX_TTL {
        return Err(Error::config(format!(
            "default_time_to_live must be less than or equal to {} (got {})",
            crate::limits::MAX_TTL,
            p.default_time_to_live
        )));
    }
    if !d.is_scylla() && p.gc_grace_seconds < 0 {
        return Err(Error::config(format!(
            "gc_grace_seconds must be greater than or equal to 0 (got {})",
            p.gc_grace_seconds
        )));
    }
    if p.min_index_interval < 1 {
        return Err(Error::config(match d {
            Dialect::Scylla => "min_index_interval must be greater than 0".to_string(),
            Dialect::Cassandra => format!(
                "min_index_interval must be greater than or equal to 1 (got {})",
                p.min_index_interval
            ),
        }));
    }
    if p.memtable_flush_period_in_ms != 0 && p.memtable_flush_period_in_ms < 60_000 && d.is_scylla()
    {
        return Err(Error::config(
            "memtable_flush_period_in_ms must be 0 or greater than 60000",
        ));
    }
    if p.memtable_flush_period_in_ms < 0 {
        return Err(Error::config(match d {
            Dialect::Scylla => {
                "memtable_flush_period_in_ms must be greater than or equal to 0".to_string()
            }
            Dialect::Cassandra => format!(
                "memtable_flush_period_in_ms must be greater than or equal to 0 (got {})",
                p.memtable_flush_period_in_ms
            ),
        }));
    }
    Ok(())
}

/// `fmt::format("{}", double)` as ScyllaDB prints numbers in messages (shortest form).
fn scylla_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    // `{fmt}`'s shortest form switches to an exponent (two digits at least) below 1e-4.
    if v != 0.0 && v.abs() < 1e-4 {
        let e = format!("{v:e}");
        if let Some((m, x)) = e.split_once('e') {
            let (sign, digits) = match x.strip_prefix('-') {
                Some(d) => ("-", d),
                None => ("+", x),
            };
            return format!("{m}e{sign}{digits:0>2}");
        }
    }
    format!("{v}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosql_cql::ast::{Ident, Literal};
    use std::borrow::Cow;

    fn simple(name: &str, text: &str) -> Property<'static> {
        Property {
            name: Ident::unquoted(name.to_string()),
            value: PropertyValue::Simple(Literal {
                kind: LiteralKind::String,
                text: Cow::Owned(text.into()),
                dollar: false,
            }),
        }
    }

    fn mapped(name: &str, pairs: &[(&str, &str)]) -> Property<'static> {
        let lit = |s: &str| Literal {
            kind: LiteralKind::String,
            text: Cow::Owned(s.into()),
            dollar: false,
        };
        Property {
            name: Ident::unquoted(name.to_string()),
            value: PropertyValue::Map(pairs.iter().map(|(k, v)| (lit(k), lit(v))).collect()),
        }
    }

    fn run(d: Dialect, props: &[Property<'static>]) -> Result<TableParams> {
        let p = Properties::new(props.iter(), valid_table_options(d), d)?;
        let mut params = TableParams::defaults(d, false);
        apply(&mut params, &p, false)?;
        Ok(params)
    }

    fn err(d: Dialect, props: &[Property<'static>]) -> (crate::ErrorCode, String) {
        let e = run(d, props).unwrap_err();
        (e.code, e.message)
    }

    #[test]
    fn compaction_normalisation_and_errors() {
        let c = run(
            Dialect::Cassandra,
            &[mapped(
                "compaction",
                &[
                    ("class", "LeveledCompactionStrategy"),
                    ("sstable_size_in_mb", "64"),
                ],
            )],
        )
        .unwrap();
        assert_eq!(
            c.compaction,
            map(&[
                (
                    "class",
                    "org.apache.cassandra.db.compaction.LeveledCompactionStrategy"
                ),
                ("max_threshold", "32"),
                ("min_threshold", "4"),
                ("sstable_size_in_mb", "64")
            ])
        );
        let s = run(
            Dialect::Scylla,
            &[mapped(
                "compaction",
                &[
                    ("class", "LeveledCompactionStrategy"),
                    ("sstable_size_in_mb", "64"),
                ],
            )],
        )
        .unwrap();
        assert_eq!(
            s.compaction,
            map(&[
                ("class", "LeveledCompactionStrategy"),
                ("sstable_size_in_mb", "64")
            ])
        );
        let ucs = run(
            Dialect::Cassandra,
            &[mapped(
                "compaction",
                &[("class", "UnifiedCompactionStrategy")],
            )],
        )
        .unwrap();
        assert_eq!(
            ucs.compaction,
            map(&[(
                "class",
                "org.apache.cassandra.db.compaction.UnifiedCompactionStrategy"
            )])
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                &[mapped("compaction", &[("class", "Nope")])]
            )
            .1,
            "Unable to find compaction strategy class 'Nope'"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[mapped("compaction", &[("class", "Nope")])]
            )
            .1,
            "Unable to find compaction strategy class 'org.apache.cassandra.db.compaction.Nope'"
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                &[mapped("compaction", &[("min_threshold", "2")])]
            )
            .1,
            "Missing sub-option 'class' for the 'compaction' option."
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[mapped("compaction", &[("min_threshold", "2")])]
            )
            .1,
            "Missing sub-option 'class' for the 'compaction' option"
        );
        let bad = [mapped(
            "compaction",
            &[("class", "SizeTieredCompactionStrategy"), ("nope", "1")],
        )];
        assert_eq!(
            err(Dialect::Scylla, &bad).1,
            "Invalid compaction strategy options {\"nope\": \"1\"} for chosen strategy type"
        );
        assert_eq!(
            err(Dialect::Cassandra, &bad).1,
            "Properties specified [nope] are not understood by SizeTieredCompactionStrategy"
        );
        let thr = [mapped(
            "compaction",
            &[
                ("class", "SizeTieredCompactionStrategy"),
                ("min_threshold", "1"),
            ],
        )];
        assert_eq!(
            err(Dialect::Scylla, &thr).1,
            "min_threshold value (1) must be bigger or equal to 2"
        );
        assert_eq!(
            err(Dialect::Cassandra, &thr).1,
            "Min compaction threshold cannot be less than 2 (got 1)"
        );
        let twcs = [mapped(
            "compaction",
            &[
                ("class", "TimeWindowCompactionStrategy"),
                ("compaction_window_unit", "WEEKS"),
            ],
        )];
        assert_eq!(
            err(Dialect::Scylla, &twcs).1,
            "Invalid window unit WEEKS for compaction_window_unit"
        );
        assert_eq!(
            err(Dialect::Cassandra, &twcs),
            (
                crate::ErrorCode::Config,
                "WEEKS is not valid for compaction_window_unit".into()
            )
        );
    }

    #[test]
    fn scalar_option_errors() {
        assert_eq!(
            err(Dialect::Cassandra, &[simple("gc_grace_seconds", "-1")]).1,
            "gc_grace_seconds must be greater than or equal to 0 (got -1)"
        );
        assert!(run(Dialect::Scylla, &[simple("gc_grace_seconds", "-1")]).is_ok());
        assert_eq!(
            err(Dialect::Scylla, &[simple("default_time_to_live", "-1")]).1,
            "default_time_to_live cannot be smaller than 0, (default 0)"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[simple("default_time_to_live", "999999999")]
            )
            .1,
            "default_time_to_live must be less than or equal to 630720000 (got 999999999)"
        );
        assert_eq!(
            err(Dialect::Scylla, &[simple("bloom_filter_fp_chance", "2")]).1,
            "bloom_filter_fp_chance must be larger than 6.71e-05 and less than or equal to 1.0 (got 2)"
        );
        assert_eq!(
            err(Dialect::Cassandra, &[simple("bloom_filter_fp_chance", "0")]).1,
            "bloom_filter_fp_chance must be larger than 6.71E-5 and less than or equal to 1.0 (got 0.0)"
        );
        assert_eq!(
            err(Dialect::Scylla, &[simple("min_index_interval", "0")]).1,
            "min_index_interval must be greater than 0"
        );
        assert_eq!(
            err(Dialect::Cassandra, &[simple("min_index_interval", "0")]).1,
            "min_index_interval must be greater than or equal to 1 (got 0)"
        );
        assert_eq!(
            err(Dialect::Cassandra, &[simple("crc_check_chance", "2")]).1,
            "crc_check_chance must be larger than or equal to 0 and smaller than or equal to 1.0 (got 2.0)"
        );
        assert!(run(Dialect::Scylla, &[simple("crc_check_chance", "2")]).is_ok());
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[simple("comment", "a"), simple("comment", "b")]
            ),
            (
                crate::ErrorCode::Syntax,
                "Multiple definitions for property 'comment'".into()
            )
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                &[simple("comment", "a"), simple("comment", "b")]
            )
            .1,
            "Multiple definition for property 'comment'"
        );
        assert_eq!(
            err(Dialect::Scylla, &[simple("nope", "1")]),
            (crate::ErrorCode::Syntax, "Unknown property 'nope'".into())
        );
        assert_eq!(
            err(Dialect::Cassandra, &[simple("tablets", "1")]).1,
            "Unknown property 'tablets'"
        );
        assert_eq!(
            err(Dialect::Cassandra, &[mapped("cdc", &[("enabled", "true")])]).1,
            "Invalid value for property 'cdc'. It should be a string"
        );
        assert_eq!(
            err(Dialect::Scylla, &[simple("cdc", "true")]),
            (
                crate::ErrorCode::Server,
                "Cannot create cdc info from string".into()
            )
        );
        assert_eq!(
            err(Dialect::Scylla, &[simple("synchronous_updates", "true")]).1,
            "The synchronous_updates option is only applicable to materialized views, not to base tables"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[simple("speculative_retry", "sometimes")]
            )
            .1,
            "Invalid value sometimes for option 'speculative_retry'"
        );
        assert_eq!(
            err(Dialect::Scylla, &[simple("speculative_retry", "95p")]),
            (crate::ErrorCode::Server, "sstring out of range".into())
        );
        assert_eq!(
            run(
                Dialect::Cassandra,
                &[simple("speculative_retry", "99PERCENTILE")]
            )
            .unwrap()
            .speculative_retry,
            "99p"
        );
        assert_eq!(
            run(
                Dialect::Scylla,
                &[simple("speculative_retry", "99PERCENTILE")]
            )
            .unwrap()
            .speculative_retry,
            "99.0PERCENTILE"
        );
        assert_eq!(
            err(Dialect::Cassandra, &[simple("gc_grace_seconds", "x")]).1,
            "Invalid integer value x for 'gc_grace_seconds'"
        );
    }

    #[test]
    fn caching_and_compression() {
        assert_eq!(
            err(Dialect::Scylla, &[mapped("caching", &[("keys", "SOME")])]).1,
            "Invalid key value: SOME"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[mapped("caching", &[("keys", "SOME")])]
            )
            .1,
            "Invalid value 'SOME' for caching sub-option 'keys': only 'ALL' and 'NONE' are allowed"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[mapped(
                    "caching",
                    &[("keys", "ALL"), ("rows_per_partition", "MANY")]
                )]
            )
            .1,
            "Invalid value 'MANY' for caching sub-option 'rows_per_partition': only 'ALL', 'NONE', and integer values are allowed"
        );
        assert_eq!(
            run(
                Dialect::Cassandra,
                &[mapped(
                    "caching",
                    &[("keys", "ALL"), ("rows_per_partition", "10")]
                )]
            )
            .unwrap()
            .caching,
            map(&[("keys", "ALL"), ("rows_per_partition", "10")])
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                &[mapped("compression", &[("class", "LZ4Compressor")])]
            )
            .1,
            "Missing sub-option 'sstable_compression' for the 'compression' option."
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[mapped(
                    "compression",
                    &[("sstable_compression", "LZ4Compressor")]
                )]
            )
            .1,
            "Missing sub-option 'class' for the 'compression' option."
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[mapped("compression", &[("class", "NopeCompressor")])]
            )
            .1,
            "Could not create Compression for type org.apache.cassandra.io.compress.NopeCompressor"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[mapped(
                    "compression",
                    &[("class", "LZ4Compressor"), ("chunk_length_in_kb", "3")]
                )]
            )
            .1,
            "chunk_length_in_kb must be a power of 2"
        );
        assert_eq!(
            err(
                Dialect::Cassandra,
                &[mapped(
                    "compression",
                    &[("class", "LZ4Compressor"), ("nope", "1")]
                )]
            )
            .1,
            "Unknown compression options nope"
        );
        let s = run(
            Dialect::Scylla,
            &[mapped(
                "compression",
                &[
                    ("sstable_compression", "DeflateCompressor"),
                    ("chunk_length_in_kb", "64"),
                ],
            )],
        )
        .unwrap();
        assert_eq!(
            s.compression,
            map(&[
                ("chunk_length_in_kb", "64"),
                (
                    "sstable_compression",
                    "org.apache.cassandra.io.compress.DeflateCompressor"
                )
            ])
        );
        let c = run(
            Dialect::Cassandra,
            &[mapped(
                "compression",
                &[("class", "ZstdCompressor"), ("chunk_length_in_kb", "16")],
            )],
        )
        .unwrap();
        assert_eq!(
            c.compression,
            map(&[
                ("chunk_length_in_kb", "16"),
                ("class", "org.apache.cassandra.io.compress.ZstdCompressor")
            ])
        );
        assert_eq!(
            run(
                Dialect::Cassandra,
                &[mapped("compression", &[("enabled", "false")])]
            )
            .unwrap()
            .compression,
            map(&[("enabled", "false")])
        );
    }

    #[test]
    fn scylla_extensions_round_trip() {
        let p = TableParams::defaults(Dialect::Scylla, false);
        let ext = p.extensions_map(Dialect::Scylla);
        // The exact bytes the reference stores for the default tombstone_gc.
        assert_eq!(
            &ext["tombstone_gc"][..],
            b"\x02\x00\x00\x00\x04\x00\x00\x00mode\x06\x00\x00\x00repair\x1c\x00\x00\x00propagation_delay_in_seconds\x04\x00\x00\x003600"
        );
        let mut back = TableParams::defaults(Dialect::Scylla, false);
        back.tombstone_gc.clear();
        back.extensions = ext;
        back.absorb_extensions(Dialect::Scylla);
        assert_eq!(back, p);
        assert!(parse_scylla_map_blob(b"\x05\x00").is_none());
        assert_eq!(
            p.gc_policy().mode,
            nosql_storage::options::TombstoneGcMode::Repair
        );
        assert_eq!(p.compaction_params().class, "IncrementalCompactionStrategy");
    }
}
