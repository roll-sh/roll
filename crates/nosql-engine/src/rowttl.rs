//! ScyllaDB's per-row TTL (`docs/cql/cql-extensions.md`, "Per-row TTL"): one regular column of
//! type `timestamp`, `bigint` or `int` holds the time its row expires (milliseconds for a
//! timestamp, seconds since the epoch otherwise).
//!
//! - It is chosen with `CREATE TABLE t (..., e timestamp TTL)` or `ALTER TABLE t TTL e`, and
//!   removed with `ALTER TABLE t TTL NULL` or by dropping the column. As on ScyllaDB, the choice
//!   is the `system:ttl_attribute` tag of the `scylla_tags` extension (Alternator's TTL tag), so
//!   DESCRIBE shows both the column's `TTL` and the tag, and removing it leaves an empty tag map.
//! - Expiration is eventual, as on ScyllaDB: every `alternator_ttl_period_in_seconds` an
//!   expiration pass ([`Database::expire_rows`]) scans the tables with a TTL column and deletes
//!   the rows whose time has passed, through the normal write path (views follow). Until then
//!   the row reads and writes as usual. In the CDC log the deletion is a service operation:
//!   `cdc$operation` -3 (`service_row_delete`) or, without clustering columns, -4
//!   (`service_partition_delete`).
//! - Expiration times more than five years in the past are ignored (the row never expires).
//! - The pass is counted in `scylla_expiration_*` metrics ([`Expiration`]).
//!
//! A tag naming no column (Alternator's TTL attribute lives in its attribute map), or a column
//! of another type, is not a per-row TTL column.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::StreamExt;
use nosql_cql::ast;
use nosql_storage::read::{ClusteringFilter, PartitionRead, RangeRead};
use nosql_storage::{Clustering, DeletionTime, PartitionKey, PartitionUpdate, Row, RowDeletion};
use nosql_types::CqlType;

use crate::database::Database;
use crate::error::{Error, Result};
use crate::schema::params::{parse_scylla_map_blob, scylla_map_blob};
use crate::schema::{Column, ColumnKind, Table, TableParams};

/// The tag naming the TTL column.
pub const TAG: &str = "system:ttl_attribute";

const TAGS: &str = "scylla_tags";

/// The `scylla_tags` of a table (empty without the extension).
fn tags(p: &TableParams) -> std::collections::BTreeMap<String, String> {
    p.extensions
        .get(TAGS)
        .and_then(|b| parse_scylla_map_blob(b))
        .unwrap_or_default()
}

/// The name in the TTL tag.
pub fn tagged(p: &TableParams) -> Option<String> {
    tags(p).remove(TAG)
}

/// Sets or removes the TTL tag (removing keeps the extension, as an empty map).
pub fn set_tag(p: &mut TableParams, column: Option<&str>) {
    let mut t = tags(p);
    match column {
        Some(c) => t.insert(TAG.to_string(), c.to_string()),
        None => t.remove(TAG),
    };
    p.extensions.insert(TAGS.to_string(), scylla_map_blob(&t));
}

/// The table's per-row TTL column (a regular `timestamp`, `bigint` or `int`).
pub fn column(t: &Table) -> Option<&Column> {
    let name = tagged(&t.params)?;
    t.column(&name)
        .filter(|c| c.kind == ColumnKind::Regular && allowed(&c.ty))
}

fn allowed(ty: &CqlType) -> bool {
    matches!(ty, CqlType::Timestamp | CqlType::Bigint | CqlType::Int)
}

fn type_error(name: &str, ty: &CqlType, quoted: bool) -> Error {
    let name = if quoted {
        format!("'{name}'")
    } else {
        name.to_string()
    };
    Error::invalid(format!(
        "TTL column {name} must be of type timestamp, bigint or int, can't be {}",
        nosql_types::cql_name(ty)
    ))
}

/// CREATE TABLE, per declared column in order: a second TTL column, then its type.
pub fn check_declared(seen: &mut Option<String>, name: &str, ty: &CqlType) -> Result<()> {
    if let Some(first) = seen {
        return Err(Error::invalid(format!(
            "Cannot have more than one TTL column in a table. Saw {first} and {name}"
        )));
    }
    if !allowed(ty) {
        return Err(type_error(name, ty, true));
    }
    *seen = Some(name.to_string());
    Ok(())
}

/// CREATE TABLE, once the key is known: the TTL column must be regular.
pub fn check_kind(name: &str, kind: ColumnKind) -> Result<()> {
    match kind {
        ColumnKind::PartitionKey | ColumnKind::Clustering => Err(Error::invalid(format!(
            "TTL column {name} cannot be part of the PRIMARY KEY"
        ))),
        ColumnKind::Static => Err(Error::invalid(format!(
            "TTL column {name} cannot be a static column"
        ))),
        ColumnKind::Regular => Ok(()),
    }
}

/// `ALTER TABLE t TTL c` / `TTL NULL`: the new params, or the error.
pub fn alter(t: &Table, columns: &[Column], target: Option<&ast::Ident>) -> Result<TableParams> {
    let full = format!("{}.{}", t.keyspace, t.name);
    let current = tagged(&t.params).filter(|c| columns.iter().any(|x| &x.name == c));
    let mut p = t.params.clone();
    match target {
        None => {
            if current.is_none() {
                return Err(Error::invalid(format!(
                    "Cannot unset TTL column, table {full} does not have a TTL column set"
                )));
            }
            set_tag(&mut p, None);
        }
        Some(id) => {
            let name = id.name();
            let Some(col) = columns.iter().find(|c| c.name == name) else {
                return Err(Error::invalid(format!(
                    "Column '{name}' does not exist in table {full}"
                )));
            };
            match col.kind {
                ColumnKind::PartitionKey | ColumnKind::Clustering => {
                    return Err(Error::invalid(format!(
                        "Cannot use a primary key column {name} as a TTL column"
                    )));
                }
                ColumnKind::Static => {
                    return Err(Error::invalid(format!(
                        "Cannot use a static column {name} as a TTL column"
                    )));
                }
                ColumnKind::Regular => {}
            }
            if !allowed(&col.ty) {
                return Err(type_error(&name, &col.ty, false));
            }
            if let Some(c) = current {
                return Err(Error::invalid(format!(
                    "Cannot set TTL column, table {full} already has a TTL column defined: {c}"
                )));
            }
            set_tag(&mut p, Some(&name));
        }
    }
    Ok(p)
}

/// Expiration times more than five years old are ignored (DynamoDB's rule, which ScyllaDB's
/// expiration service applies to per-row TTL too).
const MAX_AGE_MS: i64 = 5 * 365 * 86_400 * 1000;

/// When a stored TTL value expires (milliseconds since the epoch).
fn expires_at(ty: &CqlType, v: &[u8]) -> Option<i64> {
    match ty {
        CqlType::Timestamp => Some(i64::from_be_bytes(v.try_into().ok()?)),
        CqlType::Bigint => Some(i64::from_be_bytes(v.try_into().ok()?).saturating_mul(1000)),
        CqlType::Int => Some(i64::from(i32::from_be_bytes(v.try_into().ok()?)) * 1000),
        _ => None,
    }
}

/// Whether a stored row has expired at `now_ms` by its TTL column.
fn expired(t: &Table, col: &Column, row: &Row, now_ms: i64) -> bool {
    let now_s = now_ms.div_euclid(1000);
    if !crate::storage::row_is_live(t, row, now_s) {
        return false;
    }
    crate::storage::column_value(t, col, row, now_s, None)
        .and_then(|v| expires_at(&col.ty, &v))
        .is_some_and(|at| at < now_ms && at > now_ms - MAX_AGE_MS)
}

/// The expiration counters (ScyllaDB's `scylla_expiration_*`, which Alternator's TTL shares).
#[derive(Debug, Default)]
pub struct Expiration {
    /// Passes over every table.
    pub scan_passes: AtomicU64,
    /// Table scans.
    pub scan_table: AtomicU64,
    /// Rows deleted.
    pub items_deleted: AtomicU64,
    /// Secondary ranges scanned (always 0: one node owns every range).
    pub secondary_ranges_scanned: AtomicU64,
}

impl Expiration {
    /// `(name, value)` of each counter, as Prometheus names them.
    pub fn counters(&self) -> [(&'static str, u64); 4] {
        let v = |c: &AtomicU64| c.load(Ordering::Relaxed);
        [
            ("scylla_expiration_items_deleted", v(&self.items_deleted)),
            ("scylla_expiration_scan_passes", v(&self.scan_passes)),
            ("scylla_expiration_scan_table", v(&self.scan_table)),
            (
                "scylla_expiration_secondary_ranges_scanned",
                v(&self.secondary_ranges_scanned),
            ),
        ]
    }
}

impl Database {
    /// The per-row TTL expiration counters.
    pub fn expiration_metrics(&self) -> &Expiration {
        &self.live.expiration
    }

    /// One expiration pass at `now_ms`: deletes the expired rows of every per-row TTL table;
    /// returns how many.
    pub async fn expire_rows(self: &Arc<Self>, now_ms: i64) -> Result<u64> {
        let tables: Vec<Arc<Table>> = self
            .schema()
            .user_keyspaces()
            .flat_map(|k| k.tables.values().cloned().collect::<Vec<_>>())
            .filter(|t| !t.is_view() && column(t).is_some())
            .collect();
        let m = &self.live.expiration;
        let mut deleted = 0;
        for t in tables {
            m.scan_table.fetch_add(1, Ordering::Relaxed);
            deleted += self.expire_table(&t, now_ms).await?;
        }
        m.scan_passes.fetch_add(1, Ordering::Relaxed);
        m.items_deleted.fetch_add(deleted, Ordering::Relaxed);
        Ok(deleted)
    }

    async fn expire_table(self: &Arc<Self>, t: &Arc<Table>, now_ms: i64) -> Result<u64> {
        let Some(col) = column(t) else { return Ok(0) };
        let st = self.storage_table(t)?;
        let mut stream = st.scan(RangeRead::all(now_ms.div_euclid(1000)));
        let mut found: Vec<(PartitionKey, Clustering)> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(Error::internal)?;
            for row in &chunk.rows {
                if expired(t, col, row, now_ms) {
                    found.push((chunk.key.clone(), row.clustering.clone()));
                }
            }
        }
        let mut deleted = 0;
        for (key, clustering) in found {
            // Under the partition's lock, and only if still expired (it may have been updated).
            let _lock = self.lock_partition(&t.id, key.token).await;
            let mut read = PartitionRead::new(key.clone(), now_ms.div_euclid(1000));
            read.clustering = ClusteringFilter::Names(vec![clustering.clone()]);
            let parts = st.read(read).partitions().await.map_err(Error::internal)?;
            let still = parts
                .iter()
                .filter(|p| p.key == key)
                .flat_map(|p| p.rows.iter())
                .any(|r| r.clustering == clustering && expired(t, col, r, now_ms));
            if !still {
                continue;
            }
            let ts = self.timestamp();
            let deletion = DeletionTime::new(ts, now_ms.div_euclid(1000));
            let mut u = PartitionUpdate::new(key);
            if t.clustering.is_empty() {
                u.deletion = deletion;
            } else {
                u.rows
                    .push(Row::new(clustering).with_deletion(RowDeletion::regular(deletion)));
            }
            crate::cdc::as_service(crate::statements::modify::apply_with_observers(
                self,
                t,
                vec![u],
            ))
            .await?;
            deleted += 1;
        }
        Ok(deleted)
    }

    /// Runs [`Database::expire_rows`] every `alternator_ttl_period_in_seconds` while the
    /// database is open (ScyllaDB databases).
    pub(crate) fn spawn_row_expiration(self: &Arc<Self>) {
        if !self.dialect().is_scylla() {
            return;
        }
        let weak: Weak<Database> = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let period = match weak.upgrade() {
                    Some(db) => db.settings().scylla.alternator_ttl_period_in_seconds,
                    None => return,
                };
                tokio::time::sleep(Duration::from_secs_f64(period.clamp(0.01, 1e9))).await;
                let Some(db) = weak.upgrade() else { return };
                if let Err(e) = db.expire_rows(crate::rowttl::now_millis()).await {
                    tracing::warn!(error = %e.message, "row expiration pass failed");
                }
            }
        });
    }
}

/// The wall clock (milliseconds since the epoch).
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}
