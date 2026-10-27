//! The seam between the DynamoDB API and storage.
//!
//! Everything DynamoDB-specific (request parsing, validation, expressions, ReturnValues,
//! pagination tokens, capacity accounting) lives above this trait; a [`Backend`] only stores
//! table schemas and items and answers key-range reads, with the semantics ScyllaDB's CQL layer
//! gives Alternator:
//!
//! - **Items** are [`Item`]s: attribute name to DynamoDB JSON value, key attributes included.
//!   A backend stores them as described in [`crate::storage`] (key columns + the `:attrs` map)
//!   and returns what reading that representation back yields: top-level numbers come back in
//!   ScyllaDB's decimal form, everything else verbatim.
//! - **Keys.** A partition is identified by the CQL serialisation of its hash key value
//!   ([`crate::storage::encode_key`]): `{"N":"1.5"}` and `{"N":"1.50"}` are different
//!   partitions. Inside a partition, rows are ordered and identified by the range key's CQL
//!   comparator (text and blob bytewise, decimal numerically), so `1.5` and `1.50` are the
//!   same row and the row keeps the spelling it was first written with.
//! - **Order.** Partitions are ordered by Murmur3 token of the serialised hash key, then by
//!   key bytes, exactly as a full-table CQL scan returns them.
//! - **Writes** are read-modify-write under a per-item lock ([`Backend::write_item`]): the
//!   backend reads the current item, calls the closure, and applies its decision before
//!   another write to the same item can read. (ScyllaDB does this with LWT under the
//!   `always_use_lwt` write isolation.)
//! - **Indexes.** A GSI is a view keyed by `(index hash, [index range], table hash, [table
//!   range])`; a LSI by `(table hash, index range, [table range])`. Items lacking an index key
//!   attribute, or holding it with the wrong type, are not in the view. Reads through an index
//!   return whole items.
//! - **Streams.** When a table's stream is enabled, every applied write appends a
//!   [`StreamRecord`] holding the old and new item to one of the stream's shards (ScyllaDB: the
//!   CDC stream of the item's token range); the API layer shapes records per view type.

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::json::Json;
use crate::schema::{KeyAttr, TableSchema};
use crate::value::Item;

/// The key of an item: validated attribute values of the hash and range key.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimaryKey {
    pub hash: Json,
    pub range: Option<Json>,
}

/// What a write decided to do with the item.
#[derive(Clone, Debug, PartialEq)]
pub enum Mutation {
    /// PutItem: replace the whole item (key attributes included). In CQL, a row marker, a
    /// tombstone for the whole `:attrs` map and its new elements. `rmw` tells that the request
    /// needed the old item (a condition, `ReturnValues`).
    Put { item: Item, rmw: bool },
    /// UpdateItem: the item after the update. In CQL, cell writes and deletions of the
    /// `touched` top-level attributes; `sets` is false when the update only removes, in which
    /// case a missing item stays missing (but the write still reaches the stream). `reads`
    /// tells that the update expression needed the old item (nested paths, arithmetic, ADD,
    /// ...), `rmw` that the request did (that, a condition or `ReturnValues`).
    Update {
        item: Item,
        sets: bool,
        reads: bool,
        rmw: bool,
        touched: Vec<String>,
    },
    /// DeleteItem: a row deletion (even of a missing item).
    Delete,
}

/// Whether a write appears in a stream of `view_type`, and as what, as ScyllaDB's CDC-based
/// streams decide (with `alternator_streams_increased_compatibility` off):
///
/// - `KEYS_ONLY` and `OLD_IMAGE` record every write, no-ops included. `KEYS_ONLY` can't tell
///   an insert from a modification: puts and updates that only set values are `INSERT`, other
///   updates `MODIFY`. `OLD_IMAGE` uses the old item: `INSERT` without one, else `MODIFY`
///   (an update that only removes is always `MODIFY`).
/// - `NEW_IMAGE` and `NEW_AND_OLD_IMAGES` skip writes that change nothing and name the event by
///   the item's existence before and after.
/// - Deletes are `REMOVE`.
pub fn stream_event(
    view_type: &str,
    m: &Mutation,
    old: Option<&Item>,
    new: Option<&Item>,
) -> Option<ChangeKind> {
    match view_type {
        "NEW_IMAGE" | "NEW_AND_OLD_IMAGES" => match (old, new) {
            (o, n) if o == n => None,
            (None, Some(_)) => Some(ChangeKind::Insert),
            (Some(_), Some(_)) => Some(ChangeKind::Modify),
            _ => Some(ChangeKind::Remove),
        },
        "OLD_IMAGE" => Some(match m {
            Mutation::Delete => ChangeKind::Remove,
            Mutation::Update { sets: false, .. } => ChangeKind::Modify,
            _ if old.is_some() => ChangeKind::Modify,
            _ => ChangeKind::Insert,
        }),
        _ => Some(match m {
            Mutation::Delete => ChangeKind::Remove,
            Mutation::Update { sets: false, .. } | Mutation::Update { reads: true, .. } => {
                ChangeKind::Modify
            }
            _ => ChangeKind::Insert,
        }),
    }
}

/// A write's decision function: sees the current item (if any) and returns what to apply, or
/// `None` to leave the item as it is. An error aborts the write (e.g. a failed condition).
pub type WriteFn = Box<dyn FnOnce(Option<&Item>) -> Result<Option<Mutation>> + Send>;

/// The item before and after a write, as the write saw and produced them (the new item is the
/// one given to [`Mutation::Put`], not re-read from storage).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WriteOutcome {
    pub old: Option<Item>,
    pub new: Option<Item>,
}

/// A condition on the range key for a single-partition read.
#[derive(Clone, Debug, PartialEq)]
pub enum SortCondition {
    Eq(Json),
    Lt(Json),
    Le(Json),
    Gt(Json),
    Ge(Json),
    Between(Json, Json),
    BeginsWith(Json),
}

/// Which rows a read covers.
#[derive(Clone, Debug, PartialEq)]
pub enum Scope {
    /// One partition of the table or index: Query.
    Partition {
        hash: Json,
        sort: Option<SortCondition>,
    },
    /// Every partition whose token is in `[start, end]`: Scan (one segment of it).
    Tokens { start: i64, end: i64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RangeRequest {
    /// Read through this GSI or LSI.
    pub index: Option<String>,
    pub scope: Scope,
    /// Resume strictly after this position: an item holding the table's key attributes and,
    /// for an index read, the index's key attributes.
    pub start_after: Option<Item>,
    /// Ascending (`ScanIndexForward`) within a partition.
    pub forward: bool,
    /// Stop after this many rows.
    pub limit: usize,
    /// Stop once the rows read amount to at least this many bytes (of [`crate::size`]).
    pub max_bytes: usize,
}

/// Rows read, in order; `more` when the read stopped because of `limit` or `max_bytes` rather
/// than because the scope was exhausted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RangePage {
    pub items: Vec<Item>,
    pub more: bool,
    /// Where to resume when the read stopped after `query_tombstone_page_limit` tombstones
    /// past the last item (the key attributes of that position); `None`: after the last item.
    pub resume: Option<Item>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Insert,
    Modify,
    Remove,
}

/// 100 ns intervals between 1582-10-15 and the Unix epoch.
const UUID_EPOCH_OFFSET: u64 = 0x01B2_1DD2_1381_4000;

/// The low half of a stream's first position (a min-timeuuid's clock and node bits).
pub const MIN_CLOCK: u64 = 0x8080_8080_8080_8080;

/// A position in a stream shard, as ScyllaDB orders CDC log rows: the 60-bit timestamp of the
/// `cdc$time` timeuuid (100 ns since 1582-10-15) in the high half, its clock and node bits in
/// the low half. SequenceNumbers are its decimal.
pub fn stream_position(ms: i64, low: u64) -> u128 {
    let ticks = (ms.max(0) as u64)
        .wrapping_mul(10_000)
        .wrapping_add(UUID_EPOCH_OFFSET);
    (u128::from(ticks) << 64) | u128::from(low)
}

/// The position of a timeuuid (`cdc$time`).
pub fn timeuuid_position(u: &[u8; 16]) -> u128 {
    let low = u32::from_be_bytes([u[0], u[1], u[2], u[3]]) as u64;
    let mid = u16::from_be_bytes([u[4], u[5]]) as u64;
    let hi = (u16::from_be_bytes([u[6], u[7]]) & 0x0FFF) as u64;
    let ticks = (hi << 48) | (mid << 32) | low;
    let mut rest = [0u8; 8];
    rest.copy_from_slice(&u[8..]);
    (u128::from(ticks) << 64) | u128::from(u64::from_be_bytes(rest))
}

/// The milliseconds since the epoch of a position.
pub fn position_ms(p: u128) -> i64 {
    (((p >> 64) as u64).saturating_sub(UUID_EPOCH_OFFSET) / 10_000) as i64
}

/// One shard of a table's stream (ScyllaDB: one CDC stream, of a vnode or a tablet).
#[derive(Clone, Debug, PartialEq)]
pub struct StreamShard {
    /// `H <generation ms, hex>:<stream id, 32 hex digits>`.
    pub id: String,
    /// The position the shard starts at (its generation's timestamp).
    pub start: u128,
    /// Where a shard of an older generation ends (the next generation's start).
    pub end: Option<u128>,
    /// The shard of the previous generation whose token range holds this one's end.
    pub parent: Option<String>,
    /// The shards of the next generation whose token ranges overlap this one's.
    pub children: Vec<String>,
}

/// One change of an item in a table's stream.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamRecord {
    /// Increasing within the record's shard ([`stream_position`]).
    pub position: u128,
    /// Milliseconds since the epoch.
    pub time_ms: i64,
    pub kind: ChangeKind,
    pub keys: Item,
    pub old: Option<Item>,
    pub new: Option<Item>,
    /// A deletion by the TTL service (DynamoDB's `userIdentity` of `dynamodb.amazonaws.com`).
    pub expired: bool,
}

/// A CQL permission an operation needs (`alternator_enforce_authorization`), as ScyllaDB maps
/// them: reads need SELECT, writes MODIFY (even when they read), CreateTable CREATE on all
/// keyspaces, DeleteTable DROP, UpdateTable / tags / TTL ALTER on the table; reads through a GSI
/// or LSI need SELECT on its view, GetRecords SELECT on the CDC log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Select,
    Modify,
    Create,
    Drop,
    Alter,
}

impl Permission {
    pub fn name(self) -> &'static str {
        match self {
            Permission::Select => "SELECT",
            Permission::Modify => "MODIFY",
            Permission::Create => "CREATE",
            Permission::Drop => "DROP",
            Permission::Alter => "ALTER",
        }
    }
}

/// What a permission is checked on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resource {
    /// `ALL KEYSPACES`.
    AllKeyspaces,
    /// A CQL table or view (`alternator_T.T`, `alternator_T."T:gsi"`, the CDC log, a system
    /// table).
    Table { keyspace: String, table: String },
}

impl Resource {
    /// The CQL table of Alternator table `table` (or of its view or log `cql_table`).
    pub fn of(table: &str, cql_table: &str) -> Resource {
        Resource::Table {
            keyspace: crate::storage::keyspace_name(table),
            table: cql_table.to_string(),
        }
    }
}

/// Audit categories (ScyllaDB's `audit_categories`).
pub use nosql_engine::audit::Category as AuditCategory;

/// One `audit.audit_log` row of a request ([`crate::audit`]).
#[derive(Clone, Debug)]
pub struct AuditEvent {
    pub category: AuditCategory,
    /// The consistency level's name, or empty.
    pub consistency: &'static str,
    pub keyspace: String,
    pub table: String,
    /// `<Op>|<request JSON>`.
    pub operation: String,
    pub error: bool,
    /// The authenticated role.
    pub user: Option<String>,
    pub source: Option<std::net::IpAddr>,
}

/// A request being traced (ScyllaDB's probabilistic tracing and slow query log).
#[async_trait]
pub trait RequestTrace: Send {
    /// A trace event.
    fn event(&mut self, activity: &str);
    /// Ends the trace and writes it (`response_size`: bytes of the response).
    async fn end(self: Box<Self>, response_size: usize);
}

/// What a traced request is.
#[derive(Clone, Debug)]
pub struct TraceStart {
    /// The operation (`PutItem`).
    pub op: String,
    /// The session parameters: `alternator_op`, `query`, `table` (or `table[0]`, …).
    pub parameters: std::collections::BTreeMap<String, String>,
    /// The tables the request uses.
    pub tables: std::collections::BTreeSet<String>,
    pub user: Option<String>,
    pub client: Option<std::net::IpAddr>,
    pub request_size: usize,
}

/// The rows of an internal (CQL system) table, as items.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InternalTable {
    /// The CQL primary key columns: partition key then clustering columns (`S` or `B`). A Query
    /// conditions the first of each; LastEvaluatedKey holds them all.
    pub key: Vec<KeyAttr>,
    /// Every row, in CQL order (null cells absent).
    pub rows: Vec<Item>,
    /// The token of each row's partition (Scan segments select by it).
    pub tokens: Vec<i64>,
    /// How many of `key` are partition key columns.
    pub partition_key: usize,
}

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Creates a table; `false` if one with that name exists.
    async fn create_table(&self, schema: TableSchema) -> Result<bool>;

    /// Drops a table and its data, returning its last schema.
    async fn delete_table(&self, name: &str) -> Result<Option<TableSchema>>;

    async fn table(&self, name: &str) -> Result<Option<Arc<TableSchema>>>;

    /// All table names, in the order ListTables returns them.
    async fn list_tables(&self) -> Result<Vec<String>>;

    /// Replaces a table's schema (UpdateTable, tags, TTL). `false` if the table is gone.
    async fn alter_table(&self, schema: TableSchema) -> Result<bool>;

    async fn get_item(&self, table: &TableSchema, key: &PrimaryKey) -> Result<Option<Item>>;

    /// Read-modify-write of one item under its lock.
    async fn write_item(
        &self,
        table: &TableSchema,
        key: &PrimaryKey,
        f: WriteFn,
    ) -> Result<WriteOutcome>;

    async fn read_range(&self, table: &TableSchema, req: &RangeRequest) -> Result<RangePage>;

    /// The shards of a table's stream, in the order DescribeStream lists them.
    async fn stream_shards(&self, table: &TableSchema) -> Result<Vec<StreamShard>>;

    /// Records of one shard at `from` (`inclusive`) or after it, oldest first, at most `limit`,
    /// written no later than `until_ms`.
    async fn stream_records(
        &self,
        table: &TableSchema,
        shard: &str,
        from: u128,
        inclusive: bool,
        until_ms: i64,
        limit: usize,
    ) -> Result<Vec<StreamRecord>>;

    /// Whether `role` holds `perm` on `on` (or on something containing it, directly or through
    /// a granted role); `AccessDeniedException` otherwise. Superusers hold everything.
    async fn authorize(&self, _role: &str, _perm: Permission, _on: &Resource) -> Result<()> {
        Ok(())
    }

    /// The bytes a table's data takes (DescribeTable's `TableSizeBytes`; ScyllaDB counts
    /// SSTables only).
    async fn table_size_bytes(&self, _table: &TableSchema) -> Result<u64> {
        Ok(0)
    }

    /// Whether `role` is a superuser (or permissions aren't enforced): writing system tables
    /// needs it, whatever was granted (ScyllaDB's issue #23218).
    async fn is_superuser(&self, _role: &str) -> Result<bool> {
        Ok(true)
    }

    /// `alternator_streams_increased_compatibility` (see [`crate::Config`]).
    fn set_streams_increased_compatibility(&self, _on: bool) {}

    /// Gives `role` every permission on what it just created (a table, an index's view), as
    /// ScyllaDB auto-grants them. (Dropping a table or index revokes everyone's permissions on
    /// it: the backend does that itself.)
    async fn grant_creator(&self, _role: &str, _on: &Resource) -> Result<()> {
        Ok(())
    }

    /// One of ScyllaDB's internal tables (`system.local`, ...), for
    /// `.scylla.alternator.<keyspace>.<table>` reads; `None` if there is no such table.
    async fn internal_table(&self, _keyspace: &str, _table: &str) -> Result<Option<InternalTable>> {
        Ok(None)
    }

    /// Whether ScyllaDB's audit logging could log a request of `category` at all (a cheap
    /// check before [`Backend::audit_wants`]); never by default.
    fn audit_may_log(&self, _category: AuditCategory) -> bool {
        false
    }

    /// Whether a request of `category` on table `keyspace.table` (both empty: no table) by
    /// `user` is audited.
    fn audit_wants(
        &self,
        _category: AuditCategory,
        _keyspace: &str,
        _table: &str,
        _user: Option<&str>,
    ) -> bool {
        false
    }

    /// Writes an audit row.
    async fn audit(&self, _event: AuditEvent) {}

    /// The scheduling group `role`'s requests run in (`sl:<service level>`), as ScyllaDB labels
    /// its storage latency metrics.
    async fn scheduling_group(&self, _role: Option<&str>) -> String {
        "sl:default".into()
    }

    /// Starts tracing a request when tracing picks it (`None`: not traced).
    fn trace(&self, _start: TraceStart) -> Option<Box<dyn RequestTrace>> {
        None
    }
}
