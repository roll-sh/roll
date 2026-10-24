//! roll nosql storage: an LSM tree per table, with object storage (S3) as the source of truth.
//!
//! Independent of CQL: the engine supplies, per table, a [`ClusteringComparator`] (clustering
//! components and complex-column cell paths in its types' order), the token of every partition
//! key, dense column ids, and "now" (seconds) for every read. Values stay serialised (`Bytes`).
//!
//! # Layers
//!
//! - [`model`]: Cassandra's storage model — partitions, rows, cells, complex columns, counters,
//!   deletions (partition, range, row incl. shadowable, complex, cell) and liveness/TTL.
//! - [`reconcile`]: which version wins (`Reconcile::{Scylla, Cassandra}` tie-breaks), row
//!   merging and tombstone purging, ported from Cassandra.
//! - [`memtable`], [`sstable`], [`cache`]: the LSM tree's parts; SSTables are our own format
//!   (zstd blocks of ~64 KiB, a sparse index with bloom filter and statistics) read by block
//!   through a node-wide memory + local disk cache with coalesced range GETs.
//! - [`merge`], [`clip`], [`read`]: merged, reconciled reads as async streams of
//!   [`read::PartitionChunk`]s — slices, names, reversed order, column filters, limits, paging,
//!   tombstone counting with a failure threshold.
//! - [`compaction`] + [`table::Table`]: STCS, LCS, TWCS, ICS, UCS; purging after gc grace or the
//!   `tombstone_gc` mode, never past data a tombstone may still shadow.
//! - [`log`], [`manifest`], [`db`], [`store`], [`gc`]: the commit log shipped per node, manifests
//!   and their history, databases, branches, point-in-time restore, garbage collection.
//!
//! # Driving it (the engine)
//!
//! ```text
//! let store = Store::start(objects, StoreConfig { node, .. }).await?;   // one per node
//! let db = store.open(db_id, DbConfig { durability, history_retention_ms }).await?;
//! let t = db.table(table_id, TableOptions { comparator, reconcile, gc, compaction, .. }).await?;
//! t.apply(update).await?;              // or db.apply_batch(..) for a logged batch
//! let rows = t.read(PartitionRead { key, clustering, reversed, columns, limits, options });
//! t.flush().await?; t.compact_major().await?; db.truncate(id).await?; db.drop_table(id).await?;
//! store.unload(db_id).await?;          // idle: flush, final manifest, release memory
//! store.branch(parent, child, at_ms).await?;  store.restore_at(db, at_ms, into).await?;
//! store.collect_garbage(min_age).await?;      // one place per bucket
//! ```
//!
//! - Attach the system tables first (their frames wait in memory until attached), read the
//!   schema, then attach the rest. Call `db.table` again to apply `ALTER TABLE` options.
//! - Writes are normalised (rows and cells sorted, duplicates merged, rows made canonical), so
//!   updates may be built in any order. Expiration times (`now + ttl`) and local deletion
//!   times are computed by the engine with the dialect's overflow policy.
//! - Counters: the engine reads the current cell under its per-partition lock and writes
//!   [`Cell::counter`] with `clock + 1` and the new total (one writer per database).
//! - Reads evaluate TTLs and purgeability at `ReadOptions::now`; `TombstoneOverwhelming` is a
//!   typed error (`anyhow::Error::downcast_ref`).

pub mod cache;
pub mod clip;
pub mod codec;
pub mod compaction;
pub mod compare;
pub mod db;
pub mod gc;
pub mod ingest;
pub mod log;
pub mod manifest;
pub mod memory;
pub mod memtable;
pub mod merge;
pub mod model;
pub mod options;
pub mod read;
pub mod reconcile;
pub mod sstable;
pub mod store;
pub mod table;
pub mod tombstones;

/// Failure injection for tests: manifest and SSTable writes whose object key contains an armed
/// pattern fail (patterns name a test's own database, so parallel tests don't interfere).
#[cfg(test)]
pub(crate) mod failpoint {
    static ARMED: parking_lot::Mutex<Vec<String>> = parking_lot::Mutex::new(Vec::new());

    pub fn arm(pattern: &str) {
        ARMED.lock().push(pattern.to_string());
    }

    pub fn disarm(pattern: &str) {
        ARMED.lock().retain(|p| p != pattern);
    }

    pub fn check(key: &str) -> anyhow::Result<()> {
        match ARMED.lock().iter().find(|p| key.contains(p.as_str())) {
            Some(p) => anyhow::bail!("injected failure ({p}) writing {key}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;

pub use compare::{BytesComparator, ClusteringComparator};
pub use pg_storage::{Objects, StorageConfig};

/// Object storage for tests: S3 under a unique prefix when `ROLL_E2E_S3_BUCKET` is set (with
/// `ROLL_E2E_S3_ENDPOINT` and the usual AWS credentials), otherwise a fresh temp directory.
pub fn test_objects(name: &str) -> Objects {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    let unique = format!("{name}-{}-{n}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
    let cfg = match std::env::var("ROLL_E2E_S3_BUCKET") {
        Ok(bucket) => StorageConfig::S3 {
            bucket,
            region: std::env::var("AWS_REGION").ok(),
            endpoint: std::env::var("ROLL_E2E_S3_ENDPOINT").ok(),
            prefix: Some(format!("nosql-tests/{unique}")),
        },
        Err(_) => StorageConfig::Local(std::env::temp_dir().join(format!("roll-nosql-{unique}"))),
    };
    Objects::new(&cfg).expect("test object storage")
}
pub use model::*;
pub use reconcile::Reconcile;
