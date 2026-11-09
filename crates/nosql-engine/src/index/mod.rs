//! Secondary indexes: ScyllaDB global and local indexes, Cassandra's legacy indexes (2i), SAI,
//! SASI, the vector indexes (Cassandra SAI on `vector<float, n>`, ScyllaDB `vector_index`) and
//! ScyllaDB's `fulltext_index`.
//!
//! # Storage
//!
//! Every index that answers equality lookups is an **index table** with ScyllaDB's index view
//! layout (`secondary_index_manager.cc`'s view schema, as the reference shows it in
//! `system_schema.columns`): the indexed value is the partition key, then the clustering is
//! `idx_token` (the base partition's token), the base partition key and clustering columns, and
//! for `values()` of a list or map the element's cell path (`keys_for_values_idx`). Rows of one
//! indexed value are therefore in base token order, which is the order both products return
//! index results in (Cassandra's `CompositesSearcher` reads an index partition clustered by the
//! base partitioner's order). A ScyllaDB local index (`((pk), c)`) is partitioned by the base
//! partition key and clustered by the indexed column.
//!
//! - In ScyllaDB the index table is a real materialized view (`<index>_index`), visible in
//!   `system_schema.views` with its computed columns and selectable, as the reference shows.
//! - In Cassandra it is hidden: a storage table whose definition is derived from the base table
//!   and the index ([`hidden_table`]), never listed in `system_schema`.
//!
//! Both are maintained by the view machinery (`crate::views`) atomically with each base write,
//! and backfilled when the index is created. Query results are always re-checked against the
//! base row, so an index entry can only make a read cheaper, never change its result.
//!
//! SAI range queries, SASI `LIKE` and vector search read the base table (in token order, or by
//! similarity) instead of an index table; results are exact (see [`vector`]).
//!
//! # Modules
//!
//! - [`ddl`]: `CREATE INDEX` / `DROP INDEX` with each dialect's validation and naming.
//! - [`planner`]: which restrictions an index serves (`Index.supportsExpression`, index groups).
//! - [`sasi`]: Cassandra SASI: options, modes, analyzers and its base-table read.
//! - [`scan`]: reading base rows through an index table.
//! - [`vector`]: `ORDER BY … ANN OF` (exact top-K by similarity).
//! - [`vector_ddl`]: ScyllaDB vector index targets (local partition, filtering columns), their
//!   stored JSON, and the index options.
//! - [`fulltext`]: ScyllaDB `BM25()` searches over a `fulltext_index` (validated; answered as
//!   the reference without a Vector Store answers them).
//!
//! Vector and full-text indexes have no index table: ScyllaDB's Vector Store reads the CDC log
//! the first one creates.

pub mod ddl;
pub mod fulltext;
pub mod planner;
pub mod sasi;
pub mod scan;
pub mod vector;
pub mod vector_ddl;

use std::collections::BTreeMap;
use std::sync::Arc;

use nosql_storage::manifest::TableId;
use nosql_types::{CqlType, Dialect};

use crate::config::DbConfig;
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::Hooks;
use crate::schema::{
    Column, ColumnKind, IndexMeta, Table, TableFlags, TableKind, TableParams, ViewMeta,
};

/// Registers the index subsystem's hooks.
pub fn install(hooks: &mut Hooks, cfg: &DbConfig) {
    hooks.index = Some(Arc::new(planner::Planner::new(cfg.dialect)));
    hooks.statements.push(Arc::new(ddl::Handler));
}

/// What an index target indexes (`IndexTarget.Type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// The column value (a non-collection, or a frozen collection in ScyllaDB's `fl` form).
    Simple,
    /// The elements of a list or set, or the values of a map.
    Values,
    /// The keys of a map (ScyllaDB also stores a set's elements as `keys(st)`).
    Keys,
    /// `(key, value)` entries of a map.
    Entries,
    /// A frozen collection as a whole.
    Full,
}

impl TargetKind {
    pub fn name(self) -> &'static str {
        match self {
            TargetKind::Simple => "",
            TargetKind::Values => "values",
            TargetKind::Keys => "keys",
            TargetKind::Entries => "entries",
            TargetKind::Full => "full",
        }
    }
}

/// The implementation behind an index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexClass {
    /// ScyllaDB's global index / Cassandra's `CassandraIndex` (`COMPOSITES`).
    Secondary,
    /// ScyllaDB local index (`((pk), c)`).
    Local,
    /// Cassandra Storage-Attached Index.
    Sai,
    /// Cassandra SASI (`sasi_indexes_enabled`).
    Sasi,
    /// ScyllaDB `vector_index` (served by the Vector Store service).
    Vector,
    /// ScyllaDB `fulltext_index` (BM25 search, served by the Vector Store service).
    Fulltext,
}

/// A parsed index definition.
#[derive(Clone, Debug)]
pub struct IndexDef {
    pub name: String,
    /// The indexed base column (table column index).
    pub column: usize,
    pub target: TargetKind,
    pub class: IndexClass,
    pub options: BTreeMap<String, String>,
}

impl IndexDef {
    /// Whether this index has an index table (equality lookups).
    pub fn has_table(&self, table: &Table) -> bool {
        match self.class {
            IndexClass::Secondary | IndexClass::Local => true,
            IndexClass::Sai => !table.columns[self.column].ty.is_vector(),
            IndexClass::Sasi | IndexClass::Vector | IndexClass::Fulltext => false,
        }
    }

    /// SAI / SASI / vector: the index serves several restrictions of one query together
    /// (Cassandra's `StorageAttachedIndexGroup`).
    pub fn grouped(&self) -> bool {
        self.class == IndexClass::Sai
    }
}

/// The class names of SAI: the class (exact case) and its short names (any case).
pub fn is_sai_class(c: &str) -> bool {
    c == "org.apache.cassandra.index.sai.StorageAttachedIndex"
        || matches!(
            c.to_ascii_lowercase().as_str(),
            "sai" | "storageattachedindex"
        )
}

pub const SASI_CLASS: &str = "org.apache.cassandra.index.sasi.SASIIndex";

/// Strips one level of CQL identifier quoting.
pub fn unquote(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].replace("\"\"", "\"")
    } else {
        s.to_string()
    }
}

/// Parses `system_schema.indexes.options.target` (`v`, `keys(m)`, `"Quoted"`, ScyllaDB's local
/// index JSON `{"pk":["p"],"ck":["v"]}`): the column name, the kind, and whether it is local.
pub fn parse_target(target: &str) -> Option<(String, TargetKind, bool)> {
    let t = target.trim();
    // A ScyllaDB vector index with a local partition or filtering columns.
    if let Some(v) = vector_ddl::VectorTarget::parse(t) {
        return Some((v.column, TargetKind::Simple, false));
    }
    if t.starts_with('{') {
        // `{"pk":[...],"ck":["v"]}`: the indexed column is the first clustering entry.
        let ck = t.find("\"ck\"")?;
        let rest = &t[ck + 4..];
        let open = rest.find('[')?;
        let close = rest.find(']')?;
        let inner = &rest[open + 1..close];
        let first = inner.split(',').next()?.trim();
        let name = json_string(first)?;
        return Some((name, TargetKind::Simple, true));
    }
    for (prefix, kind) in [
        ("values(", TargetKind::Values),
        ("keys(", TargetKind::Keys),
        ("entries(", TargetKind::Entries),
        ("full(", TargetKind::Full),
    ] {
        if t.len() > prefix.len()
            && t[..prefix.len()].eq_ignore_ascii_case(prefix)
            && t.ends_with(')')
        {
            return Some((unquote(&t[prefix.len()..t.len() - 1]), kind, false));
        }
    }
    Some((unquote(t), TargetKind::Simple, false))
}

/// A JSON string literal's content (`"a\"b"` → `a"b`).
fn json_string(s: &str) -> Option<String> {
    let s = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            out.push(it.next()?);
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// The index definitions of a table (unparseable ones are skipped).
pub fn defs(table: &Table) -> Vec<IndexDef> {
    table.indexes.iter().filter_map(|m| def(table, m)).collect()
}

/// Parses one stored index.
pub fn def(table: &Table, m: &IndexMeta) -> Option<IndexDef> {
    let (name, mut target, local) = parse_target(m.options.get("target")?)?;
    let column = table.column_index(&name)?;
    let col = &table.columns[column];
    let class = if local {
        IndexClass::Local
    } else if m.kind == "CUSTOM" {
        let c = m
            .options
            .get("class_name")
            .map(String::as_str)
            .unwrap_or("");
        if is_sai_class(c) {
            IndexClass::Sai
        } else if c == SASI_CLASS {
            IndexClass::Sasi
        } else if c.eq_ignore_ascii_case("vector_index") {
            IndexClass::Vector
        } else if c.eq_ignore_ascii_case("fulltext_index") {
            IndexClass::Fulltext
        } else {
            return None;
        }
    } else {
        IndexClass::Secondary
    };
    // ScyllaDB stores a frozen collection's full index as the bare column.
    if target == TargetKind::Simple && col.ty.is_frozen_collection() {
        target = TargetKind::Full;
    }
    Some(IndexDef {
        name: m.name.clone(),
        column,
        target,
        class,
        options: m.options.clone(),
    })
}

/// How an index table's generated columns are computed (ScyllaDB's `column_computation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Computed {
    /// `idx_token`: the base partition's token (`token_v2`).
    Token,
    /// `coll_value`: a collection's elements / keys / entries.
    Elements(TargetKind),
    /// `keys_for_values_idx`: the cell path of each value of a list or map.
    Path,
}

impl Computed {
    /// The computation JSON ScyllaDB stores in `system_schema.computed_columns`.
    pub fn json(self, collection: &str) -> String {
        let quoted = json_quote(collection);
        match self {
            Computed::Token => r#"{"type":"token_v2"}"#.to_string(),
            Computed::Elements(k) => format!(
                r#"{{"type":"collection_{}","collection_name":{quoted}}}"#,
                match k {
                    TargetKind::Keys => "keys",
                    TargetKind::Entries => "entries",
                    _ => "values",
                }
            ),
            Computed::Path => {
                format!(r#"{{"type":"collection_keys","collection_name":{quoted}}}"#)
            }
        }
    }

    /// Parses a stored computation.
    pub fn parse(json: &str) -> Option<Computed> {
        if json.contains("\"token_v2\"") || json.contains("\"token\"") {
            return Some(Computed::Token);
        }
        if json.contains("\"collection_values\"") {
            return Some(Computed::Elements(TargetKind::Values));
        }
        if json.contains("\"collection_entries\"") {
            return Some(Computed::Elements(TargetKind::Entries));
        }
        if json.contains("\"collection_keys\"") {
            return Some(Computed::Elements(TargetKind::Keys));
        }
        None
    }
}

fn json_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The element type a collection index's `coll_value` holds.
fn element_type(ty: &CqlType, kind: TargetKind) -> CqlType {
    match (ty, kind) {
        (CqlType::List { elem, .. } | CqlType::Set { elem, .. }, _) => (**elem).clone(),
        (CqlType::Map { key, .. }, TargetKind::Keys) => (**key).clone(),
        (CqlType::Map { key, value, .. }, TargetKind::Entries) => {
            CqlType::Tuple(vec![(**key).clone(), (**value).clone()])
        }
        (CqlType::Map { value, .. }, _) => (**value).clone(),
        (other, _) => other.clone(),
    }
}

/// The columns of an index table (ScyllaDB's index view schema) and its computed columns.
pub fn layout(base: &Table, def: &IndexDef) -> (Vec<Column>, Vec<(String, Computed)>) {
    let target = &base.columns[def.column];
    let mut cols: Vec<Column> = Vec::new();
    let mut computed = Vec::new();
    let mut add = |name: &str, ty: CqlType, kind: ColumnKind, position: i32, reversed: bool| {
        let id = cols.len() as u32;
        cols.push(Column {
            name: name.to_string(),
            ty,
            kind,
            position,
            reversed,
            id,
        });
    };
    if def.class == IndexClass::Local {
        for (i, c) in base.partition_key_columns().enumerate() {
            add(
                &c.name,
                c.ty.clone(),
                ColumnKind::PartitionKey,
                i as i32,
                false,
            );
        }
        let mut pos = 0;
        add(
            &target.name,
            target.ty.clone(),
            ColumnKind::Clustering,
            pos,
            false,
        );
        pos += 1;
        for c in base.clustering_columns() {
            if c.name == target.name {
                continue;
            }
            add(
                &c.name,
                c.ty.clone(),
                ColumnKind::Clustering,
                pos,
                c.reversed,
            );
            pos += 1;
        }
        return (cols, computed);
    }
    let elements = target.ty.is_multicell() && target.ty.is_collection();
    if elements {
        let ty = element_type(&target.ty, def.target);
        let ty = if def.target == TargetKind::Entries {
            ty.freeze()
        } else {
            ty
        };
        // A set's values index is its elements, which ScyllaDB stores as `keys(st)`.
        let kind = match (&target.ty, def.target) {
            (CqlType::Set { .. }, _) => TargetKind::Keys,
            (_, k) => k,
        };
        add("coll_value", ty, ColumnKind::PartitionKey, 0, false);
        computed.push(("coll_value".to_string(), Computed::Elements(kind)));
    } else {
        add(
            &target.name,
            target.ty.clone(),
            ColumnKind::PartitionKey,
            0,
            false,
        );
    }
    add(
        "idx_token",
        CqlType::Bigint,
        ColumnKind::Clustering,
        0,
        false,
    );
    computed.push(("idx_token".to_string(), Computed::Token));
    let mut pos = 1;
    for c in base.partition_key_columns() {
        if c.name == target.name {
            continue;
        }
        add(&c.name, c.ty.clone(), ColumnKind::Clustering, pos, false);
        pos += 1;
    }
    if !target.is_static() {
        for c in base.clustering_columns() {
            if c.name == target.name {
                continue;
            }
            add(
                &c.name,
                c.ty.clone(),
                ColumnKind::Clustering,
                pos,
                c.reversed,
            );
            pos += 1;
        }
    }
    if elements && def.target == TargetKind::Values {
        let path_ty = match &target.ty {
            CqlType::List { .. } => Some(CqlType::Timeuuid),
            CqlType::Map { key, .. } => Some((**key).clone()),
            _ => None,
        };
        if let Some(ty) = path_ty {
            add(
                "keys_for_values_idx",
                ty,
                ColumnKind::Clustering,
                pos,
                false,
            );
            computed.push(("keys_for_values_idx".to_string(), Computed::Path));
        }
    }
    (cols, computed)
}

/// The view metadata of an index table.
pub fn view_meta(base: &Table, def: &IndexDef, computed: &[(String, Computed)]) -> ViewMeta {
    let target = &base.columns[def.column];
    let collection = target.ty.is_multicell() && target.ty.is_collection();
    ViewMeta {
        base_table_id: base.id,
        base_table_name: base.name.clone(),
        where_clause: if collection {
            String::new()
        } else {
            format!(
                "{} IS NOT NULL",
                nosql_types::quote_identifier(&target.name, nosql_types::Dialect::Scylla)
            )
        },
        include_all_columns: false,
        computed: computed
            .iter()
            .map(|(n, c)| (n.clone(), c.json(&target.name)))
            .collect(),
    }
}

/// The index table of `def` as a view table named `name` (`<index>_index` in ScyllaDB), in a
/// keyspace with `tablets` or not.
pub fn index_table(
    base: &Table,
    def: &IndexDef,
    name: &str,
    id: [u8; 16],
    d: Dialect,
    tablets: bool,
) -> Table {
    let (cols, computed) = layout(base, def);
    let mut params = TableParams::defaults(d, false);
    // ScyllaDB's local index views are updated synchronously, and (with tablets) their
    // tombstones expire by timeout.
    if d.is_scylla() && def.class == IndexClass::Local {
        params.extensions.insert(
            "scylla_tags".to_string(),
            crate::schema::params::scylla_map_blob(&BTreeMap::from([(
                "system:synchronous_view_updates".to_string(),
                "true".to_string(),
            )])),
        );
        if tablets {
            params
                .tombstone_gc
                .insert("mode".to_string(), "timeout".to_string());
        }
    }
    let mut t = Table::new(
        base.keyspace.clone(),
        name,
        id,
        crate::uuid::timeuuid(),
        cols,
        params,
        TableFlags {
            compound: true,
            ..Default::default()
        },
        TableKind::Regular,
    );
    t.view = Some(view_meta(base, def, &computed));
    t
}

/// The storage id of a hidden (Cassandra) index table: stable for the base table and index.
pub fn hidden_id(base: &Table, index: &str) -> [u8; 16] {
    let mut name = base.id.to_vec();
    name.extend_from_slice(b"/index/");
    name.extend_from_slice(index.as_bytes());
    crate::uuid::v5(&crate::uuid::NAMESPACE_ROLL, &name)
}

/// Cassandra keeps index tables out of the schema: the hidden index table of `def`, if it has
/// one.
pub fn hidden_table(base: &Table, def: &IndexDef, d: Dialect) -> Option<Table> {
    if d.is_scylla() || !def.has_table(base) {
        return None;
    }
    let id = hidden_id(base, &def.name);
    let mut t = index_table(
        base,
        def,
        &format!("{}.{}", base.name, def.name),
        id,
        d,
        false,
    );
    t.version = id;
    t.generation = base.index_generations.get(&def.name).copied().unwrap_or(0);
    Some(t)
}

/// The storage table of `base`'s hidden index `index`.
pub fn hidden_storage_id(base: &Table, index: &str) -> TableId {
    crate::schema::storage_id(
        hidden_id(base, index),
        base.index_generations.get(index).copied().unwrap_or(0),
    )
}

/// The storage tables of `base`'s hidden indexes (dropped and truncated with it).
pub fn hidden_tables(base: &Table) -> Vec<TableId> {
    base.indexes
        .iter()
        .map(|m| hidden_storage_id(base, &m.name))
        .collect()
}

/// Truncates `base`'s hidden index tables (attaching them first).
pub async fn truncate_hidden(db: &Arc<Database>, base: &Table) -> Result<()> {
    let d = db.dialect();
    for def in defs(base) {
        if let Some(t) = hidden_table(base, &def, d) {
            let st = crate::storage::attach(&db.store, &t, d, db.memtable_flush_bytes()).await?;
            st.truncate().await.map_err(Error::internal)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("v"),
            Some(("v".into(), TargetKind::Simple, false))
        );
        assert_eq!(
            parse_target("keys(m)"),
            Some(("m".into(), TargetKind::Keys, false))
        );
        assert_eq!(
            parse_target("values(\"Ab\")"),
            Some(("Ab".into(), TargetKind::Values, false))
        );
        assert_eq!(
            parse_target(r#"{"pk":["pk"],"ck":["v"]}"#),
            Some(("v".into(), TargetKind::Simple, true))
        );
        assert_eq!(
            parse_target("full(fl)"),
            Some(("fl".into(), TargetKind::Full, false))
        );
    }

    #[test]
    fn computations_round_trip() {
        for c in [
            Computed::Token,
            Computed::Elements(TargetKind::Values),
            Computed::Elements(TargetKind::Keys),
            Computed::Elements(TargetKind::Entries),
        ] {
            assert_eq!(Computed::parse(&c.json("l")), Some(c));
        }
        assert_eq!(
            Computed::Path.json("m"),
            r#"{"type":"collection_keys","collection_name":"m"}"#
        );
    }
}
