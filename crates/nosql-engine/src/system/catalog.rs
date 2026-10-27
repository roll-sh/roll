//! Every table of the built-in keyspaces as the reference images define them: columns, the
//! `system_schema.tables` options (comment, compaction, caching, TTL, `gc_grace_seconds`, flags,
//! ScyllaDB's `tombstone_gc` extension), ScyllaDB's system user types and Cassandra's virtual
//! keyspaces (`system_views`, `system_virtual_schema`).
//!
//! The data ([`catalog_data`](super::catalog_data)) is generated from the reference catalog
//! captures by `scripts/gen_system_catalog.py`, so `DESCRIBE FULL SCHEMA`, `system_schema.*` and
//! `system_virtual_schema.*` list the same tables with the same options as a fresh reference
//! node. Definitions made elsewhere (`system/local.rs`, `system/schema_tables.rs`, the auth
//! tables, …) take precedence for columns and kind; the options always come from here.
//!
//! Tables are generated ([`TableKind::Virtual`]: rows from a registered
//! [`super::VirtualTable`], none otherwise) except the stored ones: `system_traces.*`, which
//! tracing writes, `audit.audit_log` ([`crate::audit`]), and the [`stored`] tables of `system`
//! roll has no generator for, which ScyllaDB lets a superuser write (its Alternator suite
//! writes `scylla_table_schema_history` with CQL and reads it back through Alternator).

use std::collections::BTreeMap;
use std::sync::Arc;

use nosql_types::{Dialect, UdtDef};

use super::catalog_data;
use super::{ColDef, TableDef};
use crate::schema::{TableFlags, TableKind, TableParams};

/// A value of a `system_schema.tables` option.
pub enum O {
    Text(&'static str),
    Int(i64),
    Float(f64),
    Bool(bool),
    Map(&'static [(&'static str, &'static str)]),
    Flags(&'static [&'static str]),
}

/// A built-in table: columns `(name, type, kind, position, reversed)` and options.
pub struct Cat {
    pub keyspace: &'static str,
    pub name: &'static str,
    /// A Cassandra virtual table (`system_virtual_schema.tables`).
    pub is_virtual: bool,
    pub columns: &'static [(&'static str, &'static str, char, i32, bool)],
    pub options: &'static [(&'static str, O)],
}

/// The dialect's built-in tables.
pub fn entries(d: Dialect) -> &'static [Cat] {
    match d {
        Dialect::Scylla => catalog_data::SCYLLA,
        Dialect::Cassandra => catalog_data::CASSANDRA,
    }
}

/// The catalog entry of `keyspace.name`.
pub fn entry(d: Dialect, keyspace: &str, name: &str) -> Option<&'static Cat> {
    entries(d)
        .iter()
        .find(|c| c.keyspace == keyspace && c.name == name)
}

/// Cassandra's virtual keyspaces (none in ScyllaDB).
pub fn virtual_keyspaces(d: Dialect) -> &'static [&'static str] {
    match d {
        Dialect::Scylla => &[],
        Dialect::Cassandra => &["system_views", "system_virtual_schema"],
    }
}

/// Whether `keyspace` is one of the dialect's virtual keyspaces.
pub fn is_virtual_keyspace(d: Dialect, keyspace: &str) -> bool {
    virtual_keyspaces(d).contains(&keyspace)
}

/// A built-in user type: keyspace, name, `(field, type)`s.
pub type TypeEntry = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

/// The user types of the built-in keyspaces, in `DESCRIBE` order.
pub fn type_entries(d: Dialect) -> &'static [TypeEntry] {
    match d {
        Dialect::Scylla => catalog_data::SCYLLA_TYPES,
        Dialect::Cassandra => catalog_data::CASSANDRA_TYPES,
    }
}

/// The user types of built-in keyspace `keyspace`, resolved (types may nest earlier ones).
pub fn types(d: Dialect, keyspace: &str) -> BTreeMap<String, Arc<UdtDef>> {
    let mut out: BTreeMap<String, Arc<UdtDef>> = BTreeMap::new();
    for (ks, name, fields) in type_entries(d) {
        if *ks != keyspace {
            continue;
        }
        let fields = fields
            .iter()
            .map(|(f, ty)| {
                let t = nosql_types::parse_cql_type(ty, ks, &|_, n| out.get(n).cloned())
                    .unwrap_or_else(|e| panic!("built-in type {ks}.{name}.{f}: {e}"));
                (f.to_string(), t)
            })
            .collect();
        out.insert(name.to_string(), Arc::new(UdtDef::new(*ks, *name, fields)));
    }
    out
}

/// Indexes on built-in tables (`keyspace`, `table`, `index`, `DESCRIBE` statement).
pub fn indexes(d: Dialect) -> &'static [(&'static str, &'static str, &'static str, &'static str)] {
    match d {
        Dialect::Scylla => catalog_data::SCYLLA_INDEXES,
        Dialect::Cassandra => catalog_data::CASSANDRA_INDEXES,
    }
}

/// Built-in tables outside `system_traces` that are stored rather than generated.
fn stored(keyspace: &str, name: &str) -> bool {
    matches!(
        (keyspace, name),
        ("audit", "audit_log") | ("system", "scylla_table_schema_history")
    )
}

/// The table definitions of the catalog.
pub fn tables(d: Dialect) -> Vec<TableDef> {
    entries(d)
        .iter()
        .map(|c| {
            let kind = if c.keyspace == "system_traces" || stored(c.keyspace, c.name) {
                TableKind::Regular
            } else {
                TableKind::Virtual
            };
            // The `empty` value column of a dense compact table is hidden (not in the model,
            // as for user tables; `system_schema.columns` and DESCRIBE add it back).
            let columns = c
                .columns
                .iter()
                .filter(|&&(_, ty, ..)| ty != "empty")
                .map(|&(name, ty, kind, position, reversed)| ColDef {
                    name,
                    ty,
                    kind,
                    position,
                    reversed,
                })
                .collect();
            let mut t = super::def(c.keyspace, c.name, "", 0, kind, columns);
            let (params, flags) = options(d, c);
            t.comment = c
                .options
                .iter()
                .find_map(|(k, v)| match (k, v) {
                    (&"comment", O::Text(s)) => Some(*s),
                    _ => None,
                })
                .unwrap_or("");
            t.params = Some(params);
            t.flags = Some(flags);
            t
        })
        .collect()
}

/// The options and flags of a catalog table.
pub fn options(d: Dialect, c: &Cat) -> (TableParams, TableFlags) {
    let mut p = TableParams::defaults(d, true);
    let mut flags = TableFlags {
        compound: true,
        ..Default::default()
    };
    let map = |m: &[(&str, &str)]| -> BTreeMap<String, String> {
        m.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    for (k, v) in c.options {
        match (*k, v) {
            ("bloom_filter_fp_chance", O::Float(f)) => p.bloom_filter_fp_chance = *f,
            ("crc_check_chance", O::Float(f)) => p.crc_check_chance = *f,
            ("caching", O::Map(m)) => p.caching = map(m),
            ("compaction", O::Map(m)) => p.compaction = map(m),
            ("compression", O::Map(m)) => p.compression = map(m),
            ("tombstone_gc", O::Map(m)) => p.tombstone_gc = map(m),
            ("comment", O::Text(s)) => p.comment = s.to_string(),
            ("speculative_retry", O::Text(s)) => p.speculative_retry = s.to_string(),
            ("additional_write_policy", O::Text(s)) => p.additional_write_policy = s.to_string(),
            ("read_repair", O::Text(s)) => p.read_repair = s.to_string(),
            ("default_time_to_live", O::Int(i)) => p.default_time_to_live = *i as i32,
            ("gc_grace_seconds", O::Int(i)) => p.gc_grace_seconds = *i as i32,
            ("max_index_interval", O::Int(i)) => p.max_index_interval = *i as i32,
            ("min_index_interval", O::Int(i)) => p.min_index_interval = *i as i32,
            ("memtable_flush_period_in_ms", O::Int(i)) => p.memtable_flush_period_in_ms = *i as i32,
            ("flags", O::Flags(fs)) => {
                flags = TableFlags {
                    compound: fs.contains(&"compound"),
                    dense: fs.contains(&"dense"),
                    counter: fs.contains(&"counter"),
                    super_: fs.contains(&"super"),
                }
            }
            // Read-repair chances (legacy, always 0/null) and options a fresh node leaves null.
            _ => {}
        }
    }
    (p, flags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_builds() {
        for d in [Dialect::Scylla, Dialect::Cassandra] {
            for t in tables(d) {
                let types = types(d, t.keyspace);
                let built = t.build_with(d, &types);
                assert_eq!(
                    built.columns.len(),
                    t.columns.len(),
                    "{}.{}",
                    t.keyspace,
                    t.name
                );
            }
        }
        let e = entry(Dialect::Scylla, "system_traces", "sessions").unwrap();
        let (p, _) = options(Dialect::Scylla, e);
        assert_eq!(p.default_time_to_live, 86400);
        assert_eq!(
            p.tombstone_gc.get("mode").map(String::as_str),
            Some("repair")
        );
        let e = entry(Dialect::Scylla, "system", "IndexInfo").unwrap();
        let (_, f) = options(Dialect::Scylla, e);
        assert!(f.dense && !f.compound);
        assert!(
            entry(Dialect::Cassandra, "system_views", "settings")
                .unwrap()
                .is_virtual
        );
        assert_eq!(types(Dialect::Scylla, "system").len(), 3);
    }
}
