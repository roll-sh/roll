//! Limits shared by both references.

/// `Attributes.MAX_TTL`: 20 years.
pub const MAX_TTL: i32 = 630_720_000;
/// Partition key components and keys (`FBUtilities.MAX_UNSIGNED_SHORT`).
pub const MAX_KEY_LENGTH: usize = 65_535;
/// Bind markers per statement.
pub const MAX_MARKERS: usize = 65_535;
/// Elements of a collection.
pub const MAX_COLLECTION_ELEMENTS: usize = 65_535;
/// Default page size when the client doesn't page (internal paging of aggregates).
pub const INTERNAL_PAGE: usize = 10_000;
/// ScyllaDB ends a page once its rows reach this many bytes, whatever the page size (the
/// built-in "short page" size limit of paged queries, `docs/dev/paged-queries.md`). A page size
/// of 0 doesn't page in either reference.
pub const SCYLLA_PAGE_BYTES: u64 = 1 << 20;

/// ScyllaDB's largest vector dimension.
pub const SCYLLA_MAX_VECTOR_DIMENSION: i64 = 16_000;

/// ScyllaDB's `max_relations_in_where_clause` default: more relations are a syntax error.
pub const SCYLLA_MAX_RELATIONS: usize = 100;
