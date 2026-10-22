//! ScyllaDB's large data guardrails, for tables created or altered with
//! `large_data_guardrails_enabled = true` (ScyllaDB `docs/cql/guardrails.rst`).
//!
//! - **Coordinator checks** look at the mutation itself: a simple cell's value larger than
//!   `large_cell_fail_threshold_mb`, a collection write with more elements than
//!   `large_collection_elements_fail_threshold`, or a row whose written cells add up to more
//!   than `large_row_fail_threshold_mb`, is refused with an InvalidRequest. Conditional
//!   statements are checked too.
//! - **Replica checks** look at what is already stored (not for LWT, whose Paxos learn is
//!   exempt, nor for a `DELETE … BYPASS LARGE_DATA_GUARDRAILS`): a write to a partition whose
//!   flushed size or row count exceeds `large_partition_fail_threshold_mb` /
//!   `rows_count_fail_threshold`, to a row larger than `large_row_fail_threshold_mb`, or to a
//!   collection with more elements than `large_collection_elements_fail_threshold`, fails as a
//!   `WriteFailure` ("Write rejected for ks.t: …"). Partitions count their SSTables only (the
//!   reference reads SSTable metadata); rows and collections their merged state.
//! - The `compaction_*_warning_threshold` soft limits produce one warning per write naming
//!   the categories ("Large data guardrail: Soft limit violation for row, cell") when
//!   `large_data_cql_warnings` is on, and count in `scylla_database_large_*` metrics.
//!
//! A hard limit of 0 is off. Sizes are the cells' value (and path) bytes plus the clustering
//! key; the reference's on-disk sizes add a little format overhead, which the limits, in MiB,
//! don't notice. The replica checks read the partition before the write, so the guardrails
//! cost a read per write — the reason they are opt-in per table.

use std::sync::Arc;

use futures::StreamExt;
use nosql_storage::read::{ClusteringFilter, PartitionRead};
use nosql_storage::{ColumnData, PartitionUpdate, Row};

use crate::config::LargeData;
use crate::database::Database;
use crate::error::{Consistency, Error, Result};
use crate::guardrails::Metrics;
use crate::schema::Table;

const MIB: u64 = 1024 * 1024;

/// How a write reaches the guardrails.
#[derive(Clone, Copy, Debug)]
pub struct Write {
    pub consistency: Consistency,
    /// `SIMPLE`, `BATCH`, `UNLOGGED_BATCH`, `COUNTER`, `CAS` (the replica failure's write type).
    pub write_type: &'static str,
    /// Run the replica checks (not for LWT or a bypassing DELETE).
    pub replica: bool,
}

/// Soft limit categories, in the reference's order.
#[derive(Default)]
struct Soft {
    partition: bool,
    row: bool,
    cell: bool,
    collection: bool,
}

impl Soft {
    fn warning(&self) -> Option<String> {
        let names: Vec<&str> = [
            (self.partition, "partition"),
            (self.row, "row"),
            (self.cell, "cell"),
            (self.collection, "collection"),
        ]
        .into_iter()
        .filter_map(|(on, n)| on.then_some(n))
        .collect();
        (!names.is_empty()).then(|| {
            format!(
                "Large data guardrail: Soft limit violation for {}",
                names.join(", ")
            )
        })
    }
}

fn over(n: u64, limit: u64) -> bool {
    limit > 0 && n > limit
}

/// Checks one write (a statement or a whole batch); returns its warnings.
pub async fn check(
    db: &Arc<Database>,
    writes: &[(&Table, &PartitionUpdate)],
    w: Write,
) -> Result<Vec<String>> {
    if !db.dialect().is_scylla() || !writes.iter().any(|(t, _)| t.params.large_data_guardrails) {
        return Ok(Vec::new());
    }
    let settings = db.settings();
    let l = &settings.scylla.large_data;
    let m = db.guardrail_metrics();
    let mut soft = Soft::default();
    for (t, u) in writes.iter().filter(|(t, _)| t.params.large_data_guardrails) {
        coordinator(t, u, l, m, &mut soft)?;
    }
    if w.replica {
        for (t, u) in writes.iter().filter(|(t, _)| t.params.large_data_guardrails) {
            replica(db, t, u, l, m, &mut soft, w).await?;
        }
    }
    Ok(match soft.warning() {
        Some(warning) if l.cql_warnings => vec![warning],
        _ => Vec::new(),
    })
}

/// The written size of a row: its cells' values and paths, and its clustering.
fn row_size(r: &Row) -> u64 {
    let mut n = r.clustering.encoded().len() as u64;
    for c in &r.columns {
        match c {
            ColumnData::Simple { cell, .. } => n += cell.value.len() as u64,
            ColumnData::Complex { cells, .. } => {
                for cell in cells {
                    n += cell.value.len() as u64 + cell.path.as_ref().map_or(0, |p| p.len() as u64);
                }
            }
        }
    }
    n
}

fn column_name(t: &Table, id: u32) -> &str {
    t.columns
        .iter()
        .find(|c| c.id == id)
        .map_or("", |c| c.name.as_str())
}

/// `clustering_key=2:x` (components as text, `:`-separated).
fn clustering_text(t: &Table, r: &Row, d: nosql_types::Dialect) -> String {
    r.clustering
        .components()
        .zip(t.clustering_columns())
        .map(|(v, c)| {
            nosql_types::to_text(&c.ty, v, d)
                .ok()
                .flatten()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(":")
}

/// `partition_key=pk{000400000001}`: the key's components, each with a 16-bit length.
fn partition_text(t: &Table, key: &[u8]) -> String {
    let parts: Vec<&[u8]> = if t.partition_key.len() > 1 {
        nosql_types::split_composite(key).unwrap_or_else(|| vec![key])
    } else {
        vec![key]
    };
    let mut hex = String::new();
    for p in parts {
        hex.push_str(&format!("{:04x}", p.len() as u16));
        for b in p {
            hex.push_str(&format!("{b:02x}"));
        }
    }
    format!("pk{{{hex}}}")
}

fn coordinator(t: &Table, u: &PartitionUpdate, l: &LargeData, m: &Metrics, soft: &mut Soft) -> Result<()> {
    let ks = &t.keyspace;
    let d = nosql_types::Dialect::Scylla;
    for r in u.static_row.iter().chain(&u.rows) {
        let is_static = u.static_row.as_ref().is_some_and(|s| std::ptr::eq(s, r));
        for c in &r.columns {
            match c {
                ColumnData::Simple { column, cell } => {
                    let n = cell.value.len() as u64;
                    if over(n, l.cell_fail_mb * MIB) {
                        return Err(Error::invalid(format!(
                            "Large data guardrail: cell value size {n} exceeds hard limit {} (keyspace={ks}, table={}, column={})",
                            l.cell_fail_mb * MIB,
                            t.name,
                            column_name(t, *column)
                        )));
                    }
                    if over(n, l.cell_warn_mb * MIB) {
                        soft.cell = true;
                        Metrics::inc(&m.large_cell_exceeding_threshold);
                    }
                }
                ColumnData::Complex { column, cells, .. } => {
                    let n = cells.iter().filter(|c| !c.is_tombstone()).count() as u64;
                    if over(n, l.collection_fail) {
                        return Err(Error::invalid(format!(
                            "Large data guardrail: collection element count {n} exceeds hard limit {} (keyspace={ks}, table={}, column={})",
                            l.collection_fail,
                            t.name,
                            column_name(t, *column)
                        )));
                    }
                    if over(n, l.collection_warn) {
                        soft.collection = true;
                        Metrics::inc(&m.large_collection_exceeding_threshold);
                    }
                }
            }
        }
        let n = row_size(r);
        if over(n, l.row_fail_mb * MIB) {
            return Err(Error::invalid(if is_static {
                format!(
                    "Large data guardrail: static row size {n} exceeds hard limit {} (keyspace={ks}, table={})",
                    l.row_fail_mb * MIB,
                    t.name
                )
            } else {
                format!(
                    "Large data guardrail: row size {n} exceeds hard limit {} (keyspace={ks}, table={}, clustering_key={})",
                    l.row_fail_mb * MIB,
                    t.name,
                    clustering_text(t, r, d)
                )
            }));
        }
        if over(n, l.row_warn_mb * MIB) {
            soft.row = true;
            Metrics::inc(&m.large_rows_exceeding_threshold);
        }
    }
    Ok(())
}

async fn replica(
    db: &Arc<Database>,
    t: &Table,
    u: &PartitionUpdate,
    l: &LargeData,
    m: &Metrics,
    soft: &mut Soft,
    w: Write,
) -> Result<()> {
    let ks = &t.keyspace;
    let d = db.dialect();
    let reject = |what: String| {
        crate::guardrails::config_table::write_failure(
            db,
            format!("Write rejected for {ks}.{}: Large data guardrail: {what}", t.name),
            w.consistency,
            w.write_type,
        )
    };
    let st = db.storage_table(t)?;
    let now = db.now_secs();
    // The partition, as flushed.
    if l.partition_fail_mb > 0 || l.partition_warn_mb > 0 || l.rows_fail > 0 || l.rows_warn > 0 {
        let mut stream = st.read_flushed(PartitionRead::new(u.key.clone(), now));
        let (mut size, mut rows) = (0u64, 0u64);
        while let Some(c) = stream.next().await {
            let c = c.map_err(Error::internal)?;
            for r in c.static_row.iter().chain(&c.rows) {
                size += row_size(r);
            }
            rows += c.rows.len() as u64;
        }
        size += u.key.key.len() as u64;
        let pk = partition_text(t, &u.key.key);
        if rows > 0 && over(size, l.partition_fail_mb * MIB) {
            return Err(reject(format!(
                "partition size {size} exceeds hard limit {} (keyspace={ks}, table={}, partition_key={pk})",
                l.partition_fail_mb * MIB,
                t.name
            )));
        }
        if over(rows, l.rows_fail) {
            return Err(reject(format!(
                "partition row count {rows} exceeds hard limit {} (keyspace={ks}, table={}, partition_key={pk})",
                l.rows_fail, t.name
            )));
        }
        if (rows > 0 && over(size, l.partition_warn_mb * MIB)) || over(rows, l.rows_warn) {
            soft.partition = true;
            Metrics::inc(&m.large_partition_exceeding_threshold);
        }
    }
    // The rows and collections written, as stored.
    let names: Vec<nosql_storage::Clustering> = u.rows.iter().map(|r| r.clustering.clone()).collect();
    if names.is_empty() && u.static_row.is_none() {
        return Ok(());
    }
    let mut read = PartitionRead::new(u.key.clone(), now);
    if !t.clustering.is_empty() {
        read.clustering = ClusteringFilter::Names(names);
    }
    let mut stream = st.read(read);
    let mut existing: Vec<Row> = Vec::new();
    while let Some(c) = stream.next().await {
        let c = c.map_err(Error::internal)?;
        existing.extend(c.static_row);
        existing.extend(c.rows);
    }
    for written in u.static_row.iter().chain(&u.rows) {
        let is_static = u.static_row.as_ref().is_some_and(|s| std::ptr::eq(s, written));
        let Some(old) = existing.iter().find(|e| {
            if is_static {
                e.clustering.is_empty() && !t.clustering.is_empty()
            } else {
                e.clustering == written.clustering
            }
        }) else {
            continue;
        };
        let ck = clustering_text(t, written, d);
        let n = row_size(old);
        if over(n, l.row_fail_mb * MIB) {
            return Err(reject(format!(
                "row size {n} exceeds hard limit {} (keyspace={ks}, table={}, clustering_key={ck})",
                l.row_fail_mb * MIB,
                t.name
            )));
        }
        if over(n, l.row_warn_mb * MIB) {
            soft.row = true;
            Metrics::inc(&m.large_rows_exceeding_threshold);
        }
        for c in &written.columns {
            let ColumnData::Complex { column, .. } = c else {
                continue;
            };
            let count = old
                .columns
                .iter()
                .find_map(|o| match o {
                    ColumnData::Complex { column: oc, cells, .. } if oc == column => {
                        Some(cells.iter().filter(|c| !c.is_tombstone()).count() as u64)
                    }
                    _ => None,
                })
                .unwrap_or(0);
            if over(count, l.collection_fail) {
                return Err(reject(format!(
                    "collection element count {count} exceeds hard limit {} (keyspace={ks}, table={}, column={}, clustering_key={ck})",
                    l.collection_fail,
                    t.name,
                    column_name(t, *column)
                )));
            }
            if over(count, l.collection_warn) {
                soft.collection = true;
                Metrics::inc(&m.large_collection_exceeding_threshold);
            }
        }
    }
    Ok(())
}
