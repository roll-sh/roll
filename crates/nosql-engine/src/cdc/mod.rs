//! ScyllaDB Change Data Capture: the `<table>_scylla_cdc_log` table and its rows.
//!
//! Learned from ScyllaDB's documentation (`docs/features/cdc/`) and the reference image (the
//! `derived.py` capture): the log's schema (key columns as regular columns, `cdc$deleted_X` and,
//! for non-frozen collections, `cdc$deleted_elements_X`), its options (TWCS sized from the CDC
//! TTL, no caching, `tombstone_gc` timeout, the CDC partitioner), and the rows each write produces:
//!
//! - One *group* per base partition and write timestamp: rows share `cdc$time` (a timeuuid at
//!   the write's timestamp) and are numbered by `cdc$batch_seq_no`; the last one has
//!   `cdc$end_of_batch`.
//! - Pre-images (operation 0, `preimage`: the touched columns of rows that existed; `full`: every
//!   column), then deltas (1 update, 2 insert, 3 row delete, 4 partition delete, 5–8 range bounds),
//!   then post-images (9, `postimage`: the whole row after the write). Deletions by a service
//!   ([`as_service`]: per-row TTL expiration) are -3 and -4. The static row comes first
//!   and is logged apart from clustering rows.
//! - A delta row holds the written values, `cdc$deleted_X` for nulls and collection-wide
//!   tombstones, `cdc$deleted_elements_X` for removed elements, and `cdc$ttl`; cells with different
//!   TTLs are logged in separate rows (dead cells first). `delta: keys` logs the keys only.
//! - Log rows expire after the CDC `ttl` option.
//!
//! Streams: a base partition's rows go to the stream of the token range holding its token. A
//! vnodes keyspace uses the node's CDC generation (one stream per vnode,
//! `system_distributed.cdc_streams_descriptions_v2`); a tablets keyspace a stream set per table
//! (`system.cdc_streams`). Stream ids have ScyllaDB's layout (the range's token in the first 8
//! bytes, version 1 in the last). Unlike ScyllaDB's `CDCPartitioner`, log partitions are placed by
//! the Murmur3 token of the stream id, so `token("cdc$stream_id")` and the order of a log scan
//! differ (stream ids are random on the reference anyway).
//!
//! Vector Search: a ScyllaDB `vector_index` feeds from the log, so creating one turns the log on
//! with the default options when the table has none (the `cdc` option stays unset), and requires
//! a TTL of at least a day (or none) and full deltas or post-images when it has some. Dropping the
//! last vector index stops the logging and leaves the log as a plain table.
//!
//! Cassandra's CDC (`cdc = true`, a commit log feature) only stores the table flag (core).
//!
//! Alternator's DynamoDB Streams map onto these rows (see the engine README).

pub mod tables;

use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use nosql_cql::ast;
use nosql_storage::manifest::TableId;
use nosql_storage::{Cell, Clustering, ColumnData, LivenessInfo, PartitionUpdate, Row};
use nosql_types::{CqlType, Dialect};

use crate::config::DbConfig;
use crate::database::Database;
use crate::error::{Error, Result};
use crate::schema::{Column, ColumnKind, Keyspace, Table, TableFlags, TableKind, TableParams};
use crate::views::observer::Touched;

/// The log table's name suffix.
pub const SUFFIX: &str = "_scylla_cdc_log";

/// `com.scylladb.dht.CDCPartitioner`.
pub const PARTITIONER: &str = "com.scylladb.dht.CDCPartitioner";

/// Stream ranges per table in a tablets keyspace without `initial` tablets (the reference's
/// default tablet count).
const TABLET_STREAMS: usize = 16;

/// Whether `t` has CDC enabled (ScyllaDB `cdc = {'enabled': true}`).
pub fn enabled(t: &Table) -> bool {
    t.params
        .scylla_cdc
        .get("enabled")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Whether `t` has a ScyllaDB vector or full-text index (the Vector Store reads the log).
pub fn vector_search(t: &Table) -> bool {
    t.indexes.iter().any(|i| {
        i.options.get("class_name").is_some_and(|c| {
            c.eq_ignore_ascii_case("vector_index") || c.eq_ignore_ascii_case("fulltext_index")
        })
    })
}

/// Whether `t` has a ScyllaDB full-text index.
pub fn fulltext_search(t: &Table) -> bool {
    t.indexes.iter().any(|i| {
        i.options
            .get("class_name")
            .is_some_and(|c| c.eq_ignore_ascii_case("fulltext_index"))
    })
}

/// Whether writes to `t` are logged: CDC enabled, or Vector Search on.
pub fn active(t: &Table) -> bool {
    enabled(t) || vector_search(t)
}

/// Whether `t` has the log table's key layout.
fn log_shaped(t: &Table) -> bool {
    let pk: Vec<&str> = t.partition_key_columns().map(|c| c.name.as_str()).collect();
    let ck: Vec<&str> = t.clustering_columns().map(|c| c.name.as_str()).collect();
    pk == ["cdc$stream_id"] && ck == ["cdc$time", "cdc$batch_seq_no"]
}

/// The log of `base` in `ks`, if `ks` has one (logging or not): dropped with its base.
pub fn log_of<'a>(ks: &'a Keyspace, base: &str) -> Option<&'a Arc<Table>> {
    ks.tables.get(&log_name(base)).filter(|t| log_shaped(t))
}

/// The error for a log name taken by another table.
pub fn log_name_taken(ks: &str, base: &str) -> Error {
    Error::invalid(format!(
        "Cannot create CDC log table for table {ks}.{base} because a table of name {ks}.{} already exists",
        log_name(base)
    ))
}

/// Whether the CDC options of `t` meet Vector Search's needs (a TTL of a day or none; full deltas
/// or post-images).
fn vector_ready(t: &Table) -> bool {
    let ttl = ttl_option(t);
    let opt = |k: &str| t.params.scylla_cdc.get(k).map(|v| v.to_ascii_lowercase());
    (ttl == 0 || ttl >= 86_400)
        && (opt("delta").as_deref() != Some("keys") || opt("postimage").as_deref() == Some("true"))
}

/// `CREATE INDEX ... USING 'vector_index'` on `t`: its CDC options must allow it.
pub fn check_vector_index(t: &Table) -> Result<()> {
    if t.params.scylla_cdc.contains_key("enabled") && !enabled(t) {
        return Err(Error::invalid(
            "Cannot create the vector index when CDC is explicitly disabled.\n\
             Please enable CDC with the required parameters first.\n\
             CDC's TTL must be at least 86400 seconds (24 hours), and the CDC's delta mode must be set to 'full' or postimage must be enabled to enable Vector Search.\n\
             Check documentation on how to setup CDC's parameters - https://docs.scylladb.com/manual/branch-2025.2/features/cdc/cdc-intro.html#cdc-parameters",
        ));
    }
    if enabled(t) && !vector_ready(t) {
        return Err(Error::invalid(
            "To enable Vector Search on this table, the CDC log must meet the minimal requirements for external indexes.\n\
             CDC's TTL must be at least 86400 seconds (24 hours), and the CDC's delta mode must be set to 'full' or postimage must be enabled.",
        ));
    }
    Ok(())
}

/// `CREATE CUSTOM INDEX ... USING 'fulltext_index'` on `t`: its CDC options must allow it (CDC
/// explicitly disabled is fine: the index turns logging on).
pub fn check_fulltext_index(t: &Table) -> Result<()> {
    if enabled(t) && !vector_ready(t) {
        return Err(Error::invalid(
            "To enable Full-Text Search on this table, the CDC log must meet the minimal requirements for external indexes.\n\
             CDC's TTL must be at least 86400 seconds (24 hours), and the CDC's delta mode must be set to 'full' or postimage must be enabled.",
        ));
    }
    Ok(())
}

/// `ALTER TABLE t WITH cdc = ...` while Vector Search is on: CDC stays on and usable.
pub fn check_vector_alter(t: &Table) -> Result<()> {
    if !vector_search(t) || t.params.scylla_cdc.is_empty() {
        return Ok(());
    }
    if fulltext_search(t) {
        if !enabled(t) {
            return Err(Error::invalid(
                "Cannot disable CDC when Full-Text Search is enabled on the table.\n\
                 Please drop the fulltext index first, then disable CDC.",
            ));
        }
        if !vector_ready(t) {
            return Err(Error::invalid(
                "Full-Text Search is enabled on this table.\n\
                 The CDC log must meet the minimal requirements for external indexes.\n\
                 This means that the CDC's TTL must be at least 86400 seconds (24 hours), and the CDC's delta mode must be set to 'full' or postimage must be enabled.",
            ));
        }
        return Ok(());
    }
    if !enabled(t) {
        return Err(Error::invalid(
            "Cannot disable CDC when Vector Search is enabled on the table.\n\
             Please drop the vector index first, then disable CDC.",
        ));
    }
    if !vector_ready(t) {
        return Err(Error::invalid(
            "Vector Search is enabled on this table.\n\
             The CDC log must meet the minimal requirements for external indexes.\n\
             This means that the CDC's TTL must be at least 86400 seconds (24 hours), and the CDC's delta mode must be set to 'full' or postimage must be enabled.",
        ));
    }
    Ok(())
}

/// The log table name of `base`.
pub fn log_name(base: &str) -> String {
    format!("{base}{SUFFIX}")
}

/// Whether `t` is the CDC log of a table of `ks`.
pub fn is_log(ks: &Keyspace, t: &Table) -> bool {
    t.name
        .strip_suffix(SUFFIX)
        .and_then(|b| ks.tables.get(b))
        .is_some_and(|b| {
            log_shaped(t) && (vector_search(b) || b.params.scylla_cdc.contains_key("enabled"))
        })
}

/// Whether `t` is the log of a table of `ks` whose writes are logged (CDC enabled or a vector
/// index): `DESCRIBE` shows it as a log only then.
pub fn is_active_log(ks: &Keyspace, t: &Table) -> bool {
    is_log(ks, t)
        && t.name
            .strip_suffix(SUFFIX)
            .and_then(|b| ks.tables.get(b))
            .is_some_and(|b| active(b))
}

/// `ALTER TABLE` of a CDC log table: its columns follow the base table, so adding, dropping,
/// retyping or renaming them is refused (with a longer explanation while the base has CDC
/// disabled: the table stays a log until it is dropped).
pub fn check_log_alter(ks: &Keyspace, t: &Table, action: &ast::AlterTableAction) -> Result<()> {
    use ast::AlterTableAction as A;
    if !is_log(ks, t) {
        return Ok(());
    }
    let inactive = !is_active_log(ks, t);
    match action {
        A::Add { .. } | A::DropObject { .. } | A::AlterType { .. } => {
            Err(Error::invalid(if inactive {
                "You cannot modify the set of columns of a CDC log table directly. Although the base table has deactivated CDC, this table will continue being a CDC log table until it is dropped. If you want to modify the columns in it, you can only do that by reenabling CDC on the base table, which will reattach this log table. Then you will be able to modify the columns in the base table, and that will have effect on the log table too. Modifying the columns of a CDC log table directly is never allowed."
            } else {
                "You cannot modify the set of columns of a CDC log table directly. Modify the base table instead."
            }))
        }
        A::Rename { .. } => Err(Error::invalid(if inactive {
            "You cannot rename a column of a CDC log table. Although the base table has deactivated CDC, this table will continue being a CDC log table until it is dropped."
        } else {
            "Cannot rename a column of a CDC log table."
        })),
        _ => Ok(()),
    }
}

/// Validates ScyllaDB's `cdc` options (`cdc::options`).
pub fn validate_options(opts: &BTreeMap<String, String>) -> Result<()> {
    for (k, v) in opts {
        let ok = match k.as_str() {
            "enabled" | "postimage" => matches!(v.as_str(), "true" | "false"),
            "preimage" => matches!(v.as_str(), "true" | "false" | "full"),
            "delta" => matches!(v.as_str(), "full" | "keys"),
            "ttl" => match v.parse::<i64>() {
                Ok(n) if n >= 0 => true,
                Ok(_) => return Err(Error::config("Invalid CDC option: ttl must be >= 0")),
                Err(_) => false,
            },
            _ => return Err(Error::config(format!("Invalid CDC option: {k}"))),
        };
        if !ok {
            return Err(Error::config(format!(
                "Invalid value for CDC option \"{k}\": {v}"
            )));
        }
    }
    Ok(())
}

fn ttl_option(base: &Table) -> i64 {
    base.params
        .scylla_cdc
        .get("ttl")
        .and_then(|v| v.parse().ok())
        .unwrap_or(86_400)
}

/// The type of the log's value column for a base column (non-frozen collections frozen, a
/// list as the `map<timeuuid, V>` it is).
fn value_type(ty: &CqlType) -> CqlType {
    match ty {
        CqlType::List {
            elem,
            frozen: false,
        } => CqlType::map(CqlType::Timeuuid, (**elem).clone(), true),
        t if t.is_multicell() => t.freeze(),
        t => t.clone(),
    }
}

/// The element type of `cdc$deleted_elements_X`.
fn deleted_elements_type(ty: &CqlType) -> Option<CqlType> {
    if !ty.is_multicell() {
        return None;
    }
    Some(match ty {
        CqlType::List { .. } => CqlType::set(CqlType::Timeuuid, true),
        CqlType::Set { elem, .. } => CqlType::set((**elem).clone(), true),
        CqlType::Map { key, .. } => CqlType::set((**key).clone(), true),
        _ => CqlType::set(CqlType::Smallint, true),
    })
}

/// The log table of `base` (`cdc::log_schema`), with a new id.
pub fn log_table(base: &Table, d: Dialect) -> Result<Table> {
    if base.is_counter() {
        return Err(Error::invalid(format!(
            "Cannot create CDC log for table {}.{}. Counter support not implemented",
            base.keyspace, base.name
        )));
    }
    let mut cols: Vec<(String, CqlType, ColumnKind, i32)> = vec![
        (
            "cdc$stream_id".into(),
            CqlType::Blob,
            ColumnKind::PartitionKey,
            0,
        ),
        (
            "cdc$time".into(),
            CqlType::Timeuuid,
            ColumnKind::Clustering,
            0,
        ),
        (
            "cdc$batch_seq_no".into(),
            CqlType::Int,
            ColumnKind::Clustering,
            1,
        ),
        (
            "cdc$end_of_batch".into(),
            CqlType::Boolean,
            ColumnKind::Regular,
            -1,
        ),
        (
            "cdc$operation".into(),
            CqlType::Tinyint,
            ColumnKind::Regular,
            -1,
        ),
        ("cdc$ttl".into(), CqlType::Bigint, ColumnKind::Regular, -1),
    ];
    for c in &base.columns {
        if c.is_primary_key() {
            cols.push((c.name.clone(), c.ty.clone(), ColumnKind::Regular, -1));
            continue;
        }
        cols.push((c.name.clone(), value_type(&c.ty), ColumnKind::Regular, -1));
        cols.push((
            format!("cdc$deleted_{}", c.name),
            CqlType::Boolean,
            ColumnKind::Regular,
            -1,
        ));
        if let Some(t) = deleted_elements_type(&c.ty) {
            cols.push((
                format!("cdc$deleted_elements_{}", c.name),
                t,
                ColumnKind::Regular,
                -1,
            ));
        }
    }
    let columns: Vec<Column> = cols
        .into_iter()
        .enumerate()
        .map(|(i, (name, ty, kind, position))| Column {
            name,
            ty,
            kind,
            position,
            reversed: false,
            id: i as u32,
        })
        .collect();
    let ttl = ttl_option(base);
    let mut params = TableParams::defaults(d, false);
    params.comment = format!("CDC log for {}.{}", base.keyspace, base.name);
    params.caching = BTreeMap::from([
        ("enabled".to_string(), "false".to_string()),
        ("keys".to_string(), "NONE".to_string()),
        ("rows_per_partition".to_string(), "NONE".to_string()),
    ]);
    // TWCS windows of a 24th of the TTL; without a TTL, plain TWCS and the default grace.
    params.compaction = BTreeMap::from([(
        "class".to_string(),
        "TimeWindowCompactionStrategy".to_string(),
    )]);
    if ttl > 0 {
        let (window, check) = ((ttl + 1439) / 1440, ttl / 48);
        params.compaction.extend([
            (
                "compaction_window_size".to_string(),
                window.max(1).to_string(),
            ),
            ("compaction_window_unit".to_string(), "MINUTES".to_string()),
            (
                "expired_sstable_check_frequency_seconds".to_string(),
                check.max(1).to_string(),
            ),
        ]);
        params.gc_grace_seconds = 0;
    }
    params.tombstone_gc = BTreeMap::from([
        ("mode".to_string(), "timeout".to_string()),
        (
            "propagation_delay_in_seconds".to_string(),
            "3600".to_string(),
        ),
    ]);
    let id = crate::uuid::timeuuid();
    Ok(Table::new(
        base.keyspace.clone(),
        log_name(&base.name),
        id,
        crate::uuid::timeuuid(),
        columns,
        params,
        TableFlags {
            compound: true,
            ..Default::default()
        },
        TableKind::Regular,
    ))
}

/// `log` after its base changed (`ALTER TABLE base ADD/DROP/RENAME`, CDC re-enabled): the
/// columns follow the base, keeping their ids; the log columns that went away are dropped at
/// `at` (the statement's timestamp), as ScyllaDB records them.
pub fn follow_base(log: &Table, base: &Table, d: Dialect, at: i64) -> Result<Option<Table>> {
    let fresh = log_table(base, d)?;
    let names: Vec<&str> = log.columns.iter().map(|c| c.name.as_str()).collect();
    let fresh_names: Vec<&str> = fresh.columns.iter().map(|c| c.name.as_str()).collect();
    if names == fresh_names
        && log
            .columns
            .iter()
            .zip(&fresh.columns)
            .all(|(a, b)| a.ty == b.ty)
    {
        return Ok(None);
    }
    let mut next = log.next_column_id;
    let cols: Vec<Column> = fresh
        .columns
        .into_iter()
        .map(|mut c| {
            c.id = match log.column_ids.get(&c.name) {
                Some(id) => *id,
                None => {
                    next += 1;
                    next - 1
                }
            };
            c
        })
        .collect();
    let mut t = log.rebuild(cols, log.params.clone());
    for c in &log.columns {
        if t.column(&c.name).is_none() && !t.dropped.contains_key(&c.name) {
            t.dropped.insert(
                c.name.clone(),
                crate::schema::DroppedColumn {
                    dropped_at: at,
                    ty: c.ty.clone(),
                    kind: c.kind,
                },
            );
        }
    }
    Ok(Some(t))
}

/// A part of a delta row: (TTL class, log cell, whether it is an atomic live cell).
type Part = (Option<i32>, (u32, Option<Bytes>), bool);

/// How pre-images are logged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Image {
    Off,
    /// The columns the write touches.
    On,
    /// Every column.
    Full,
}

/// The log columns of one base column.
#[derive(Clone, Debug)]
struct LogColumn {
    base: usize,
    value: u32,
    deleted: Option<u32>,
    deleted_elements: Option<u32>,
}

/// The CDC log of a base table, resolved.
#[derive(Debug)]
pub struct LogSpec {
    pub log: Arc<Table>,
    pub preimage: Image,
    pub postimage: bool,
    pub delta_full: bool,
    pub ttl: i64,
    columns: Vec<LogColumn>,
    end_of_batch: u32,
    operation: u32,
    ttl_column: u32,
    pub streams: Arc<Streams>,
}

impl LogSpec {
    /// The log of `t` when CDC is enabled and the log exists (ScyllaDB).
    pub fn for_base(ks: &Keyspace, t: &Table, cfg: &DbConfig) -> Option<LogSpec> {
        if !cfg.dialect.is_scylla() || !active(t) {
            return None;
        }
        let log = ks.tables.get(&log_name(&t.name))?.clone();
        let opt = |k: &str| t.params.scylla_cdc.get(k).map(|v| v.to_ascii_lowercase());
        let preimage = match opt("preimage").as_deref() {
            Some("true") => Image::On,
            Some("full") => Image::Full,
            _ => Image::Off,
        };
        let id = |n: &str| log.column(n).map(|c| c.id);
        let mut columns = Vec::new();
        for (bi, c) in t.columns.iter().enumerate() {
            let Some(value) = id(&c.name) else { continue };
            columns.push(LogColumn {
                base: bi,
                value,
                deleted: id(&format!("cdc$deleted_{}", c.name)),
                deleted_elements: id(&format!("cdc$deleted_elements_{}", c.name)),
            });
        }
        Some(LogSpec {
            preimage,
            postimage: opt("postimage").as_deref() == Some("true"),
            delta_full: opt("delta").as_deref() != Some("keys"),
            ttl: ttl_option(t),
            columns,
            end_of_batch: id("cdc$end_of_batch")?,
            operation: id("cdc$operation")?,
            ttl_column: id("cdc$ttl")?,
            streams: Arc::new(match ks.tablets {
                Some(_) => generations(ks, t, &log).pop()?,
                None => Streams::new(cfg, t, &log, false),
            }),
            log,
        })
    }
}

/// The streams of a table: token range ends and their stream ids.
#[derive(Debug)]
pub struct Streams {
    pub ranges: Vec<(i64, [u8; 16])>,
    /// When the stream set started operating (milliseconds).
    pub timestamp_ms: i64,
    pub tablets: bool,
}

/// ScyllaDB's stream id layout: the token, then hashed bits, then version 1.
fn stream_id(token: i64, salt: &[u8]) -> [u8; 16] {
    let h = crate::uuid::v3(salt);
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&token.to_be_bytes());
    id[8..14].copy_from_slice(&h[..6]);
    id[15] = 1;
    id
}

/// The vnode generation's timestamp: a fixed instant before any write (ScyllaDB's is the
/// cluster's bootstrap).
pub const GENERATION_MS: i64 = 1_704_067_200_000;

/// The tablets of a table in a keyspace with `initial` tablets (0: the default count), a power
/// of two as ScyllaDB rounds it.
pub fn tablet_count(initial: i32) -> usize {
    match initial {
        n if n > 0 => (n as usize).next_power_of_two().min(1 << 16),
        _ => TABLET_STREAMS,
    }
}

impl Streams {
    pub fn new(cfg: &DbConfig, base: &Table, log: &Table, tablets: bool) -> Streams {
        if tablets {
            return Streams::tablets(base, log, TABLET_STREAMS);
        }
        let host = cfg.host_id();
        let ranges = generation(cfg)
            .into_iter()
            .map(|t| {
                let mut salt = host.to_vec();
                salt.extend_from_slice(&t.to_be_bytes());
                (t, stream_id(t, &salt))
            })
            .collect();
        Streams {
            ranges,
            timestamp_ms: GENERATION_MS,
            tablets: false,
        }
    }

    /// The stream set of a table with `count` tablets (a power of two): tablet `i` ends at token
    /// `-2^63 + (i + 1) * 2^64 / count - 1`, as ScyllaDB splits the ring.
    pub fn tablets(base: &Table, log: &Table, count: usize) -> Streams {
        let count = count.max(1);
        let width = (1u128 << 64) / count as u128;
        let ranges = (0..count)
            .map(|i| {
                let end = (i128::from(i64::MIN) + (i as i128 + 1) * width as i128 - 1) as i64;
                let mut salt = base.id.to_vec();
                salt.extend_from_slice(&(i as u32).to_be_bytes());
                (end, stream_id(end, &salt))
            })
            .collect();
        Streams {
            ranges,
            timestamp_ms: unix_millis(&log.id),
            tablets: true,
        }
    }

    /// A later stream set of the table (after a tablet count change at `timestamp_ms`): the
    /// stream ids also hash the generation, so no id is reused.
    pub fn tablets_at(base: &Table, count: usize, timestamp_ms: i64) -> Streams {
        let count = count.max(1);
        let width = (1u128 << 64) / count as u128;
        let ranges = (0..count)
            .map(|i| {
                let end = (i128::from(i64::MIN) + (i as i128 + 1) * width as i128 - 1) as i64;
                let mut salt = base.id.to_vec();
                salt.extend_from_slice(&(i as u32).to_be_bytes());
                salt.extend_from_slice(&timestamp_ms.to_be_bytes());
                (end, stream_id(end, &salt))
            })
            .collect();
        Streams {
            ranges,
            timestamp_ms,
            tablets: true,
        }
    }

    /// The token range `(start, end]` of stream `i`.
    pub fn range(&self, i: usize) -> (i64, i64) {
        let start = if i == 0 {
            i64::MIN
        } else {
            self.ranges[i - 1].0
        };
        (start, self.ranges[i].0)
    }

    /// The stream of a base token (the range `(previous end, end]` holding it, wrapping).
    pub fn of(&self, token: i64) -> [u8; 16] {
        let i = self.ranges.partition_point(|(end, _)| *end < token);
        self.ranges
            .get(i)
            .or_else(|| self.ranges.first())
            .map(|(_, s)| *s)
            .unwrap_or([0; 16])
    }
}

/// Where the tablet count changes of a CDC table's tablet group (its base table and log,
/// which ScyllaDB co-locates) are recorded: `ms:from:to` entries, `;`-separated, in the
/// `extensions` of the table the `ALTER TABLE … WITH tablets` named (persisted with the schema).
pub const RESIZES: &str = "roll_cdc_tablet_resizes";

/// A tablet count change of a CDC table group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Resize {
    ms: i64,
    from: usize,
    to: usize,
}

fn resizes(t: &Table) -> Vec<Resize> {
    let Some(b) = t.params.extensions.get(RESIZES) else {
        return Vec::new();
    };
    String::from_utf8_lossy(b)
        .split(';')
        .filter_map(|e| {
            let mut it = e.split(':').map(|x| x.parse::<i64>().ok());
            let (ms, from, to) = (it.next()??, it.next()??, it.next()??);
            Some(Resize {
                ms,
                from: from.max(1) as usize,
                to: to.max(1) as usize,
            })
        })
        .collect()
}

/// The `min_tablet_count` hint of a table.
fn min_tablets(p: &TableParams) -> usize {
    p.tablets
        .get("min_tablet_count")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| *v >= 1.0)
        .map_or(0, |v| v.ceil() as usize)
}

/// The tablet count of a CDC table group in a tablets keyspace: the keyspace's initial count
/// (its default when 0), raised to the larger `min_tablet_count` of the base and the log, a
/// power of two.
pub fn group_tablet_count(ks: &Keyspace, base: &TableParams, log: &TableParams) -> usize {
    tablet_count(ks.tablets.unwrap_or(0))
        .max(min_tablets(base).max(min_tablets(log)).next_power_of_two())
        .min(1 << 16)
}

/// The stream sets of a CDC table in a tablets keyspace, oldest first: the one created with the
/// log, then one per tablet count change (ScyllaDB starts a stream generation whenever the
/// table's tablets split or merge).
pub fn generations(ks: &Keyspace, base: &Table, log: &Table) -> Vec<Streams> {
    let mut changes = resizes(base);
    changes.extend(resizes(log));
    changes.sort_by_key(|r| r.ms);
    let first = changes
        .first()
        .map_or_else(|| group_tablet_count(ks, &base.params, &log.params), |r| r.from);
    let mut out = vec![Streams::tablets(base, log, first)];
    for r in changes {
        out.push(Streams::tablets_at(base, r.to, r.ms));
    }
    out
}

/// Records a tablet count change of `t`'s CDC table group made by `ALTER TABLE t WITH tablets`
/// (`new`: `t`'s new options), at `now_ms`.
pub fn note_tablet_resize(ks: &Keyspace, t: &Table, new: &mut TableParams, now_ms: i64) {
    if ks.tablets.is_none() {
        return;
    }
    let (base, log) = if is_log(ks, t) {
        let Some(b) = t.name.strip_suffix(SUFFIX).and_then(|b| ks.tables.get(b)) else {
            return;
        };
        (b.clone(), None)
    } else {
        match log_of(ks, &t.name) {
            Some(l) => (Arc::new(t.clone()), Some(l.clone())),
            None => return,
        }
    };
    let (old, now) = match &log {
        // `t` is the base.
        Some(l) => (
            group_tablet_count(ks, &base.params, &l.params),
            group_tablet_count(ks, new, &l.params),
        ),
        // `t` is the log.
        None => (
            group_tablet_count(ks, &base.params, &t.params),
            group_tablet_count(ks, &base.params, new),
        ),
    };
    if old == now {
        return;
    }
    // Generations must start after the previous one.
    let last = generations(ks, &base, log.as_deref().unwrap_or(t))
        .last()
        .map_or(0, |s| s.timestamp_ms);
    let ms = now_ms.max(last + 1);
    let mut text = new
        .extensions
        .get(RESIZES)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();
    if !text.is_empty() {
        text.push(';');
    }
    text.push_str(&format!("{ms}:{old}:{now}"));
    new.extensions
        .insert(RESIZES.to_string(), bytes::Bytes::from(text.into_bytes()));
}

/// The Unix time (milliseconds) of a timeuuid.
pub fn unix_millis(u: &[u8; 16]) -> i64 {
    let low = u32::from_be_bytes([u[0], u[1], u[2], u[3]]) as u64;
    let mid = u16::from_be_bytes([u[4], u[5]]) as u64;
    let hi = (u16::from_be_bytes([u[6], u[7]]) & 0x0FFF) as u64;
    let t = (hi << 48) | (mid << 32) | low;
    (t.saturating_sub(0x01B2_1DD2_1381_4000) / 10_000) as i64
}

/// The node's vnode tokens (the CDC generation's range ends).
pub fn generation(cfg: &DbConfig) -> Vec<i64> {
    crate::system::local::tokens(&cfg.host_id(), cfg.num_tokens)
}

/// A v1 UUID at `micros` with random clock and node bits (`cdc$time`).
fn timeuuid_at(micros: i64) -> [u8; 16] {
    let t = (micros as u64)
        .wrapping_mul(10)
        .wrapping_add(0x01B2_1DD2_1381_4000);
    let mut u = [0u8; 16];
    u[0..4].copy_from_slice(&((t & 0xFFFF_FFFF) as u32).to_be_bytes());
    u[4..6].copy_from_slice(&(((t >> 32) & 0xFFFF) as u16).to_be_bytes());
    u[6..8].copy_from_slice(&((((t >> 48) & 0x0FFF) as u16) | 0x1000).to_be_bytes());
    let r: [u8; 8] = rand::random();
    u[8..16].copy_from_slice(&r);
    u[8] = (u[8] & 0x3F) | 0x80;
    u
}

/// The write timestamp of an update (its newest cell, marker or deletion).
fn write_timestamp(u: &PartitionUpdate) -> i64 {
    let mut ts = if u.deletion.is_live() {
        i64::MIN
    } else {
        u.deletion.marked_at
    };
    for rt in &u.range_tombstones {
        ts = ts.max(rt.deletion.marked_at);
    }
    if let Some(s) = &u.static_row {
        ts = ts.max(row_timestamp(s));
    }
    for r in &u.rows {
        ts = ts.max(row_timestamp(r));
    }
    ts
}

/// A row's write timestamp: collection-wide tombstones are `T - 1` (compensated, as ScyllaDB).
fn row_timestamp(r: &Row) -> i64 {
    let mut ts = r.liveness.timestamp.max(r.deletion.time.marked_at);
    for c in &r.columns {
        match c {
            ColumnData::Simple { cell, .. } => ts = ts.max(cell.timestamp),
            ColumnData::Complex {
                deletion, cells, ..
            } => {
                if !deletion.is_live() {
                    ts = ts.max(deletion.marked_at + 1);
                }
                for c in cells {
                    ts = ts.max(c.timestamp);
                }
            }
        }
    }
    ts
}

/// The log rows of one group being built.
struct Group<'a> {
    spec: &'a LogSpec,
    base: &'a Table,
    key: Vec<Option<Bytes>>,
    time: [u8; 16],
    timestamp: i64,
    now: i64,
    rows: Vec<Row>,
}

impl Group<'_> {
    /// Starts a log row: keys, operation, TTL.
    fn row(
        &self,
        op: i8,
        clustering: Option<&Clustering>,
        ttl: Option<i32>,
    ) -> Vec<(u32, Option<Bytes>)> {
        let mut cells: Vec<(u32, Option<Bytes>)> = vec![(
            self.spec.operation,
            Some(Bytes::copy_from_slice(&[op as u8])),
        )];
        if let Some(t) = ttl {
            cells.push((
                self.spec.ttl_column,
                Some(Bytes::copy_from_slice(&(t as i64).to_be_bytes())),
            ));
        }
        for lc in &self.spec.columns {
            let col = &self.base.columns[lc.base];
            match col.kind {
                ColumnKind::PartitionKey => {
                    cells.push((
                        lc.value,
                        self.key.get(col.position as usize).cloned().flatten(),
                    ));
                }
                ColumnKind::Clustering => {
                    if let Some(c) = clustering {
                        cells.push((lc.value, c.get(col.position as usize)));
                    }
                }
                _ => {}
            }
        }
        cells
    }

    fn push(&mut self, cells: Vec<(u32, Option<Bytes>)>) {
        let seq = self.rows.len() as i32;
        let clustering = Clustering::new([&self.time[..], &seq.to_be_bytes()[..]]);
        let ttl = self.spec.ttl;
        let (liveness, cell) = if ttl > 0 {
            let exp = self.now + ttl;
            (
                LivenessInfo::expiring(self.timestamp, ttl as i32, exp),
                Box::new(move |ts: i64, v: Bytes| Cell::expiring(ts, ttl as i32, exp, v))
                    as Box<dyn Fn(i64, Bytes) -> Cell>,
            )
        } else {
            (
                LivenessInfo::live(self.timestamp),
                Box::new(|ts: i64, v: Bytes| Cell::live(ts, v)) as Box<dyn Fn(i64, Bytes) -> Cell>,
            )
        };
        let mut row = Row::new(clustering).with_liveness(liveness);
        for (id, v) in cells {
            if let Some(v) = v {
                row = row.with_cell(id, cell(self.timestamp, v));
            }
        }
        row.columns.sort_by_key(|c| c.column());
        row.columns.dedup_by_key(|c| c.column());
        self.rows.push(row);
    }

    fn column(&self, base: usize) -> Option<&LogColumn> {
        self.spec.columns.iter().find(|c| c.base == base)
    }

    /// An image row (pre: `deleted` flags for nulls; post: values only) of `columns`.
    fn image(
        &mut self,
        op: i8,
        clustering: Option<&Clustering>,
        row: Option<&Row>,
        columns: &[usize],
    ) {
        self.image_of(op, clustering, row, columns, None);
    }

    /// An image; for a post-image, `emptied` is the write and the row before it: a non-frozen
    /// collection the write emptied shows as empty rather than null (as on the reference).
    fn image_of(
        &mut self,
        op: i8,
        clustering: Option<&Clustering>,
        row: Option<&Row>,
        columns: &[usize],
        emptied: Option<(&Row, Option<&Row>)>,
    ) {
        let mut cells = self.row(op, clustering, None);
        for &bi in columns {
            let Some(lc) = self.column(bi).cloned() else {
                continue;
            };
            let col = &self.base.columns[bi];
            let v =
                row.and_then(|r| crate::storage::column_value(self.base, col, r, self.now, None));
            let v = v.map(|v| log_value(&col.ty, row, col.id, v));
            let v = v.or_else(|| {
                let (write, before) = emptied?;
                let had = before
                    .and_then(|b| crate::storage::column_value(self.base, col, b, self.now, None));
                (col.ty.is_multicell()
                    && col.ty.is_collection()
                    && write.column(col.id).is_some()
                    && had.is_some())
                .then(|| Bytes::from_static(&[0, 0, 0, 0]))
            });
            match v {
                Some(v) => cells.push((lc.value, Some(v))),
                None if op == 0 => {
                    if let Some(d) = lc.deleted {
                        cells.push((d, Some(Bytes::from_static(&[1]))));
                    }
                }
                None => {}
            }
        }
        self.push(cells);
    }

    /// The delta rows of one written row (static or clustering).
    ///
    /// Learned from the reference: when every part of the write has the same TTL class (dead
    /// parts count as "no TTL"), one row holds it all (operation 2 for an insert, 1 otherwise).
    /// Otherwise an insert's row marker gets its own operation 2 row (with the atomic cells of its
    /// TTL), and the rest is logged as updates, one row per TTL class, the TTL-less class first.
    fn delta(&mut self, row: &Row, clustering: Option<&Clustering>) {
        let insert = clustering.is_some() && !row.liveness.is_empty();
        let op: i8 = if insert { 2 } else { 1 };
        if !self.spec.delta_full {
            self.push(self.row(op, clustering, None));
            return;
        }
        // Every part: (TTL class, log cell, whether it is an atomic live cell).
        let mut parts: Vec<Part> = Vec::new();
        for data in &row.columns {
            let Some(bi) = self.base.columns.iter().position(|c| c.id == data.column()) else {
                continue;
            };
            let Some(lc) = self.column(bi).cloned() else {
                continue;
            };
            let col = &self.base.columns[bi];
            let flag = Some(Bytes::from_static(&[1]));
            match data {
                ColumnData::Simple { cell, .. } => {
                    if cell.is_tombstone() {
                        if let Some(d) = lc.deleted {
                            parts.push((None, (d, flag), false));
                        }
                    } else {
                        let ttl = cell.is_expiring().then_some(cell.ttl);
                        parts.push((ttl, (lc.value, Some(cell.value.clone())), true));
                    }
                }
                ColumnData::Complex {
                    deletion, cells, ..
                } => {
                    if !deletion.is_live()
                        && let Some(d) = lc.deleted
                    {
                        parts.push((None, (d, flag), false));
                    }
                    let live: Vec<&Cell> = cells.iter().filter(|c| !c.is_tombstone()).collect();
                    let dead: Vec<&[u8]> = cells
                        .iter()
                        .filter(|c| c.is_tombstone())
                        .filter_map(|c| c.path.as_deref())
                        .collect();
                    if !live.is_empty() {
                        let ttl = live.first().filter(|c| c.is_expiring()).map(|c| c.ttl);
                        parts.push((ttl, (lc.value, Some(added(&col.ty, &live))), false));
                    }
                    if !dead.is_empty()
                        && let Some(d) = lc.deleted_elements
                    {
                        parts.push((None, (d, Some(nosql_types::pack_collection(dead))), false));
                    }
                }
            }
        }
        let marker_ttl = (insert && row.liveness.is_expiring()).then_some(row.liveness.ttl);
        let uniform = parts.iter().all(|(t, _, _)| *t == marker_ttl)
            || (!insert && parts.windows(2).all(|w| w[0].0 == w[1].0));
        if uniform {
            let ttl = if insert {
                marker_ttl
            } else {
                parts.first().and_then(|p| p.0)
            };
            let mut c = self.row(op, clustering, ttl);
            c.extend(parts.into_iter().map(|(_, cell, _)| cell));
            self.push(c);
            return;
        }
        let mut rest = parts;
        if insert {
            let mut c = self.row(2, clustering, marker_ttl);
            let (mine, others): (Vec<_>, Vec<_>) = rest
                .into_iter()
                .partition(|(t, _, atomic)| *atomic && *t == marker_ttl);
            c.extend(mine.into_iter().map(|(_, cell, _)| cell));
            self.push(c);
            rest = others;
        }
        let mut groups: BTreeMap<Option<i32>, Vec<(u32, Option<Bytes>)>> = BTreeMap::new();
        for (ttl, cell, _) in rest {
            groups.entry(ttl).or_default().push(cell);
        }
        for (ttl, cells) in groups {
            let mut c = self.row(1, clustering, ttl);
            c.extend(cells);
            self.push(c);
        }
    }

    fn mark_end(&mut self) {
        if let Some(last) = self.rows.last_mut() {
            let ts = self.timestamp;
            let cell = match self.spec.ttl {
                t if t > 0 => Cell::expiring(ts, t as i32, self.now + t, Bytes::from_static(&[1])),
                _ => Cell::live(ts, Bytes::from_static(&[1])),
            };
            last.columns
                .push(ColumnData::simple(self.spec.end_of_batch, cell));
            last.columns.sort_by_key(|c| c.column());
        }
    }
}

/// The value of a non-frozen collection as the log's frozen type (a list as `map<timeuuid, V>`).
fn log_value(ty: &CqlType, row: Option<&Row>, id: u32, whole: Bytes) -> Bytes {
    if let (CqlType::List { frozen: false, .. }, Some(row)) = (ty, row)
        && let Some(ColumnData::Complex {
            cells, deletion, ..
        }) = row.column(id)
    {
        let live: Vec<&Cell> = cells
            .iter()
            .filter(|c| !c.is_tombstone() && !deletion.deletes(c.timestamp))
            .collect();
        return added(ty, &live);
    }
    whole
}

/// The added elements of a non-frozen collection write.
fn added(ty: &CqlType, cells: &[&Cell]) -> Bytes {
    match ty {
        CqlType::Set { .. } => {
            nosql_types::pack_collection(cells.iter().filter_map(|c| c.path.as_deref()))
        }
        CqlType::Udt(def, _) => {
            let mut fields: Vec<Option<&[u8]>> = vec![None; def.fields.len()];
            for c in cells {
                if let Some(p) = c.path.as_deref().filter(|p| p.len() == 2) {
                    let i = u16::from_be_bytes([p[0], p[1]]) as usize;
                    if i < fields.len() {
                        fields[i] = Some(&c.value);
                    }
                }
            }
            nosql_types::pack_fields(fields)
        }
        _ => nosql_types::pack_map(
            cells
                .iter()
                .filter_map(|c| c.path.as_deref().map(|p| (p, &c.value[..]))),
        ),
    }
}

/// The non-key columns of a row kind (static or regular), by base column index.
fn columns_of(base: &Table, kind: ColumnKind) -> Vec<usize> {
    base.columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.kind == kind)
        .map(|(i, _)| i)
        .collect()
}

/// The base columns a row write touches.
fn touched_columns(base: &Table, r: &Row) -> Vec<usize> {
    r.columns
        .iter()
        .filter_map(|d| base.columns.iter().position(|c| c.id == d.column()))
        .collect()
}

/// The range tombstone bound rows (operations 5–8).
fn bound(g: &mut Group, b: &nosql_storage::Bound, start: bool) {
    let (inclusive, exclusive) = if start { (5, 6) } else { (7, 8) };
    let op = if b.prefix.is_empty() {
        inclusive
    } else if start == b.after {
        exclusive
    } else {
        inclusive
    };
    let c = (!b.prefix.is_empty()).then(|| b.prefix.clone());
    let cells = g.row(op, c.as_ref(), None);
    g.push(cells);
}

tokio::task_local! {
    /// Set while a service (not the client) writes: per-row TTL expiration.
    static SERVICE: ();
}

/// Runs `f` as a service write: its row and partition deletions are logged as operations -3
/// (`service_row_delete`) and -4 (`service_partition_delete`).
pub async fn as_service<F: std::future::Future>(f: F) -> F::Output {
    SERVICE.scope((), f).await
}

/// The log rows of one touched base partition.
pub fn log_updates(
    db: &Database,
    spec: &LogSpec,
    t: &Touched,
) -> Result<Vec<(TableId, PartitionUpdate)>> {
    let base = &**t.base;
    let key: Vec<Option<Bytes>> = crate::storage::key_components(base, &t.key.key);
    let stream = spec.streams.of(t.key.token);
    let pk = crate::storage::partition_key(&[Bytes::copy_from_slice(&stream)], db.dialect(), true)?;
    // One group per write timestamp, in statement order.
    let mut order: Vec<i64> = Vec::new();
    for u in &t.updates {
        let ts = write_timestamp(u);
        if !order.contains(&ts) {
            order.push(ts);
        }
    }
    let mut out = PartitionUpdate::new(pk);
    let (row_delete, partition_delete) = if SERVICE.try_with(|_| ()).is_ok() {
        (-3, -4)
    } else {
        (3, 4)
    };
    for ts in order {
        let updates: Vec<&PartitionUpdate> = t
            .updates
            .iter()
            .copied()
            .filter(|u| write_timestamp(u) == ts)
            .collect();
        let mut g = Group {
            spec,
            base,
            key: key.clone(),
            time: timeuuid_at(ts),
            timestamp: ts,
            now: t.now,
            rows: Vec::new(),
        };
        let statics = columns_of(base, ColumnKind::Static);
        let regulars = columns_of(base, ColumnKind::Regular);
        // Pre-images of rows that existed.
        if spec.preimage != Image::Off {
            for u in &updates {
                if let Some(s) = &u.static_row
                    && let Some(e) = &t.existing_static
                {
                    let cols = if spec.preimage == Image::Full {
                        statics.clone()
                    } else {
                        touched_columns(base, s)
                    };
                    g.image(0, None, Some(e), &cols);
                }
                for r in &u.rows {
                    let Some(e) = t.existing_row(&r.clustering) else {
                        continue;
                    };
                    let delete = !r.deletion.is_live() && r.columns.is_empty();
                    let cols = if spec.preimage == Image::Full || delete {
                        regulars.clone()
                    } else {
                        touched_columns(base, r)
                    };
                    g.image(0, Some(&r.clustering), Some(e), &cols);
                }
                // In a table without clustering columns, a partition deletion pre-images the
                // row it deletes (Alternator's DeleteItem of a hash-only table is one, and its
                // stream record carries the old item); with clustering columns it doesn't.
                if base.clustering.is_empty() && !u.deletion.is_live() && u.rows.is_empty() {
                    for e in t.existing.values() {
                        g.image(0, None, Some(e), &regulars);
                    }
                }
            }
        }
        // Deltas.
        for u in &updates {
            if let Some(s) = &u.static_row {
                g.delta(s, None);
            }
            for r in &u.rows {
                if !r.deletion.is_live() {
                    let cells = g.row(row_delete, Some(&r.clustering), None);
                    g.push(cells);
                    if r.columns.is_empty() && r.liveness.is_empty() {
                        continue;
                    }
                }
                g.delta(r, Some(&r.clustering));
            }
            for rt in &u.range_tombstones {
                bound(&mut g, &rt.start, true);
                bound(&mut g, &rt.end, false);
            }
            if !u.deletion.is_live() {
                let cells = g.row(partition_delete, None, None);
                g.push(cells);
            }
        }
        // Post-images of rows written (not deleted).
        if spec.postimage {
            for u in &updates {
                if u.static_row.is_some() {
                    g.image(9, None, t.merged_static.as_ref(), &statics);
                }
                for r in &u.rows {
                    if !r.deletion.is_live() && r.columns.is_empty() {
                        continue;
                    }
                    let m = t.merged_row(&r.clustering);
                    let before = t.existing_row(&r.clustering);
                    g.image_of(9, Some(&r.clustering), m, &regulars, Some((r, before)));
                }
            }
        }
        g.mark_end();
        out.rows.extend(g.rows);
    }
    if out.rows.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![(spec.log.table_id(), out)])
}
