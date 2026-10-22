//! The schema: keyspaces, tables, columns, user types, and the definitions later subsystems
//! attach (indexes, views, functions, aggregates, triggers).
//!
//! A [`Schema`] is an immutable snapshot. The database holds an `Arc<Schema>` and swaps it on
//! every change, so statements prepared or executing against one snapshot never see a half-applied
//! change. Schema is persisted as data in the `system_schema` tables ([`persist`]); built-in
//! keyspaces ([`crate::system`]) are compiled in and never stored.
//!
//! Column order follows Cassandra's `TableMetadata.columns()`: partition key and clustering
//! columns by position, then static and regular columns each sorted by name bytes. That order is
//! `SELECT *`'s.

pub mod compact;
pub mod compaction_options;
pub mod ddl;
pub mod params;
pub mod persist;
pub mod replication;
pub mod types;

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use nosql_storage::ClusteringComparator;
use nosql_storage::manifest::TableId;
use nosql_types::{CqlType, Dialect, UdtDef};

pub use params::TableParams;
pub use replication::Replication;

/// What a column is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ColumnKind {
    PartitionKey,
    Clustering,
    Static,
    Regular,
}

impl ColumnKind {
    /// The `system_schema.columns.kind` value.
    pub fn name(self) -> &'static str {
        match self {
            ColumnKind::PartitionKey => "partition_key",
            ColumnKind::Clustering => "clustering",
            ColumnKind::Static => "static",
            ColumnKind::Regular => "regular",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "partition_key" => ColumnKind::PartitionKey,
            "clustering" => ColumnKind::Clustering,
            "static" => ColumnKind::Static,
            "regular" => ColumnKind::Regular,
            _ => return None,
        })
    }

    pub fn is_primary_key(self) -> bool {
        matches!(self, ColumnKind::PartitionKey | ColumnKind::Clustering)
    }
}

/// A column of a table.
#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: CqlType,
    pub kind: ColumnKind,
    /// Position within the partition key or clustering columns; -1 for the others.
    pub position: i32,
    /// A clustering column declared `DESC`.
    pub reversed: bool,
    /// Storage column id.
    pub id: u32,
}

impl Column {
    pub fn is_primary_key(&self) -> bool {
        self.kind.is_primary_key()
    }

    pub fn is_static(&self) -> bool {
        self.kind == ColumnKind::Static
    }

    pub fn is_counter(&self) -> bool {
        self.ty.is_counter()
    }

    /// The `system_schema.columns.clustering_order` value.
    pub fn clustering_order(&self, dialect: Dialect) -> &'static str {
        let s = match (self.kind, self.reversed) {
            (ColumnKind::Clustering, true) => "desc",
            (ColumnKind::Clustering, false) => "asc",
            _ => "none",
        };
        if dialect.is_scylla() {
            match s {
                "desc" => "DESC",
                "asc" => "ASC",
                _ => "NONE",
            }
        } else {
            s
        }
    }
}

/// A dropped column (`system_schema.dropped_columns`).
#[derive(Clone, Debug, PartialEq)]
pub struct DroppedColumn {
    /// Microseconds: cells written at or before this are ignored.
    pub dropped_at: i64,
    pub ty: CqlType,
    pub kind: ColumnKind,
}

/// `system_schema.tables.flags`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TableFlags {
    pub compound: bool,
    pub dense: bool,
    pub counter: bool,
    pub super_: bool,
}

impl TableFlags {
    pub fn names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.compound {
            v.push("compound");
        }
        if self.counter {
            v.push("counter");
        }
        if self.dense {
            v.push("dense");
        }
        if self.super_ {
            v.push("super");
        }
        v
    }
}

/// A secondary index definition (`system_schema.indexes`). Index implementations live in their
/// own module (see `crate::hooks::IndexProvider`); the core only stores and lists them.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexMeta {
    pub name: String,
    /// `COMPOSITES`, `KEYS` or `CUSTOM`.
    pub kind: String,
    pub options: BTreeMap<String, String>,
}

/// A materialized view's link to its base (`system_schema.views`); the view itself is a
/// [`Table`] of its keyspace.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewMeta {
    pub base_table_id: [u8; 16],
    pub base_table_name: String,
    pub where_clause: String,
    pub include_all_columns: bool,
    /// ScyllaDB computed columns (`system_schema.computed_columns`): name and computation JSON
    /// (`{"type":"token_v2"}`, …) of index views' generated key columns.
    pub computed: Vec<(String, String)>,
}

/// A trigger definition (`system_schema.triggers`).
#[derive(Clone, Debug, PartialEq)]
pub struct TriggerMeta {
    pub name: String,
    pub options: BTreeMap<String, String>,
}

/// A user-defined function (`system_schema.functions`), stored for the UDF subsystem.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionMeta {
    pub name: String,
    pub argument_types: Vec<String>,
    pub argument_names: Vec<String>,
    pub body: String,
    pub called_on_null_input: bool,
    pub language: String,
    pub return_type: String,
}

/// A user-defined aggregate (`system_schema.aggregates`).
#[derive(Clone, Debug, PartialEq)]
pub struct AggregateMeta {
    pub name: String,
    pub argument_types: Vec<String>,
    pub final_func: Option<String>,
    pub initcond: Option<String>,
    pub return_type: String,
    pub state_func: String,
    pub state_type: String,
    /// ScyllaDB `REDUCEFUNC`.
    pub reduce_func: Option<String>,
}

/// A column's dynamic data mask (Cassandra 5 `ColumnMask`, `system_schema.column_masks`): the
/// masking function and the arguments after the first (the column's value), as stored.
/// [`crate::masking`] resolves and applies it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMask {
    pub function_keyspace: String,
    pub function_name: String,
    /// CQL types of the partial arguments (`function_argument_types`).
    pub arg_types: Vec<String>,
    /// Their values as `AbstractType.getString` prints them (`None`: null).
    pub arg_values: Vec<Option<String>>,
}

/// Where a table's rows come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableKind {
    /// A storage table.
    Regular,
    /// A `system_schema` table: stored rows of user keyspaces plus generated rows of the
    /// built-in keyspaces.
    SchemaTable,
    /// Rows generated from engine state ([`crate::system::VirtualTable`]).
    Virtual,
}

/// A table (or materialized view, or index view).
#[derive(Clone, Debug)]
pub struct Table {
    pub keyspace: String,
    pub name: String,
    pub id: [u8; 16],
    /// Changes on every ALTER (prepared statements on the table are invalidated).
    pub version: [u8; 16],
    /// Canonical column order.
    pub columns: Vec<Column>,
    pub by_name: HashMap<String, usize>,
    pub partition_key: Vec<usize>,
    pub clustering: Vec<usize>,
    pub params: TableParams,
    pub flags: TableFlags,
    pub dropped: BTreeMap<String, DroppedColumn>,
    /// Every column name ever used with its storage id (dropped ones keep theirs, so a re-added
    /// column sees its older cells filtered by the drop time, as Cassandra does).
    pub column_ids: BTreeMap<String, u32>,
    pub next_column_id: u32,
    pub indexes: Vec<IndexMeta>,
    pub triggers: Vec<TriggerMeta>,
    pub view: Option<ViewMeta>,
    pub kind: TableKind,
    pub comparator: Arc<EngineComparator>,
    /// Column masks by column name (Cassandra 5 dynamic data masking).
    pub masks: BTreeMap<String, ColumnMask>,
    /// The storage incarnation of `id` ([`storage_id`]): the references let a dropped table's
    /// id be used again (`WITH ID = …`, index tables named after their index), and storage never
    /// reuses a dropped id. Not persisted: it is the first generation storage hasn't dropped
    /// ([`Table::assign_storage`]), at creation and at open.
    pub generation: u32,
    /// The same for the hidden (Cassandra) index tables, by index name.
    pub index_generations: BTreeMap<String, u32>,
}

/// The storage table of schema id `id` in its `generation`th incarnation (the id itself first).
pub fn storage_id(id: [u8; 16], generation: u32) -> TableId {
    if generation == 0 {
        return TableId(u128::from_be_bytes(id));
    }
    let mut name = id.to_vec();
    name.extend_from_slice(b"/generation/");
    name.extend_from_slice(&generation.to_be_bytes());
    TableId(u128::from_be_bytes(crate::uuid::v5(
        &crate::uuid::NAMESPACE_ROLL,
        &name,
    )))
}

/// The first generation of `id`, from `from`, whose storage table was not dropped.
pub fn first_live_generation(
    id: [u8; 16],
    from: u32,
    dropped: &std::collections::BTreeSet<TableId>,
) -> u32 {
    let mut g = from;
    while dropped.contains(&storage_id(id, g)) {
        g += 1;
    }
    g
}

impl Table {
    /// Builds a table from its columns (any order), computing the canonical order and the
    /// comparator. Column ids must already be assigned.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        keyspace: impl Into<String>,
        name: impl Into<String>,
        id: [u8; 16],
        version: [u8; 16],
        mut columns: Vec<Column>,
        params: TableParams,
        flags: TableFlags,
        kind: TableKind,
    ) -> Table {
        columns.sort_by(canonical_order);
        let mut by_name = HashMap::with_capacity(columns.len());
        let mut partition_key = Vec::new();
        let mut clustering = Vec::new();
        for (i, c) in columns.iter().enumerate() {
            by_name.insert(c.name.clone(), i);
            match c.kind {
                ColumnKind::PartitionKey => partition_key.push(i),
                ColumnKind::Clustering => clustering.push(i),
                _ => {}
            }
        }
        let column_ids: BTreeMap<String, u32> =
            columns.iter().map(|c| (c.name.clone(), c.id)).collect();
        let next_column_id = columns.iter().map(|c| c.id + 1).max().unwrap_or(0);
        let comparator = Arc::new(EngineComparator::new(&columns, &clustering));
        Table {
            keyspace: keyspace.into(),
            name: name.into(),
            id,
            version,
            columns,
            by_name,
            partition_key,
            clustering,
            params,
            flags,
            dropped: BTreeMap::new(),
            column_ids,
            next_column_id,
            indexes: Vec::new(),
            triggers: Vec::new(),
            view: None,
            kind,
            comparator,
            masks: BTreeMap::new(),
            generation: 0,
            index_generations: BTreeMap::new(),
        }
    }

    /// The storage table id.
    pub fn table_id(&self) -> TableId {
        storage_id(self.id, self.generation)
    }

    /// Moves the table and its hidden index tables past storage tables that were dropped.
    /// Generations only grow, so a table already attached keeps its storage. Returns whether
    /// anything changed.
    pub fn assign_storage(&mut self, dropped: &std::collections::BTreeSet<TableId>) -> bool {
        let mut changed = false;
        let g = first_live_generation(self.id, self.generation, dropped);
        changed |= g != self.generation;
        self.generation = g;
        for m in &self.indexes {
            let from = self.index_generations.get(&m.name).copied().unwrap_or(0);
            let g = first_live_generation(crate::index::hidden_id(self, &m.name), from, dropped);
            if g != from {
                self.index_generations.insert(m.name.clone(), g);
                changed = true;
            }
        }
        changed
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.by_name.get(name).map(|&i| &self.columns[i])
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }

    pub fn partition_key_columns(&self) -> impl Iterator<Item = &Column> {
        self.partition_key.iter().map(|&i| &self.columns[i])
    }

    pub fn clustering_columns(&self) -> impl Iterator<Item = &Column> {
        self.clustering.iter().map(|&i| &self.columns[i])
    }

    pub fn has_statics(&self) -> bool {
        self.columns.iter().any(|c| c.kind == ColumnKind::Static)
    }

    pub fn is_counter(&self) -> bool {
        self.flags.counter
    }

    pub fn is_view(&self) -> bool {
        self.view.is_some()
    }

    pub fn is_virtual(&self) -> bool {
        self.kind == TableKind::Virtual
    }

    /// Cells of `column` written at or before this timestamp belong to a dropped incarnation.
    pub fn dropped_before(&self, column: &str) -> Option<i64> {
        self.dropped.get(column).map(|d| d.dropped_at)
    }

    /// A copy with a new version and the given changes applied (`ALTER`).
    pub fn rebuild(&self, columns: Vec<Column>, params: TableParams) -> Table {
        let mut t = Table::new(
            self.keyspace.clone(),
            self.name.clone(),
            self.id,
            crate::uuid::timeuuid(),
            columns,
            params,
            self.flags,
            self.kind,
        );
        t.dropped = self.dropped.clone();
        let mut ids = self.column_ids.clone();
        for c in &t.columns {
            ids.insert(c.name.clone(), c.id);
        }
        t.next_column_id = ids
            .values()
            .map(|i| i + 1)
            .max()
            .unwrap_or(0)
            .max(self.next_column_id);
        t.column_ids = ids;
        t.indexes = self.indexes.clone();
        t.triggers = self.triggers.clone();
        t.view = self.view.clone();
        t.generation = self.generation;
        t.index_generations = self.index_generations.clone();
        t.masks = self
            .masks
            .iter()
            .filter(|(c, _)| t.by_name.contains_key(*c))
            .map(|(c, m)| (c.clone(), m.clone()))
            .collect();
        t
    }
}

/// Partition key and clustering by position, then statics, then regulars, each by name bytes.
pub fn canonical_order(a: &Column, b: &Column) -> Ordering {
    let rank = |c: &Column| match c.kind {
        ColumnKind::PartitionKey => 0,
        ColumnKind::Clustering => 1,
        ColumnKind::Static => 2,
        ColumnKind::Regular => 3,
    };
    rank(a).cmp(&rank(b)).then_with(|| {
        if a.kind.is_primary_key() {
            a.position.cmp(&b.position)
        } else {
            a.name.as_bytes().cmp(b.name.as_bytes())
        }
    })
}

/// The table's ordering for storage: clustering components by their type (reversed for `DESC`),
/// complex-column cell paths by the collection's key type.
#[derive(Debug)]
pub struct EngineComparator {
    clustering: Vec<(CqlType, bool)>,
    paths: HashMap<u32, PathOrder>,
}

#[derive(Debug)]
enum PathOrder {
    /// List cells: timeuuid paths.
    Timeuuid,
    Type(CqlType),
    /// UDT field indexes (2-byte big-endian shorts) and anything opaque.
    Bytes,
}

impl EngineComparator {
    pub fn new(columns: &[Column], clustering: &[usize]) -> Self {
        let clustering = clustering
            .iter()
            .map(|&i| (columns[i].ty.clone(), columns[i].reversed))
            .collect();
        let mut paths = HashMap::new();
        for c in columns {
            if !c.ty.is_multicell() {
                continue;
            }
            let order = match &c.ty {
                CqlType::List { .. } => PathOrder::Timeuuid,
                CqlType::Set { elem, .. } => PathOrder::Type((**elem).clone()),
                CqlType::Map { key, .. } => PathOrder::Type((**key).clone()),
                _ => PathOrder::Bytes,
            };
            paths.insert(c.id, order);
        }
        EngineComparator { clustering, paths }
    }

    pub fn clustering_types(&self) -> &[(CqlType, bool)] {
        &self.clustering
    }
}

impl ClusteringComparator for EngineComparator {
    fn compare_component(&self, index: usize, a: &[u8], b: &[u8]) -> Ordering {
        match self.clustering.get(index) {
            Some((ty, reversed)) => nosql_types::compare_with_order(ty, *reversed, a, b),
            None => a.cmp(b),
        }
    }

    fn compare_path(&self, column: u32, a: &[u8], b: &[u8]) -> Ordering {
        match self.paths.get(&column) {
            Some(PathOrder::Timeuuid) => nosql_types::compare_timeuuid(a, b),
            Some(PathOrder::Type(t)) => nosql_types::compare(t, a, b),
            _ => a.cmp(b),
        }
    }
}

/// A keyspace's origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyspaceKind {
    User,
    /// A built-in keyspace (`system`, `system_schema`, …): compiled in, never stored.
    System,
    /// A virtual keyspace (`system_views`, `system_virtual_schema`).
    Virtual,
}

/// A keyspace.
#[derive(Clone, Debug)]
pub struct Keyspace {
    pub name: String,
    pub durable_writes: bool,
    pub replication: Replication,
    /// ScyllaDB: tablets keyspace with its `initial_tablets` (`system_schema.scylla_keyspaces`).
    pub tablets: Option<i32>,
    pub kind: KeyspaceKind,
    pub tables: BTreeMap<String, Arc<Table>>,
    pub types: BTreeMap<String, Arc<UdtDef>>,
    pub functions: Vec<FunctionMeta>,
    pub aggregates: Vec<AggregateMeta>,
    /// Cassandra's `column_masks` rows that outlive their column or table ((table, column) →
    /// mask): dropping, renaming or dropping the table doesn't delete them
    /// ([`crate::masking::carry_stale`]).
    pub stale_masks: BTreeMap<(String, String), ColumnMask>,
}

impl Keyspace {
    pub fn new(name: impl Into<String>, replication: Replication, kind: KeyspaceKind) -> Self {
        Keyspace {
            name: name.into(),
            durable_writes: true,
            replication,
            tablets: None,
            kind,
            tables: BTreeMap::new(),
            types: BTreeMap::new(),
            functions: Vec::new(),
            aggregates: Vec::new(),
            stale_masks: BTreeMap::new(),
        }
    }

    pub fn is_builtin(&self) -> bool {
        self.kind != KeyspaceKind::User
    }

    /// Tables that use user type `name` (directly or nested), excluding views.
    pub fn tables_using_type(&self, name: &str) -> Vec<&str> {
        self.tables
            .values()
            .filter(|t| {
                t.columns
                    .iter()
                    .any(|c| c.ty.references_udt(&self.name, name))
            })
            .map(|t| t.name.as_str())
            .collect()
    }
}

/// An immutable schema snapshot.
#[derive(Clone, Debug, Default)]
pub struct Schema {
    pub version: [u8; 16],
    pub keyspaces: BTreeMap<String, Arc<Keyspace>>,
    by_id: HashMap<u128, (String, String)>,
}

impl Schema {
    pub fn new(keyspaces: BTreeMap<String, Arc<Keyspace>>) -> Schema {
        let mut s = Schema {
            version: [0; 16],
            keyspaces,
            by_id: HashMap::new(),
        };
        s.reindex();
        s
    }

    fn reindex(&mut self) {
        self.by_id.clear();
        for ks in self.keyspaces.values() {
            for t in ks.tables.values() {
                self.by_id
                    .insert(u128::from_be_bytes(t.id), (ks.name.clone(), t.name.clone()));
            }
        }
        self.version = self.digest();
    }

    /// A name-based version over everything a client could observe changing.
    fn digest(&self) -> [u8; 16] {
        let mut buf = Vec::new();
        for ks in self.keyspaces.values() {
            if ks.kind != KeyspaceKind::User {
                continue;
            }
            buf.extend_from_slice(ks.name.as_bytes());
            buf.push(0);
            buf.push(ks.durable_writes as u8);
            for (k, v) in ks.replication.as_map() {
                buf.extend_from_slice(k.as_bytes());
                buf.extend_from_slice(v.as_bytes());
            }
            for t in ks.tables.values() {
                buf.extend_from_slice(&t.version);
            }
            for (n, u) in &ks.types {
                buf.extend_from_slice(n.as_bytes());
                for (f, ty) in &u.fields {
                    buf.extend_from_slice(f.as_bytes());
                    buf.extend_from_slice(nosql_types::cql_name(ty).as_bytes());
                }
            }
            // Every field: a replaced function or aggregate is a new version.
            for f in &ks.functions {
                buf.extend_from_slice(format!("{f:?}").as_bytes());
            }
            for a in &ks.aggregates {
                buf.extend_from_slice(format!("{a:?}").as_bytes());
            }
            buf.push(0xff);
        }
        crate::uuid::v3(&buf)
    }

    pub fn keyspace(&self, name: &str) -> Option<&Arc<Keyspace>> {
        self.keyspaces.get(name)
    }

    pub fn table(&self, keyspace: &str, name: &str) -> Option<&Arc<Table>> {
        self.keyspaces.get(keyspace)?.tables.get(name)
    }

    pub fn table_by_id(&self, id: &[u8; 16]) -> Option<&Arc<Table>> {
        let (ks, t) = self.by_id.get(&u128::from_be_bytes(*id))?;
        self.table(ks, t)
    }

    /// A copy with `keyspace` replaced (or added).
    pub fn with_keyspace(&self, keyspace: Keyspace) -> Schema {
        let mut keyspaces = self.keyspaces.clone();
        keyspaces.insert(keyspace.name.clone(), Arc::new(keyspace));
        Schema::new(keyspaces)
    }

    /// A copy without `keyspace`.
    pub fn without_keyspace(&self, name: &str) -> Schema {
        let mut keyspaces = self.keyspaces.clone();
        keyspaces.remove(name);
        Schema::new(keyspaces)
    }

    /// User keyspaces (stored in `system_schema`).
    pub fn user_keyspaces(&self) -> impl Iterator<Item = &Arc<Keyspace>> {
        self.keyspaces
            .values()
            .filter(|k| k.kind == KeyspaceKind::User)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: CqlType, kind: ColumnKind, position: i32, id: u32) -> Column {
        Column {
            name: name.into(),
            ty,
            kind,
            position,
            reversed: false,
            id,
        }
    }

    #[test]
    fn canonical_column_order() {
        let cols = vec![
            col("v", CqlType::Int, ColumnKind::Regular, -1, 0),
            col("ck2", CqlType::Int, ColumnKind::Clustering, 1, 1),
            col("Z", CqlType::Int, ColumnKind::Regular, -1, 2),
            col("s", CqlType::Int, ColumnKind::Static, -1, 3),
            col("pk", CqlType::Int, ColumnKind::PartitionKey, 0, 4),
            col("ck1", CqlType::Int, ColumnKind::Clustering, 0, 5),
            col("a", CqlType::Int, ColumnKind::Regular, -1, 6),
        ];
        let t = Table::new(
            "ks",
            "t",
            [1; 16],
            [2; 16],
            cols,
            TableParams::default(),
            TableFlags::default(),
            TableKind::Regular,
        );
        let names: Vec<&str> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["pk", "ck1", "ck2", "s", "Z", "a", "v"]);
        assert_eq!(t.partition_key, vec![0]);
        assert_eq!(t.clustering, vec![1, 2]);
        assert_eq!(t.next_column_id, 7);
        assert!(t.has_statics());
        assert_eq!(
            t.column("s").unwrap().clustering_order(Dialect::Scylla),
            "NONE"
        );
        assert_eq!(
            t.column("ck1")
                .unwrap()
                .clustering_order(Dialect::Cassandra),
            "asc"
        );
    }

    #[test]
    fn comparator_orders_by_type_and_direction() {
        let mut c = col("ck", CqlType::Int, ColumnKind::Clustering, 0, 1);
        c.reversed = true;
        let cols = vec![
            col("pk", CqlType::Int, ColumnKind::PartitionKey, 0, 0),
            c,
            col(
                "l",
                CqlType::list(CqlType::Int, false),
                ColumnKind::Regular,
                -1,
                2,
            ),
            col(
                "m",
                CqlType::map(CqlType::Int, CqlType::Text, false),
                ColumnKind::Regular,
                -1,
                3,
            ),
        ];
        let t = Table::new(
            "ks",
            "t",
            [1; 16],
            [2; 16],
            cols,
            TableParams::default(),
            TableFlags::default(),
            TableKind::Regular,
        );
        let cmp = &t.comparator;
        assert_eq!(
            cmp.compare_component(0, &1i32.to_be_bytes(), &2i32.to_be_bytes()),
            Ordering::Greater
        );
        assert_eq!(
            cmp.compare_path(3, &(-1i32).to_be_bytes(), &1i32.to_be_bytes()),
            Ordering::Less
        );
        assert_eq!(cmp.compare_path(99, b"a", b"b"), Ordering::Less);
    }
}
