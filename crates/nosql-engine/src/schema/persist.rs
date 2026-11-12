//! Schema as data: keyspace definitions to and from `system_schema` rows.
//!
//! The row format is exactly what each reference stores (Cassandra's `SchemaKeyspace`,
//! Apache-2.0; ScyllaDB's from the image), because drivers build their metadata by reading these
//! tables. A schema change rewrites the changed keyspace: one partition per `system_schema` table,
//! deleting the previous partition at `ts - 1` and writing every row at `ts`. That keeps a
//! keyspace's rows consistent in one atomic batch, and branches / restores carry the schema with
//! the data.
//!
//! Storage metadata a client must not see (column ids, the table version) lives in a hidden cell
//! of the `system_schema.tables` / `views` row ([`HIDDEN_META`]); hidden cells are never returned
//! because they are not columns of the CQL definition.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use nosql_storage::manifest::TableId;
use nosql_storage::read::Partition;
use nosql_storage::{
    Cell, Clustering, ColumnData, DeletionTime, LivenessInfo, PartitionKey, PartitionUpdate, Row,
};
use nosql_types::{CqlType, Dialect, UdtDef};

use super::{
    AggregateMeta, Column, ColumnKind, DroppedColumn, FunctionMeta, IndexMeta, Keyspace,
    KeyspaceKind, Replication, Table, TableFlags, TableKind, TableParams, TriggerMeta, ViewMeta,
};
use crate::error::{Error, Result};
use crate::system::schema_tables::{HIDDEN_DROPPED_KIND, HIDDEN_META};

/// The `system_schema` tables of a dialect, by name.
pub struct SchemaTables {
    pub dialect: Dialect,
    pub tables: BTreeMap<String, Arc<Table>>,
}

impl SchemaTables {
    pub fn new(dialect: Dialect, system_schema: &Keyspace) -> Self {
        SchemaTables {
            dialect,
            tables: system_schema.tables.clone(),
        }
    }

    pub fn get(&self, name: &str) -> &Arc<Table> {
        &self.tables[name]
    }

    pub fn ids(&self) -> Vec<TableId> {
        self.tables.values().map(|t| t.table_id()).collect()
    }
}

/// Builds one row of a `system_schema` table.
struct RowBuilder<'t> {
    table: &'t Table,
    row: Row,
    ts: i64,
}

impl<'t> RowBuilder<'t> {
    fn new(table: &'t Table, clustering: &[&str], ts: i64) -> Self {
        let row = Row::new(Clustering::new(clustering.iter().map(|s| s.as_bytes())))
            .with_liveness(LivenessInfo::live(ts));
        RowBuilder { table, row, ts }
    }

    fn set(mut self, column: &str, value: Option<Bytes>) -> Self {
        if let (Some(v), Some(c)) = (value, self.table.column(column)) {
            self.row
                .columns
                .push(ColumnData::simple(c.id, Cell::live(self.ts, v)));
        }
        self
    }

    fn hidden(mut self, id: u32, value: Bytes) -> Self {
        self.row
            .columns
            .push(ColumnData::simple(id, Cell::live(self.ts, value)));
        self
    }

    fn build(mut self) -> Row {
        self.row.columns.sort_by_key(|c| c.column());
        self.row
    }
}

fn text(s: &str) -> Option<Bytes> {
    Some(Bytes::copy_from_slice(s.as_bytes()))
}
fn int(v: i32) -> Option<Bytes> {
    Some(Bytes::copy_from_slice(&v.to_be_bytes()))
}
fn double(v: f64) -> Option<Bytes> {
    Some(Bytes::copy_from_slice(&v.to_be_bytes()))
}
fn boolean(v: bool) -> Option<Bytes> {
    Some(Bytes::copy_from_slice(&[v as u8]))
}
fn uuid(u: &[u8; 16]) -> Option<Bytes> {
    Some(Bytes::copy_from_slice(u))
}
fn bigint(v: i64) -> Option<Bytes> {
    Some(Bytes::copy_from_slice(&v.to_be_bytes()))
}
fn text_map(m: &BTreeMap<String, String>) -> Option<Bytes> {
    Some(nosql_types::pack_map(
        m.iter().map(|(k, v)| (k.as_bytes(), v.as_bytes())),
    ))
}
/// ScyllaDB serialises an index's options from its `std::unordered_map` of them, filled with
/// the statement's options (in name order) and then `class_name`, `index_version` and
/// `target`: they read back in that map's iteration order.
fn scylla_index_options(m: &BTreeMap<String, String>) -> Option<Bytes> {
    const LAST: [&str; 3] = ["class_name", "index_version", "target"];
    let keys: Vec<&str> = m
        .keys()
        .map(String::as_str)
        .filter(|k| !LAST.contains(k))
        .chain(LAST.into_iter().filter(|k| m.contains_key(*k)))
        .collect();
    let order = crate::cpp::unordered_map_order(&keys);
    Some(nosql_types::pack_map(
        order.iter().map(|k| (k.as_bytes(), m[*k].as_bytes())),
    ))
}
fn text_list(v: &[String]) -> Option<Bytes> {
    Some(nosql_types::pack_collection(v.iter().map(|s| s.as_bytes())))
}

pub fn partition_key(keyspace: &str) -> PartitionKey {
    PartitionKey::new(
        nosql_types::token(keyspace.as_bytes()),
        Bytes::copy_from_slice(keyspace.as_bytes()),
    )
}

/// The type text stored in `system_schema` (`frozen<map<text, int>>`, `address`, …).
pub fn type_text(ty: &CqlType, d: Dialect) -> String {
    nosql_types::cql_name_in(ty, d)
}

/// The rows describing `ks`, per `system_schema` table name (no deletions).
pub fn keyspace_rows(ks: &Keyspace, st: &SchemaTables, ts: i64) -> BTreeMap<String, Vec<Row>> {
    let d = st.dialect;
    let mut out: BTreeMap<String, Vec<Row>> =
        st.tables.keys().map(|k| (k.clone(), Vec::new())).collect();
    let mut push = |table: &str, row: Row| {
        if let Some(v) = out.get_mut(table) {
            v.push(row);
        }
    };
    let t = st.get("keyspaces");
    push(
        "keyspaces",
        RowBuilder::new(t, &[], ts)
            .set("durable_writes", boolean(ks.durable_writes))
            .set("replication", text_map(&ks.replication.as_map()))
            .set(
                "replication_v2",
                ks.replication.v2_map().as_ref().and_then(text_map),
            )
            .build(),
    );
    if d.is_scylla()
        && let Some(n) = ks.tablets
    {
        push(
            "scylla_keyspaces",
            RowBuilder::new(st.get("scylla_keyspaces"), &[], ts)
                .set("initial_tablets", int(n))
                .build(),
        );
    }
    for (name, u) in &ks.types {
        let names: Vec<String> = u.fields.iter().map(|(n, _)| n.clone()).collect();
        let types: Vec<String> = u.fields.iter().map(|(_, t)| type_text(t, d)).collect();
        push(
            "types",
            RowBuilder::new(st.get("types"), &[name], ts)
                .set("field_names", text_list(&names))
                .set("field_types", text_list(&types))
                .build(),
        );
    }
    if let Some(mt) = st.tables.get("column_masks") {
        for ((table, column), m) in &ks.stale_masks {
            push("column_masks", mask_row(mt, table, column, m, ts));
        }
    }
    for table in ks.tables.values() {
        table_rows(table, st, ts, &mut push);
        // ScyllaDB's virtual columns of a view (unselected base columns that keep its rows alive).
        if d.is_scylla()
            && let Some(v) = &table.view
            && let Some(base) = ks.tables.values().find(|b| b.id == v.base_table_id)
        {
            for (cname, ty) in crate::views::virtual_columns(base, table) {
                push(
                    "view_virtual_columns",
                    RowBuilder::new(st.get("view_virtual_columns"), &[&table.name, &cname], ts)
                        .set("clustering_order", text("NONE"))
                        .set(
                            "column_name_bytes",
                            Some(Bytes::copy_from_slice(cname.as_bytes())),
                        )
                        .set("kind", text("regular"))
                        .set("position", int(-1))
                        .set("type", text(&ty))
                        .build(),
                );
            }
        }
    }
    for f in &ks.functions {
        let types = text_list(&f.argument_types).unwrap_or_default();
        let types_s = String::from_utf8_lossy(&types).to_string();
        let _ = types_s;
        let row = RowBuilder::new(st.get("functions"), &[], ts);
        // Clustering: (function_name, argument_types) — a frozen list is a clustering component.
        let mut row = row;
        row.row.clustering = Clustering::new([Bytes::copy_from_slice(f.name.as_bytes()), types]);
        push(
            "functions",
            row.set("argument_names", text_list(&f.argument_names))
                .set("body", text(&f.body))
                .set("called_on_null_input", boolean(f.called_on_null_input))
                .set("language", text(&f.language))
                .set("return_type", text(&f.return_type))
                .build(),
        );
    }
    for a in &ks.aggregates {
        let types = text_list(&a.argument_types).unwrap_or_default();
        let mut row = RowBuilder::new(st.get("aggregates"), &[], ts);
        row.row.clustering =
            Clustering::new([Bytes::copy_from_slice(a.name.as_bytes()), types.clone()]);
        push(
            "aggregates",
            row.set("final_func", a.final_func.as_deref().and_then(text))
                .set("initcond", a.initcond.as_deref().and_then(text))
                .set("return_type", text(&a.return_type))
                .set("state_func", text(&a.state_func))
                .set("state_type", text(&a.state_type))
                .build(),
        );
        if d.is_scylla()
            && let Some(r) = &a.reduce_func
        {
            let mut row = RowBuilder::new(st.get("scylla_aggregates"), &[], ts);
            row.row.clustering =
                Clustering::new([Bytes::copy_from_slice(a.name.as_bytes()), types]);
            push(
                "scylla_aggregates",
                row.set("reduce_func", text(r))
                    .set("state_type", text(&a.state_type))
                    .build(),
            );
        }
    }
    out
}

/// The hidden metadata blob: version, next column id, then `(name, id)` pairs.
fn meta_blob(t: &Table) -> Bytes {
    let mut b = BytesMut::new();
    b.extend_from_slice(&[1]);
    b.extend_from_slice(&t.version);
    b.extend_from_slice(&t.next_column_id.to_be_bytes());
    b.extend_from_slice(&(t.column_ids.len() as u32).to_be_bytes());
    for (n, id) in &t.column_ids {
        b.extend_from_slice(&(n.len() as u16).to_be_bytes());
        b.extend_from_slice(n.as_bytes());
        b.extend_from_slice(&id.to_be_bytes());
    }
    b.freeze()
}

struct Meta {
    version: [u8; 16],
    next_column_id: u32,
    ids: BTreeMap<String, u32>,
}

fn parse_meta(b: &[u8]) -> Option<Meta> {
    let mut r = b;
    let mut take = |n: usize| -> Option<&[u8]> {
        if r.len() < n {
            return None;
        }
        let (h, t) = r.split_at(n);
        r = t;
        Some(h)
    };
    if take(1)? != [1] {
        return None;
    }
    let version: [u8; 16] = take(16)?.try_into().ok()?;
    let next_column_id = u32::from_be_bytes(take(4)?.try_into().ok()?);
    let n = u32::from_be_bytes(take(4)?.try_into().ok()?);
    let mut ids = BTreeMap::new();
    for _ in 0..n {
        let l = u16::from_be_bytes(take(2)?.try_into().ok()?) as usize;
        let name = String::from_utf8(take(l)?.to_vec()).ok()?;
        let id = u32::from_be_bytes(take(4)?.try_into().ok()?);
        ids.insert(name, id);
    }
    Some(Meta {
        version,
        next_column_id,
        ids,
    })
}

fn params_cells<'t>(mut r: RowBuilder<'t>, p: &TableParams, d: Dialect) -> RowBuilder<'t> {
    r = r
        .set("bloom_filter_fp_chance", double(p.bloom_filter_fp_chance))
        .set("caching", text_map(&p.caching))
        .set("comment", text(&p.comment))
        .set("compaction", text_map(&p.compaction))
        .set("compression", text_map(&p.compression))
        .set("crc_check_chance", double(p.crc_check_chance))
        .set("default_time_to_live", int(p.default_time_to_live))
        .set(
            "extensions",
            Some(nosql_types::pack_map(
                p.extensions_map(d)
                    .iter()
                    .map(|(k, v)| (k.as_bytes(), &v[..])),
            )),
        )
        .set("gc_grace_seconds", int(p.gc_grace_seconds))
        .set("max_index_interval", int(p.max_index_interval))
        .set(
            "memtable_flush_period_in_ms",
            int(p.memtable_flush_period_in_ms),
        )
        .set("min_index_interval", int(p.min_index_interval))
        .set("speculative_retry", text(&p.speculative_retry));
    if !d.is_scylla() {
        r = r
            .set("dclocal_read_repair_chance", double(0.0))
            .set("read_repair_chance", double(0.0))
            .set("additional_write_policy", text(&p.additional_write_policy))
            .set("read_repair", text(&p.read_repair))
            .set("memtable", p.memtable.as_deref().and_then(text))
            .set(
                "allow_auto_snapshot",
                p.allow_auto_snapshot.and_then(boolean),
            )
            .set(
                "incremental_backups",
                p.incremental_backups.and_then(boolean),
            )
            .set("cdc", p.cdc.and_then(boolean));
    }
    r
}

/// The COMPACT STORAGE layouts whose CQL metadata has columns our model doesn't.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CompactShape {
    None,
    /// No clustering: defined columns are static, plus a hidden clustering and value.
    Static,
    /// Clustering but no value column: a hidden `empty` value.
    DenseNoValue,
}

impl CompactShape {
    fn of(t: &Table) -> CompactShape {
        if t.view.is_some() || t.flags.compound || t.flags.super_ {
            return CompactShape::None;
        }
        if !t.flags.dense {
            return CompactShape::Static;
        }
        if t.columns.iter().all(|c| c.is_primary_key()) {
            CompactShape::DenseNoValue
        } else {
            CompactShape::None
        }
    }

    fn from_flags(flags: &TableFlags) -> CompactShape {
        match (flags.compound, flags.super_, flags.dense) {
            (false, false, false) => CompactShape::Static,
            _ => CompactShape::None,
        }
    }
}

/// `CreateTableStatement.DefaultNames`.
/// The hidden clustering column of a static compact table (`column1`), which the model omits
/// (`DESCRIBE` prints it in `CLUSTERING ORDER BY`).
pub(crate) fn static_compact_clustering(t: &Table) -> Option<String> {
    (CompactShape::of(t) == CompactShape::Static).then(|| DefaultNames::new(t).clustering())
}

/// The hidden `empty` value column of a dense compact table without one (`value`), which the
/// model omits (ScyllaDB's `DESCRIBE` prints it).
pub(crate) fn dense_empty_value(t: &Table) -> Option<String> {
    (CompactShape::of(t) == CompactShape::DenseNoValue).then(|| DefaultNames::new(t).value())
}

struct DefaultNames {
    used: Vec<String>,
    clustering: usize,
    value: usize,
}

impl DefaultNames {
    fn new(t: &Table) -> Self {
        DefaultNames {
            used: t.columns.iter().map(|c| c.name.clone()).collect(),
            clustering: 1,
            value: 0,
        }
    }

    fn clustering(&mut self) -> String {
        loop {
            let c = format!("column{}", self.clustering);
            self.clustering += 1;
            if !self.used.contains(&c) {
                self.used.push(c.clone());
                return c;
            }
        }
    }

    fn value(&mut self) -> String {
        loop {
            let c = if self.value == 0 {
                "value".to_string()
            } else {
                format!("value{}", self.value)
            };
            self.value += 1;
            if !self.used.contains(&c) {
                self.used.push(c.clone());
                return c;
            }
        }
    }
}

/// A `system_schema.columns` row: name, type text, kind, position, reversed.
type StoredColumn = (String, String, ColumnKind, i32, bool);

fn table_rows(t: &Table, st: &SchemaTables, ts: i64, push: &mut impl FnMut(&str, Row)) {
    let d = st.dialect;
    let name = t.name.as_str();
    match &t.view {
        None => {
            let flags: Vec<&str> = t.flags.names();
            let r = RowBuilder::new(st.get("tables"), &[name], ts)
                .set("id", uuid(&t.id))
                .set(
                    "flags",
                    Some(nosql_types::pack_collection(
                        flags.iter().map(|s| s.as_bytes()),
                    )),
                )
                .hidden(HIDDEN_META, meta_blob(t));
            push("tables", params_cells(r, &t.params, d).build());
        }
        Some(v) => {
            if d.is_scylla() {
                for (cname, computation) in &v.computed {
                    push(
                        "computed_columns",
                        RowBuilder::new(st.get("computed_columns"), &[name, cname], ts)
                            .set(
                                "computation",
                                Some(Bytes::copy_from_slice(computation.as_bytes())),
                            )
                            .build(),
                    );
                }
            }
            let r = RowBuilder::new(st.get("views"), &[name], ts)
                .set("id", uuid(&t.id))
                .set("base_table_id", uuid(&v.base_table_id))
                .set("base_table_name", text(&v.base_table_name))
                .set("include_all_columns", boolean(v.include_all_columns))
                .set("where_clause", text(&v.where_clause))
                .hidden(HIDDEN_META, meta_blob(t));
            push("views", params_cells(r, &t.params, d).build());
        }
    }
    let column_row = |cname: &str, order: &str, kind: &str, position: i32, ty: &str| {
        RowBuilder::new(st.get("columns"), &[name, cname], ts)
            .set("clustering_order", text(order))
            .set(
                "column_name_bytes",
                Some(Bytes::copy_from_slice(cname.as_bytes())),
            )
            .set("kind", text(kind))
            .set("position", int(position))
            .set("type", text(ty))
            .build()
    };
    let compact = CompactShape::of(t);
    for c in &t.columns {
        // A static compact table's defined columns are static in its CQL metadata.
        let kind = if compact == CompactShape::Static && c.kind == ColumnKind::Regular {
            "static"
        } else {
            c.kind.name()
        };
        push(
            "columns",
            column_row(
                &c.name,
                c.clustering_order(d),
                kind,
                c.position,
                &type_text(&c.ty, d),
            ),
        );
    }
    // `CreateTableStatement.fixupCompactTable`: the hidden clustering and value columns.
    let mut names = DefaultNames::new(t);
    match compact {
        CompactShape::Static => {
            push(
                "columns",
                column_row(&names.clustering(), "asc", "clustering", 0, "text"),
            );
            push(
                "columns",
                column_row(
                    &names.value(),
                    "none",
                    "regular",
                    -1,
                    if t.flags.counter { "counter" } else { "blob" },
                ),
            );
        }
        CompactShape::DenseNoValue => push(
            "columns",
            column_row(&names.value(), "none", "regular", -1, "empty"),
        ),
        CompactShape::None => {}
    }
    for (cname, dc) in &t.dropped {
        let time = if d.is_scylla() {
            dc.dropped_at
        } else {
            dc.dropped_at.div_euclid(1000)
        };
        let mut r = RowBuilder::new(st.get("dropped_columns"), &[name, cname], ts)
            .set("dropped_time", bigint(time))
            .set("type", text(&type_text(&dc.ty, d)))
            .hidden(
                HIDDEN_DROPPED_KIND,
                Bytes::copy_from_slice(dc.kind.name().as_bytes()),
            );
        if !d.is_scylla() {
            r = r.set("kind", text(dc.kind.name()));
        }
        push("dropped_columns", r.build());
    }
    for i in &t.indexes {
        push(
            "indexes",
            RowBuilder::new(st.get("indexes"), &[name, &i.name], ts)
                .set("kind", text(&i.kind))
                .set(
                    "options",
                    if d.is_scylla() {
                        scylla_index_options(&i.options)
                    } else {
                        text_map(&i.options)
                    },
                )
                .build(),
        );
    }
    // Cassandra 5's `column_masks` (absent in ScyllaDB).
    if let Some(mt) = st.tables.get("column_masks") {
        for (cname, m) in &t.masks {
            push("column_masks", mask_row(mt, name, cname, m, ts));
        }
    }
    for tr in &t.triggers {
        push(
            "triggers",
            RowBuilder::new(st.get("triggers"), &[name, &tr.name], ts)
                .set("options", text_map(&tr.options))
                .build(),
        );
    }
    if d.is_scylla() {
        // A CDC log uses the CDC partitioner.
        let partitioner = (t.name.ends_with(crate::cdc::SUFFIX)
            && t.columns.first().is_some_and(|c| c.name == "cdc$stream_id"))
        .then(|| text(crate::cdc::PARTITIONER))
        .flatten();
        push(
            "scylla_tables",
            RowBuilder::new(st.get("scylla_tables"), &[name], ts)
                .set("committed_by_group0", boolean(true))
                .set(
                    "large_data_guardrails_enabled",
                    boolean(true).filter(|_| t.params.large_data_guardrails),
                )
                .set("partitioner", partitioner)
                .set(
                    "tablets",
                    Some(&t.params.tablets)
                        .filter(|m| !m.is_empty())
                        .and_then(text_map),
                )
                .set("version", uuid(&t.version))
                .build(),
        );
    }
}

/// A `column_masks` row.
fn mask_row(mt: &Table, table: &str, column: &str, m: &super::ColumnMask, ts: i64) -> Row {
    let nulls: Vec<Bytes> = m
        .arg_values
        .iter()
        .map(|v| Bytes::copy_from_slice(&[v.is_none() as u8]))
        .collect();
    let values: Vec<String> = m
        .arg_values
        .iter()
        .map(|v| v.clone().unwrap_or_default())
        .collect();
    RowBuilder::new(mt, &[table, column], ts)
        .set(
            "function_argument_nulls",
            Some(nosql_types::pack_collection(nulls.iter().map(|b| &b[..]))),
        )
        .set("function_argument_types", text_list(&m.arg_types))
        .set("function_argument_values", text_list(&values))
        .set("function_keyspace", text(&m.function_keyspace))
        .set("function_name", text(&m.function_name))
        .build()
}

/// The batch that replaces `ks`'s schema at `ts` (`now_s`: local deletion time).
pub fn keyspace_mutations(
    ks: &Keyspace,
    st: &SchemaTables,
    ts: i64,
    now_s: i64,
) -> Vec<(TableId, PartitionUpdate)> {
    let rows = keyspace_rows(ks, st, ts);
    let key = partition_key(&ks.name);
    st.tables
        .iter()
        .map(|(name, t)| {
            let mut u =
                PartitionUpdate::new(key.clone()).with_deletion(DeletionTime::new(ts - 1, now_s));
            u.rows = rows.get(name).cloned().unwrap_or_default();
            (t.table_id(), u)
        })
        .collect()
}

/// The batch that removes `keyspace`'s schema.
pub fn drop_keyspace_mutations(
    keyspace: &str,
    st: &SchemaTables,
    ts: i64,
    now_s: i64,
) -> Vec<(TableId, PartitionUpdate)> {
    let key = partition_key(keyspace);
    st.tables
        .values()
        .map(|t| {
            (
                t.table_id(),
                PartitionUpdate::new(key.clone()).with_deletion(DeletionTime::new(ts, now_s)),
            )
        })
        .collect()
}

/// Generated partitions for built-in keyspaces' schema rows (what `SELECT * FROM system_schema.x`
/// shows for `system`, `system_schema`, …), per table name, in token order.
pub fn builtin_partitions(
    kss: &[&Keyspace],
    st: &SchemaTables,
) -> BTreeMap<String, Vec<Partition>> {
    let mut out: BTreeMap<String, Vec<Partition>> = BTreeMap::new();
    for ks in kss {
        let rows = keyspace_rows(ks, st, 0);
        for (table, mut rows) in rows {
            if rows.is_empty() {
                continue;
            }
            let cmp = st.get(&table).comparator.clone();
            rows.sort_by(|a, b| {
                nosql_storage::compare::compare_clustering(&*cmp, &a.clustering, &b.clustering)
            });
            out.entry(table).or_default().push(Partition {
                key: partition_key(&ks.name),
                partition_deletion: DeletionTime::LIVE,
                static_row: None,
                rows,
                markers: Vec::new(),
            });
        }
    }
    for v in out.values_mut() {
        v.sort_by(|a, b| a.key.cmp(&b.key));
    }
    out
}

/// Typed access to a stored row's cells.
struct Cells<'a> {
    table: &'a Table,
    row: &'a Row,
}

impl<'a> Cells<'a> {
    fn raw(&self, name: &str) -> Option<&'a Bytes> {
        let c = self.table.column(name)?;
        self.row
            .cell(c.id)
            .filter(|c| !c.is_tombstone())
            .map(|c| &c.value)
    }

    fn text(&self, name: &str) -> Option<String> {
        self.raw(name)
            .map(|b| String::from_utf8_lossy(b).into_owned())
    }

    fn int(&self, name: &str) -> Option<i32> {
        self.raw(name)
            .and_then(|b| <[u8; 4]>::try_from(&b[..]).ok())
            .map(i32::from_be_bytes)
    }

    fn bigint(&self, name: &str) -> Option<i64> {
        self.raw(name)
            .and_then(|b| <[u8; 8]>::try_from(&b[..]).ok())
            .map(i64::from_be_bytes)
    }

    fn double(&self, name: &str) -> Option<f64> {
        self.raw(name)
            .and_then(|b| <[u8; 8]>::try_from(&b[..]).ok())
            .map(f64::from_be_bytes)
    }

    fn boolean(&self, name: &str) -> Option<bool> {
        self.raw(name).and_then(|b| b.first()).map(|b| *b != 0)
    }

    fn uuid(&self, name: &str) -> Option<[u8; 16]> {
        self.raw(name)
            .and_then(|b| <[u8; 16]>::try_from(&b[..]).ok())
    }

    fn text_map(&self, name: &str) -> BTreeMap<String, String> {
        self.raw(name)
            .and_then(|b| nosql_types::map_entries(b).ok())
            .map(|es| {
                es.into_iter()
                    .map(|(k, v)| {
                        (
                            String::from_utf8_lossy(k).into_owned(),
                            String::from_utf8_lossy(v).into_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn blob_map(&self, name: &str) -> BTreeMap<String, Bytes> {
        self.raw(name)
            .and_then(|b| nosql_types::map_entries(b).ok())
            .map(|es| {
                es.into_iter()
                    .map(|(k, v)| {
                        (
                            String::from_utf8_lossy(k).into_owned(),
                            Bytes::copy_from_slice(v),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn text_list(&self, name: &str) -> Vec<String> {
        self.raw(name)
            .and_then(|b| nosql_types::collection_elements(b).ok())
            .map(|es| {
                es.into_iter()
                    .map(|e| String::from_utf8_lossy(e.unwrap_or_default()).into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn hidden(&self, id: u32) -> Option<&'a Bytes> {
        self.row.cell(id).map(|c| &c.value)
    }
}

fn clustering_text(row: &Row, i: usize) -> String {
    row.clustering
        .get(i)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

fn corrupt(what: impl std::fmt::Display) -> Error {
    Error::server(format!("corrupt schema: {what}"))
}

/// Rebuilds user keyspaces from the stored `system_schema` partitions (per table name).
pub fn load(st: &SchemaTables, parts: &BTreeMap<String, Vec<Partition>>) -> Result<Vec<Keyspace>> {
    let d = st.dialect;
    let empty = Vec::new();
    let get = |n: &str| parts.get(n).unwrap_or(&empty);
    let mut kss: BTreeMap<String, Keyspace> = BTreeMap::new();
    let kt = st.get("keyspaces");
    for p in get("keyspaces") {
        let name = String::from_utf8_lossy(&p.key.key).into_owned();
        for row in &p.rows {
            let c = Cells { table: kt, row };
            let mut ks = Keyspace::new(
                name.clone(),
                Replication::from_map(c.text_map("replication"))
                    .with_v2(&c.text_map("replication_v2")),
                KeyspaceKind::User,
            );
            ks.durable_writes = c.boolean("durable_writes").unwrap_or(true);
            kss.insert(name.clone(), ks);
        }
    }
    if d.is_scylla() {
        let t = st.get("scylla_keyspaces");
        for p in get("scylla_keyspaces") {
            let name = String::from_utf8_lossy(&p.key.key).into_owned();
            for row in &p.rows {
                if let Some(ks) = kss.get_mut(&name) {
                    ks.tablets = Cells { table: t, row }.int("initial_tablets").or(Some(0));
                }
            }
        }
    }
    // User types, resolving references between them in dependency order.
    let tt = st.get("types");
    for p in get("types") {
        let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
        let Some(ks) = kss.get_mut(&ksname) else {
            continue;
        };
        let mut pending: Vec<(String, Vec<String>, Vec<String>)> = p
            .rows
            .iter()
            .map(|row| {
                let c = Cells { table: tt, row };
                (
                    clustering_text(row, 0),
                    c.text_list("field_names"),
                    c.text_list("field_types"),
                )
            })
            .collect();
        while !pending.is_empty() {
            let before = pending.len();
            let mut rest = Vec::new();
            for (name, fnames, ftypes) in pending {
                let mut fields = Vec::new();
                let mut ok = true;
                for (fname, ftype) in fnames.iter().zip(&ftypes) {
                    match nosql_types::parse_cql_type(ftype, &ksname, &|_, n| {
                        ks.types.get(n).cloned()
                    }) {
                        Ok(t) => fields.push((fname.clone(), t)),
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    ks.types.insert(
                        name.clone(),
                        Arc::new(UdtDef::new(ksname.clone(), name, fields)),
                    );
                } else {
                    rest.push((name, fnames, ftypes));
                }
            }
            if rest.len() == before {
                return Err(corrupt(format!("unresolvable user types in {ksname}")));
            }
            pending = rest;
        }
    }
    // Columns per (keyspace, table).
    let ct = st.get("columns");
    let mut columns: HashMap<(String, String), Vec<StoredColumn>> = HashMap::new();
    for p in get("columns") {
        let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
        for row in &p.rows {
            let c = Cells { table: ct, row };
            let kind = c
                .text("kind")
                .and_then(|k| ColumnKind::parse(&k))
                .ok_or_else(|| corrupt("column kind"))?;
            let reversed = c
                .text("clustering_order")
                .is_some_and(|o| o.eq_ignore_ascii_case("desc"));
            columns
                .entry((ksname.clone(), clustering_text(row, 0)))
                .or_default()
                .push((
                    clustering_text(row, 1),
                    c.text("type").unwrap_or_default(),
                    kind,
                    c.int("position").unwrap_or(-1),
                    reversed,
                ));
        }
    }
    let dt = st.get("dropped_columns");
    let mut dropped: HashMap<(String, String), BTreeMap<String, DroppedColumn>> = HashMap::new();
    for p in get("dropped_columns") {
        let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
        for row in &p.rows {
            let c = Cells { table: dt, row };
            let raw_time = c.bigint("dropped_time").unwrap_or(0);
            let dropped_at = if d.is_scylla() {
                raw_time
            } else {
                raw_time.saturating_mul(1000)
            };
            let kind = c
                .hidden(HIDDEN_DROPPED_KIND)
                .and_then(|b| ColumnKind::parse(&String::from_utf8_lossy(b)))
                .unwrap_or(ColumnKind::Regular);
            let ty = nosql_types::parse_cql_type(
                &c.text("type").unwrap_or_default(),
                &ksname,
                &|_, n| kss.get(&ksname).and_then(|k| k.types.get(n).cloned()),
            )
            .unwrap_or(CqlType::Blob);
            dropped
                .entry((ksname.clone(), clustering_text(row, 0)))
                .or_default()
                .insert(
                    clustering_text(row, 1),
                    DroppedColumn {
                        dropped_at,
                        ty,
                        kind,
                    },
                );
        }
    }
    let mut indexes: HashMap<(String, String), Vec<IndexMeta>> = HashMap::new();
    let it = st.get("indexes");
    for p in get("indexes") {
        let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
        for row in &p.rows {
            let c = Cells { table: it, row };
            indexes
                .entry((ksname.clone(), clustering_text(row, 0)))
                .or_default()
                .push(IndexMeta {
                    name: clustering_text(row, 1),
                    kind: c.text("kind").unwrap_or_default(),
                    options: c.text_map("options"),
                });
        }
    }
    let mut computed: HashMap<(String, String), Vec<(String, String)>> = HashMap::new();
    if let Some(cct) = st.tables.get("computed_columns") {
        for p in get("computed_columns") {
            let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
            for row in &p.rows {
                let c = Cells { table: cct, row };
                computed
                    .entry((ksname.clone(), clustering_text(row, 0)))
                    .or_default()
                    .push((
                        clustering_text(row, 1),
                        c.raw("computation")
                            .map(|b| String::from_utf8_lossy(b).into_owned())
                            .unwrap_or_default(),
                    ));
            }
        }
    }
    let mut triggers: HashMap<(String, String), Vec<TriggerMeta>> = HashMap::new();
    let trt = st.get("triggers");
    for p in get("triggers") {
        let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
        for row in &p.rows {
            let c = Cells { table: trt, row };
            triggers
                .entry((ksname.clone(), clustering_text(row, 0)))
                .or_default()
                .push(TriggerMeta {
                    name: clustering_text(row, 1),
                    options: c.text_map("options"),
                });
        }
    }
    // Column masks by (keyspace, table).
    let mut masks: HashMap<(String, String), BTreeMap<String, super::ColumnMask>> = HashMap::new();
    if let Some(mt) = st.tables.get("column_masks") {
        for p in get("column_masks") {
            let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
            for row in &p.rows {
                let c = Cells { table: mt, row };
                let nulls: Vec<bool> = c
                    .raw("function_argument_nulls")
                    .and_then(|b| nosql_types::collection_elements(b).ok())
                    .map(|v| {
                        v.into_iter()
                            .map(|e| e.and_then(|b| b.first()).is_some_and(|b| *b != 0))
                            .collect()
                    })
                    .unwrap_or_default();
                let values = c.text_list("function_argument_values");
                masks
                    .entry((ksname.clone(), clustering_text(row, 0)))
                    .or_default()
                    .insert(
                        clustering_text(row, 1),
                        super::ColumnMask {
                            function_keyspace: c.text("function_keyspace").unwrap_or_default(),
                            function_name: c.text("function_name").unwrap_or_default(),
                            arg_types: c.text_list("function_argument_types"),
                            arg_values: values
                                .into_iter()
                                .enumerate()
                                .map(|(i, v)| (!nulls.get(i).copied().unwrap_or(false)).then_some(v))
                                .collect(),
                        },
                    );
            }
        }
    }
    for (src, is_view) in [("tables", false), ("views", true)] {
        let tt = st.get(src);
        for p in get(src) {
            let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
            let Some(ks) = kss.get_mut(&ksname) else {
                continue;
            };
            for row in &p.rows {
                let c = Cells { table: tt, row };
                let name = clustering_text(row, 0);
                let id = c
                    .uuid("id")
                    .ok_or_else(|| corrupt(format!("table id of {ksname}.{name}")))?;
                let meta = c.hidden(HIDDEN_META).and_then(|b| parse_meta(b));
                let mut params = TableParams::defaults(d, false);
                params.bloom_filter_fp_chance = c
                    .double("bloom_filter_fp_chance")
                    .unwrap_or(params.bloom_filter_fp_chance);
                params.caching = c.text_map("caching");
                params.comment = c.text("comment").unwrap_or_default();
                params.compaction = c.text_map("compaction");
                params.compression = c.text_map("compression");
                params.crc_check_chance = c.double("crc_check_chance").unwrap_or(1.0);
                params.default_time_to_live = c.int("default_time_to_live").unwrap_or(0);
                params.extensions = c.blob_map("extensions");
                params.tombstone_gc.clear();
                params.absorb_extensions(d);
                params.gc_grace_seconds = c.int("gc_grace_seconds").unwrap_or(864_000);
                params.max_index_interval = c.int("max_index_interval").unwrap_or(2048);
                params.min_index_interval = c.int("min_index_interval").unwrap_or(128);
                params.memtable_flush_period_in_ms =
                    c.int("memtable_flush_period_in_ms").unwrap_or(0);
                params.speculative_retry = c.text("speculative_retry").unwrap_or_default();
                if !d.is_scylla() {
                    params.additional_write_policy = c
                        .text("additional_write_policy")
                        .unwrap_or_else(|| "99p".into());
                    params.read_repair = c.text("read_repair").unwrap_or_else(|| "BLOCKING".into());
                    params.memtable = c.text("memtable");
                    params.allow_auto_snapshot = c.boolean("allow_auto_snapshot");
                    params.incremental_backups = c.boolean("incremental_backups");
                    params.cdc = c.boolean("cdc");
                }
                let mut flags = TableFlags::default();
                if !is_view {
                    let raw = c.raw("flags").cloned().unwrap_or_default();
                    for f in nosql_types::collection_elements(&raw)
                        .unwrap_or_default()
                        .into_iter()
                        .flatten()
                    {
                        match f {
                            b"compound" => flags.compound = true,
                            b"counter" => flags.counter = true,
                            b"dense" => flags.dense = true,
                            b"super" => flags.super_ = true,
                            _ => {}
                        }
                    }
                } else {
                    flags.compound = true;
                }
                let key = (ksname.clone(), name.clone());
                let mut next_id = meta.as_ref().map(|m| m.next_column_id).unwrap_or(0);
                let mut ids = meta.as_ref().map(|m| m.ids.clone()).unwrap_or_default();
                let mut cols = Vec::new();
                let compact = if is_view {
                    CompactShape::None
                } else {
                    CompactShape::from_flags(&flags)
                };
                for (cname, ty, mut kind, position, reversed) in
                    columns.remove(&key).unwrap_or_default()
                {
                    // Hidden compact storage columns are not part of the model.
                    if ty == "empty"
                        || (compact == CompactShape::Static
                            && matches!(kind, ColumnKind::Clustering | ColumnKind::Regular))
                    {
                        continue;
                    }
                    if compact == CompactShape::Static && kind == ColumnKind::Static {
                        kind = ColumnKind::Regular;
                    }
                    let ty =
                        nosql_types::parse_cql_type(&ty, &ksname, &|_, n| ks.types.get(n).cloned())
                            .map_err(corrupt)?;
                    let id = *ids.entry(cname.clone()).or_insert_with(|| {
                        let id = next_id;
                        next_id += 1;
                        id
                    });
                    cols.push(Column {
                        name: cname,
                        ty,
                        kind,
                        position,
                        reversed,
                        id,
                    });
                }
                let version = meta.as_ref().map(|m| m.version).unwrap_or(id);
                let mut t = Table::new(
                    ksname.clone(),
                    name.clone(),
                    id,
                    version,
                    cols,
                    params,
                    flags,
                    TableKind::Regular,
                );
                t.dropped = dropped.remove(&key).unwrap_or_default();
                t.next_column_id = t.next_column_id.max(next_id);
                t.column_ids = ids;
                t.indexes = indexes.remove(&key).unwrap_or_default();
                t.triggers = triggers.remove(&key).unwrap_or_default();
                // Rows of columns that are gone are `column_masks` leftovers.
                let mut ms = masks.remove(&key).unwrap_or_default();
                for (column, m) in std::mem::take(&mut ms) {
                    if t.by_name.contains_key(&column) {
                        ms.insert(column, m);
                    } else {
                        ks.stale_masks.insert((name.clone(), column), m);
                    }
                }
                t.masks = ms;
                if is_view {
                    t.view = Some(ViewMeta {
                        base_table_id: c.uuid("base_table_id").unwrap_or([0; 16]),
                        base_table_name: c.text("base_table_name").unwrap_or_default(),
                        where_clause: c.text("where_clause").unwrap_or_default(),
                        include_all_columns: c.boolean("include_all_columns").unwrap_or(false),
                        computed: computed.remove(&key).unwrap_or_default(),
                    });
                }
                ks.tables.insert(name, Arc::new(t));
            }
        }
    }
    // ScyllaDB's per-table extras.
    if d.is_scylla() {
        let t = st.get("scylla_tables");
        for p in get("scylla_tables") {
            let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
            let Some(ks) = kss.get_mut(&ksname) else {
                continue;
            };
            for row in &p.rows {
                let name = clustering_text(row, 0);
                let c = Cells { table: t, row };
                let Some(table) = ks.tables.get_mut(&name) else {
                    continue;
                };
                let large = c.boolean("large_data_guardrails_enabled") == Some(true);
                let tablets = c.text_map("tablets");
                if large || !tablets.is_empty() {
                    let t = Arc::make_mut(table);
                    t.params.large_data_guardrails = large;
                    t.params.tablets = tablets;
                }
            }
        }
    }
    // Rows of dropped tables.
    for ((ksname, table), ms) in masks {
        if let Some(ks) = kss.get_mut(&ksname) {
            for (column, m) in ms {
                ks.stale_masks.insert((table.clone(), column), m);
            }
        }
    }
    let ft = st.get("functions");
    for p in get("functions") {
        let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
        let Some(ks) = kss.get_mut(&ksname) else {
            continue;
        };
        for row in &p.rows {
            let c = Cells { table: ft, row };
            let types = row.clustering.get(1).and_then(|b| {
                nosql_types::collection_elements(&b).ok().map(|v| {
                    v.into_iter()
                        .map(|e| String::from_utf8_lossy(e.unwrap_or_default()).into_owned())
                        .collect()
                })
            });
            ks.functions.push(FunctionMeta {
                name: clustering_text(row, 0),
                argument_types: types.unwrap_or_default(),
                argument_names: c.text_list("argument_names"),
                body: c.text("body").unwrap_or_default(),
                called_on_null_input: c.boolean("called_on_null_input").unwrap_or(false),
                language: c.text("language").unwrap_or_default(),
                return_type: c.text("return_type").unwrap_or_default(),
            });
        }
    }
    let at = st.get("aggregates");
    for p in get("aggregates") {
        let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
        let Some(ks) = kss.get_mut(&ksname) else {
            continue;
        };
        for row in &p.rows {
            let c = Cells { table: at, row };
            let types = row.clustering.get(1).and_then(|b| {
                nosql_types::collection_elements(&b).ok().map(|v| {
                    v.into_iter()
                        .map(|e| String::from_utf8_lossy(e.unwrap_or_default()).into_owned())
                        .collect()
                })
            });
            ks.aggregates.push(AggregateMeta {
                name: clustering_text(row, 0),
                argument_types: types.unwrap_or_default(),
                final_func: c.text("final_func"),
                initcond: c.text("initcond"),
                return_type: c.text("return_type").unwrap_or_default(),
                state_func: c.text("state_func").unwrap_or_default(),
                state_type: c.text("state_type").unwrap_or_default(),
                reduce_func: None,
            });
        }
    }
    // ScyllaDB's `REDUCEFUNC`s, keyed like the aggregates.
    if let Some(sat) = st.tables.get("scylla_aggregates") {
        for p in get("scylla_aggregates") {
            let ksname = String::from_utf8_lossy(&p.key.key).into_owned();
            let Some(ks) = kss.get_mut(&ksname) else {
                continue;
            };
            for row in &p.rows {
                let c = Cells { table: sat, row };
                let name = clustering_text(row, 0);
                let types = row.clustering.get(1).and_then(|b| {
                    nosql_types::collection_elements(&b).ok().map(|v| {
                        v.into_iter()
                            .map(|e| String::from_utf8_lossy(e.unwrap_or_default()).into_owned())
                            .collect::<Vec<_>>()
                    })
                });
                if let Some(a) = ks
                    .aggregates
                    .iter_mut()
                    .find(|a| a.name == name && Some(&a.argument_types) == types.as_ref())
                {
                    a.reduce_func = c.text("reduce_func");
                }
            }
        }
    }
    Ok(kss.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DbConfig;

    fn schema_tables(d: Dialect) -> SchemaTables {
        let kss = crate::system::builtin_keyspaces(&DbConfig::new("db", d));
        SchemaTables::new(d, kss.iter().find(|k| k.name == "system_schema").unwrap())
    }

    fn sample(d: Dialect) -> Keyspace {
        let mut ks = Keyspace::new(
            "ks",
            Replication::new("NetworkTopologyStrategy", &[("datacenter1", "1")]),
            KeyspaceKind::User,
        );
        ks.tablets = if d.is_scylla() { Some(0) } else { None };
        let addr = Arc::new(UdtDef::new(
            "ks",
            "addr",
            vec![
                ("street".into(), CqlType::Text),
                ("zip".into(), CqlType::Int),
            ],
        ));
        let person = Arc::new(UdtDef::new(
            "ks",
            "person",
            vec![("home".into(), CqlType::Udt(addr.clone(), true))],
        ));
        ks.types.insert("addr".into(), addr.clone());
        ks.types.insert("person".into(), person);
        let mut ck = Column {
            name: "ck".into(),
            ty: CqlType::Timestamp,
            kind: ColumnKind::Clustering,
            position: 0,
            reversed: true,
            id: 1,
        };
        ck.reversed = true;
        let cols = vec![
            Column {
                name: "pk".into(),
                ty: CqlType::Int,
                kind: ColumnKind::PartitionKey,
                position: 0,
                reversed: false,
                id: 0,
            },
            ck,
            Column {
                name: "m".into(),
                ty: CqlType::map(CqlType::Text, CqlType::Int, false),
                kind: ColumnKind::Regular,
                position: -1,
                reversed: false,
                id: 5,
            },
            Column {
                name: "a".into(),
                ty: CqlType::Udt(addr, false),
                kind: ColumnKind::Static,
                position: -1,
                reversed: false,
                id: 3,
            },
        ];
        let mut t = Table::new(
            "ks",
            "t",
            [9; 16],
            [8; 16],
            cols,
            TableParams::defaults(d, false),
            TableFlags {
                compound: true,
                ..Default::default()
            },
            TableKind::Regular,
        );
        t.dropped.insert(
            "old".into(),
            DroppedColumn {
                dropped_at: 1_700_000_000_123_456,
                ty: CqlType::list(CqlType::Text, false),
                kind: ColumnKind::Regular,
            },
        );
        t.column_ids.insert("old".into(), 4);
        t.next_column_id = 6;
        t.indexes.push(IndexMeta {
            name: "i".into(),
            kind: "COMPOSITES".into(),
            options: BTreeMap::from([("target".into(), "m".into())]),
        });
        ks.tables.insert("t".into(), Arc::new(t));
        ks
    }

    fn to_partitions(
        muts: Vec<(TableId, PartitionUpdate)>,
        st: &SchemaTables,
    ) -> BTreeMap<String, Vec<Partition>> {
        let mut out = BTreeMap::new();
        for (id, u) in muts {
            let name = st
                .tables
                .iter()
                .find(|(_, t)| t.table_id() == id)
                .unwrap()
                .0
                .clone();
            out.insert(
                name,
                vec![Partition {
                    key: u.key,
                    partition_deletion: DeletionTime::LIVE,
                    static_row: None,
                    rows: u.rows,
                    markers: vec![],
                }],
            );
        }
        out
    }

    #[test]
    fn round_trip_both_dialects() {
        for d in [Dialect::Scylla, Dialect::Cassandra] {
            let st = schema_tables(d);
            let ks = sample(d);
            let parts = to_partitions(keyspace_mutations(&ks, &st, 100, 1), &st);
            let back = load(&st, &parts).unwrap();
            assert_eq!(back.len(), 1);
            let b = &back[0];
            assert_eq!(b.replication, ks.replication);
            assert_eq!(b.tablets, ks.tablets);
            assert_eq!(b.types, ks.types);
            let (t, bt) = (&ks.tables["t"], &b.tables["t"]);
            assert_eq!(bt.columns, t.columns, "{d}");
            assert_eq!(bt.params, t.params, "{d}");
            assert_eq!(bt.version, t.version);
            assert_eq!(
                bt.dropped["old"].dropped_at,
                if d.is_scylla() {
                    1_700_000_000_123_456
                } else {
                    1_700_000_000_123_000
                }
            );
            assert_eq!(bt.column_ids, t.column_ids);
            assert_eq!(bt.next_column_id, 6);
            assert_eq!(bt.indexes, t.indexes);
            assert_eq!(bt.flags, t.flags);
        }
    }

    #[test]
    fn stored_values_match_the_references() {
        let st = schema_tables(Dialect::Scylla);
        let rows = keyspace_rows(&sample(Dialect::Scylla), &st, 5);
        let cols = st.get("columns");
        let ck = rows["columns"]
            .iter()
            .find(|r| clustering_text(r, 1) == "ck")
            .unwrap();
        let c = Cells {
            table: cols,
            row: ck,
        };
        assert_eq!(c.text("clustering_order").unwrap(), "DESC");
        assert_eq!(c.text("kind").unwrap(), "clustering");
        let a = rows["columns"]
            .iter()
            .find(|r| clustering_text(r, 1) == "a")
            .unwrap();
        assert_eq!(
            Cells {
                table: cols,
                row: a
            }
            .text("type")
            .unwrap(),
            "addr"
        );
        assert_eq!(rows["scylla_keyspaces"].len(), 1);
        assert_eq!(rows["scylla_tables"].len(), 1);
        let st = schema_tables(Dialect::Cassandra);
        let rows = keyspace_rows(&sample(Dialect::Cassandra), &st, 5);
        let t = Cells {
            table: st.get("tables"),
            row: &rows["tables"][0],
        };
        assert_eq!(t.double("read_repair_chance"), Some(0.0));
        assert_eq!(t.text("read_repair").unwrap(), "BLOCKING");
        assert!(t.raw("cdc").is_none());
    }

    #[test]
    fn corrupt_meta_is_rejected_gracefully() {
        assert!(parse_meta(&[1, 2, 3]).is_none());
        assert!(parse_meta(&[]).is_none());
    }
}
