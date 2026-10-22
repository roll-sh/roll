//! Messages and warnings of derived data (indexes, views, CDC), copied from the references.

/// ScyllaDB, `CREATE INDEX` in a tablets keyspace.
pub const TABLETS_INDEX_WARNING: &str = "Creating an index in a keyspace that uses tablets requires the keyspace to remain RF-rack-valid while the index exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack.";

/// ScyllaDB, `CREATE MATERIALIZED VIEW` in a tablets keyspace (sic: "keyspaces").
pub const TABLETS_VIEW_WARNING: &str = "Creating a materialized view in a keyspaces that uses tablets requires the keyspace to remain RF-rack-valid while the materialized view exists. Some operations will be restricted to enforce this: altering the keyspace's replication factor, adding a node in a new rack, and removing or decommissioning a node that would eliminate a rack.";

/// Cassandra `StorageAttachedIndex.VECTOR_USAGE_WARNING`.
pub const SAI_VECTOR_WARNING: &str = "SAI ANN indexes on vector columns are experimental and are not recommended for production use.\nThey don't yet support SELECT queries with:\n * Consistency level higher than ONE/LOCAL_ONE.\n * Paging.\n * No LIMIT clauses.\n * PER PARTITION LIMIT clauses.\n * GROUP BY clauses.\n * Aggregation functions.\n * Filters on columns without a SAI index.";

/// Cassandra `SASIIndex.USAGE_WARNING`.
pub const SASI_WARNING: &str =
    "SASI indexes are experimental and are not recommended for production use.";

/// Cassandra `View.USAGE_WARNING` (`CREATE MATERIALIZED VIEW` with views enabled).
pub const VIEW_WARNING: &str =
    "Materialized views are experimental and are not recommended for production use.";

/// ScyllaDB, `cdc` in `CREATE` / `ALTER MATERIALIZED VIEW` (whatever its value).
pub const VIEW_CDC: &str = "Cannot enable CDC for a materialized view";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts() {
        assert!(TABLETS_INDEX_WARNING.starts_with("Creating an index in a keyspace"));
        assert!(TABLETS_VIEW_WARNING.contains("in a keyspaces that uses tablets"));
        assert!(SAI_VECTOR_WARNING.ends_with("without a SAI index."));
        assert!(SASI_WARNING.starts_with("SASI"));
        assert!(VIEW_WARNING.starts_with("Materialized"));
        assert!(VIEW_CDC.ends_with("materialized view"));
    }
}
