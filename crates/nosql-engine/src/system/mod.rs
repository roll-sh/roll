//! Built-in keyspaces and the virtual-table framework.
//!
//! Built-in keyspaces (`system`, `system_schema`, `system_auth`, …) are compiled in per dialect:
//! [`builtin_keyspaces`] returns their definitions, which join the user keyspaces in every schema
//! snapshot. Their tables come in three kinds ([`TableKind`]):
//!
//! - `system_schema.*` are real storage tables (they hold user keyspaces' schema);
//! - generated tables ([`VirtualTable`]) produce their rows from engine state on every read
//!   (`system.local`, `system.peers`, `system.size_estimates`, …);
//! - other subsystems may define storage-backed system tables (`system_auth.roles`, …).
//!
//! # Adding a system or virtual table
//!
//! 1. Describe it in [`definitions`] with [`def`] and [`c`] (column name, CQL type, kind `p`/`c`/
//!    `s`/`r`, position) — exactly the columns `system_schema.columns` shows on the reference.
//! 2. For a generated table, implement [`VirtualTable`] (rows as `(column, value)` pairs) and
//!    add it in [`Registry::new`].
//!
//! Reads go through the normal SELECT pipeline, so restrictions, paging, aggregates and JSON work
//! on every system table.

pub mod catalog;
#[rustfmt::skip]
mod catalog_data;
pub mod clients;
pub mod local;
#[rustfmt::skip]
pub(crate) mod rows_data;
pub mod schema_tables;
pub mod views;

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use nosql_types::{CqlType, Dialect};

use crate::config::DbConfig;
use crate::error::Result;
use crate::schema::{
    Column, ColumnKind, Keyspace, KeyspaceKind, Replication, Table, TableFlags, TableKind,
    TableParams,
};

/// A column of a built-in table: name, CQL type text, kind and position.
pub struct ColDef {
    pub name: &'static str,
    pub ty: &'static str,
    pub kind: char,
    pub position: i32,
    /// A clustering column in `DESC` order.
    pub reversed: bool,
}

pub fn c(name: &'static str, ty: &'static str, kind: char, position: i32) -> ColDef {
    ColDef {
        name,
        ty,
        kind,
        position,
        reversed: false,
    }
}

/// A built-in table.
pub struct TableDef {
    pub keyspace: &'static str,
    pub name: &'static str,
    pub comment: &'static str,
    pub gc_grace_seconds: i32,
    pub memtable_flush_period_in_ms: i32,
    pub kind: TableKind,
    pub columns: Vec<ColDef>,
    /// Every option (the reference catalog's, [`catalog`]); `None`: the dialect's system-table
    /// defaults with `comment`, `gc_grace_seconds` and `memtable_flush_period_in_ms` above.
    pub params: Option<TableParams>,
    /// `None`: `compound`.
    pub flags: Option<TableFlags>,
}

pub fn def(
    keyspace: &'static str,
    name: &'static str,
    comment: &'static str,
    gc_grace_seconds: i32,
    kind: TableKind,
    columns: Vec<ColDef>,
) -> TableDef {
    TableDef {
        keyspace,
        name,
        comment,
        gc_grace_seconds,
        memtable_flush_period_in_ms: 0,
        kind,
        columns,
        params: None,
        flags: None,
    }
}

impl TableDef {
    /// The table's metadata. Column ids follow declaration order.
    pub fn build(&self, d: Dialect) -> Table {
        self.build_with(d, &BTreeMap::new())
    }

    /// [`Self::build`] with the user types of the table's keyspace.
    pub fn build_with(
        &self,
        d: Dialect,
        types: &BTreeMap<String, Arc<nosql_types::UdtDef>>,
    ) -> Table {
        let columns = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, cd)| Column {
                name: cd.name.to_string(),
                // `empty` (EmptyType) has no CQL name; it only appears in built-in tables.
                ty: if cd.ty == "empty" {
                    Ok(CqlType::Empty)
                } else {
                    nosql_types::parse_cql_type(cd.ty, self.keyspace, &|_, n| types.get(n).cloned())
                }
                .unwrap_or_else(|e| {
                    panic!(
                        "built-in type {} of {}.{}: {e}",
                        cd.ty, self.keyspace, self.name
                    )
                }),
                kind: match cd.kind {
                    'p' => ColumnKind::PartitionKey,
                    'c' => ColumnKind::Clustering,
                    's' => ColumnKind::Static,
                    _ => ColumnKind::Regular,
                },
                position: cd.position,
                reversed: cd.reversed,
                id: i as u32,
            })
            .collect();
        let params = self.params.clone().unwrap_or_else(|| {
            let mut params = TableParams::defaults(d, true);
            params.comment = self.comment.to_string();
            params.gc_grace_seconds = self.gc_grace_seconds;
            params.memtable_flush_period_in_ms = self.memtable_flush_period_in_ms;
            params
        });
        let id = crate::uuid::system_table_id(self.keyspace, self.name);
        Table::new(
            self.keyspace,
            self.name,
            id,
            id,
            columns,
            params,
            self.flags.unwrap_or(TableFlags {
                compound: true,
                ..Default::default()
            }),
            self.kind,
        )
    }
}

/// Every built-in table definition of the dialect: the reference catalog ([`catalog`]),
/// overridden (columns and kind) by the definitions of the subsystems that own a table. Options
/// always come from the catalog when it lists the table.
pub fn definitions(d: Dialect) -> Vec<TableDef> {
    let mut v = catalog::tables(d);
    v.extend(schema_tables::tables(d));
    v.extend(local::tables(d));
    v.extend(crate::auth::tables::tables(d));
    let mut out: Vec<TableDef> = Vec::with_capacity(v.len());
    for mut t in v {
        if let Some(i) = out
            .iter()
            .position(|o| o.keyspace == t.keyspace && o.name == t.name)
        {
            let catalog = out.remove(i);
            if t.params.is_none() {
                t.params = catalog.params;
                t.flags = t.flags.or(catalog.flags);
            }
        }
        out.push(t);
    }
    out
}

/// The built-in keyspaces with their tables.
pub fn builtin_keyspaces(cfg: &DbConfig) -> Vec<Keyspace> {
    let d = cfg.dialect;
    let mut kss: BTreeMap<&str, Keyspace> = BTreeMap::new();
    let mut add = |name: &'static str, r: Replication| {
        kss.insert(name, Keyspace::new(name, r, KeyspaceKind::System));
    };
    add("system", Replication::local());
    add("system_schema", Replication::local());
    add("system_auth", Replication::simple(1));
    add("system_distributed", Replication::simple(3));
    add("system_traces", Replication::simple(2));
    if d.is_scylla() {
        add(
            "system_replicated_keys",
            Replication::new("EverywhereStrategy", &[]),
        );
        add(
            "audit",
            Replication::new("NetworkTopologyStrategy", &[(cfg.datacenter.as_str(), "3")]),
        );
    }
    for name in catalog::virtual_keyspaces(d) {
        kss.insert(
            name,
            Keyspace::new(*name, Replication::local(), KeyspaceKind::Virtual),
        );
    }
    for ks in kss.values_mut() {
        ks.types = catalog::types(d, &ks.name);
    }
    for t in definitions(d) {
        if let Some(ks) = kss.get_mut(t.keyspace) {
            let table = t.build_with(d, &ks.types);
            ks.tables.insert(t.name.to_string(), Arc::new(table));
        }
    }
    kss.into_values().collect()
}

/// What a generated table can see.
pub struct VirtualContext<'a> {
    pub db: &'a crate::Database,
    pub cfg: &'a DbConfig,
    pub schema: &'a crate::schema::Schema,
    /// When the database was opened (Unix seconds): `gossip_generation`.
    pub started_at: i64,
}

/// A table whose rows are generated on every read.
pub trait VirtualTable: Send + Sync {
    /// Every row as `(column, value)` pairs; missing columns are null. Non-frozen collections
    /// hold their whole serialised value.
    fn rows(&self, table: &Table, ctx: &VirtualContext) -> Result<Vec<VRow>>;
}

/// A generated row.
pub type VRow = Vec<(&'static str, Option<Bytes>)>;

/// The generated tables of a database.
pub struct Registry {
    tables: HashMap<(String, String), Arc<dyn VirtualTable>>,
}

impl Registry {
    pub fn new(d: Dialect) -> Registry {
        let mut r = Registry {
            tables: HashMap::new(),
        };
        local::register(&mut r, d);
        views::register(&mut r, d);
        crate::views::status::register(&mut r, d);
        r
    }

    pub fn add(&mut self, keyspace: &str, table: &str, t: Arc<dyn VirtualTable>) {
        self.tables
            .insert((keyspace.to_string(), table.to_string()), t);
    }

    pub fn get(&self, keyspace: &str, table: &str) -> Option<&Arc<dyn VirtualTable>> {
        self.tables.get(&(keyspace.to_string(), table.to_string()))
    }
}

/// Serialisation helpers for generated rows.
pub mod val {
    use super::*;

    pub fn text(s: &str) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(s.as_bytes()))
    }

    pub fn int(v: i32) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(&v.to_be_bytes()))
    }

    pub fn bigint(v: i64) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(&v.to_be_bytes()))
    }

    pub fn boolean(v: bool) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(&[v as u8]))
    }

    pub fn double(v: f64) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(&v.to_be_bytes()))
    }

    pub fn uuid(u: &[u8; 16]) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(u))
    }

    pub fn inet(a: &std::net::IpAddr) -> Option<Bytes> {
        Some(match a {
            std::net::IpAddr::V4(v) => Bytes::copy_from_slice(&v.octets()),
            std::net::IpAddr::V6(v) => Bytes::copy_from_slice(&v.octets()),
        })
    }

    pub fn text_set<'a>(items: impl IntoIterator<Item = &'a str>) -> Option<Bytes> {
        let mut v: Vec<&str> = items.into_iter().collect();
        v.sort_unstable();
        v.dedup();
        Some(nosql_types::pack_collection(v.iter().map(|s| s.as_bytes())))
    }

    pub fn text_list<'a>(items: impl IntoIterator<Item = &'a str>) -> Option<Bytes> {
        Some(nosql_types::pack_collection(
            items.into_iter().map(|s| s.as_bytes()),
        ))
    }

    pub fn text_map(m: &BTreeMap<String, String>) -> Option<Bytes> {
        Some(nosql_types::pack_map(
            m.iter().map(|(k, v)| (k.as_bytes(), v.as_bytes())),
        ))
    }

    pub fn blob_map(m: &BTreeMap<String, Bytes>) -> Option<Bytes> {
        Some(nosql_types::pack_map(
            m.iter().map(|(k, v)| (k.as_bytes(), &v[..])),
        ))
    }

    /// Validates a generated value against its column type (debug builds).
    pub fn check(ty: &CqlType, v: &Option<Bytes>) -> bool {
        v.as_ref()
            .is_none_or(|b| nosql_types::validate(ty, b, Dialect::Cassandra).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_definitions_parse_in_both_dialects() {
        for d in [Dialect::Scylla, Dialect::Cassandra] {
            let kss = builtin_keyspaces(&DbConfig::new("db", d));
            let schema = kss.iter().find(|k| k.name == "system_schema").unwrap();
            assert!(schema.tables.contains_key("tables"));
            assert_eq!(schema.tables.contains_key("scylla_tables"), d.is_scylla());
            assert_eq!(schema.tables.contains_key("column_masks"), !d.is_scylla());
            let local = &kss.iter().find(|k| k.name == "system").unwrap().tables["local"];
            assert_eq!(local.columns[0].name, "key");
            assert_eq!(
                crate::uuid::format(&local.id),
                "7ad54392-bcdd-35a6-8417-4e047860b377"
            );
        }
    }
}
