//! Reading Cassandra and ScyllaDB SSTable backups, for `roll nosql import --from s3://…`.
//!
//! - [`descriptor`], [`stats`], [`compression`], [`data`], [`rows`], [`counter`], [`scylla`],
//!   [`types`]: the on-disk formats — Cassandra 3.0–5.0 (`m*`, `nb`, `oa`, BTI `da`) and
//!   ScyllaDB (`me`, `ms`, `mt`), ported from Apache Cassandra 5.0 (Apache-2.0; each module
//!   names the classes) and ScyllaDB's published format documentation.
//! - [`sstable`]: one SSTable's components, opened for a sequential scan of `Data.db`.
//! - [`layout`]: finding the SSTables and schema of a backup: `nodetool snapshot` directories
//!   (Cassandra and ScyllaDB), ScyllaDB Manager backups, Medusa backups; multi-node backups.
//! - [`schema`]: the backup's CQL schema, in the order to run it.
//! - [`load`]: converting partitions into roll SSTables of a target table, with bounded memory.
//! - [`source`]: where backups are read from (`s3://`, `file://`).

pub mod compression;
pub mod counter;
pub mod data;
pub mod descriptor;
pub mod input;
pub mod layout;
pub mod load;
pub mod rows;
pub mod schema;
pub mod scylla;
pub mod source;
pub mod sstable;
pub mod stats;
pub mod tar;
pub mod types;
