//! CREATE statements generated from the schema tables, as the Python driver's metadata exports
//! them (`KeyspaceMetadata.export_as_string`, `TableMetadataV3`, `UserType`, `IndexMetadata`,
//! `MaterializedViewMetadata` and `SchemaParserV3` in `cassandra/metadata.py` of the DataStax
//! driver bundled with Apache Cassandra, Apache-2.0). ScyllaDB's cqlsh describes this way (see
//! [`super::describe`]).

use bytes::Bytes;
use nosql_proto::{ColType, Consistency, Rows};

use super::lex::protect_name;
use super::pyfmt;
use super::session::cell;
use super::shell::Shell;

/// cqlsh's `SYSTEM_KEYSPACES` (skipped by DESCRIBE SCHEMA).
pub fn is_system(ks: &str) -> bool {
    matches!(
        ks,
        "system" | "system_schema" | "system_traces" | "system_auth" | "system_distributed" | "system_views" | "system_virtual_schema"
    )
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

async fn rows(shell: &Shell, q: String) -> Option<Rows> {
    shell.conn.all_rows(&q, Consistency::One).await.ok().flatten()
}

/// One schema row with its column types.
struct Row<'a> {
    rows: &'a Rows,
    cells: Vec<Option<Bytes>>,
}

impl Row<'_> {
    fn has(&self, name: &str) -> bool {
        self.rows.metadata.columns.iter().any(|c| c.name == name)
    }

    fn raw(&self, name: &str) -> Option<&Bytes> {
        cell(self.rows, &self.cells, name)
    }

    fn text(&self, name: &str) -> Option<String> {
        self.raw(name).map(|b| String::from_utf8_lossy(b).into_owned())
    }

    fn texts(&self, name: &str) -> Vec<String> {
        self.raw(name)
            .and_then(|b| nosql_types::collection_elements(b).ok())
            .unwrap_or_default()
            .iter()
            .map(|e| e.map(|e| String::from_utf8_lossy(e).into_owned()).unwrap_or_default())
            .collect()
    }

    fn map(&self, name: &str) -> Option<Vec<(String, String)>> {
        let b = self.raw(name)?;
        let es = nosql_types::map_entries(b).ok()?;
        Some(es.iter().map(|(k, v)| (String::from_utf8_lossy(k).into_owned(), String::from_utf8_lossy(v).into_owned())).collect())
    }

    fn int(&self, name: &str) -> Option<i64> {
        let b = self.raw(name)?;
        match b.len() {
            4 => Some(i32::from_be_bytes(b[..].try_into().ok()?) as i64),
            8 => Some(i64::from_be_bytes(b[..].try_into().ok()?)),
            _ => None,
        }
    }

    fn boolean(&self, name: &str) -> Option<bool> {
        self.raw(name).and_then(|b| b.first()).map(|b| *b != 0)
    }

    fn ty(&self, name: &str) -> Option<&ColType> {
        self.rows.metadata.columns.iter().find(|c| c.name == name).map(|c| &c.ty)
    }

    /// `protect_value` of an option, or a `{'k': 'v'}` map; `None` when null.
    fn option(&self, name: &str) -> Option<String> {
        let ty = self.ty(name)?.clone();
        let b = self.raw(name)?;
        Some(match ty {
            ColType::Map(..) => {
                let es = nosql_types::map_entries(b).ok()?;
                let parts: Vec<String> = es
                    .iter()
                    .map(|(k, v)| format!("'{}': '{}'", String::from_utf8_lossy(k), String::from_utf8_lossy(v)))
                    .collect();
                format!("{name} = {{{}}}", parts.join(", "))
            }
            ColType::Double => format!("{name} = {}", pyfmt::float_repr(f64::from_be_bytes(b[..].try_into().ok()?)).to_lowercase()),
            ColType::Float => format!("{name} = {}", pyfmt::float_repr(f32::from_be_bytes(b[..].try_into().ok()?) as f64).to_lowercase()),
            ColType::Int | ColType::Bigint => format!("{name} = {}", self.int(name)?),
            ColType::Boolean => format!("{name} = {}", if b.first() == Some(&1) { "true" } else { "false" }),
            _ => format!("{name} = {}", lit(&String::from_utf8_lossy(b))),
        })
    }
}

fn each(rows: &Rows) -> Vec<Row<'_>> {
    rows.to_vec().into_iter().map(|cells| Row { rows, cells }).collect()
}

const TABLE_OPTIONS: &[&str] = &[
    "bloom_filter_fp_chance",
    "caching",
    "cdc",
    "comment",
    "compaction",
    "compression",
    "crc_check_chance",
    "dclocal_read_repair_chance",
    "default_time_to_live",
    "gc_grace_seconds",
    "max_index_interval",
    "memtable_flush_period_in_ms",
    "min_index_interval",
    "read_repair_chance",
    "speculative_retry",
];

/// `TableMetadataV3._make_option_strings`, sorted.
fn option_strings(r: &Row) -> Vec<String> {
    let mut out: Vec<String> = TABLE_OPTIONS
        .iter()
        .filter(|o| r.has(o))
        .filter_map(|o| r.option(o))
        .collect();
    out.sort();
    out
}

struct Column {
    name: String,
    ty: String,
    is_static: bool,
    reversed: bool,
}

struct TableShape {
    columns: Vec<Column>,
    partition: Vec<String>,
    clustering: Vec<String>,
    compact: bool,
}

/// `_build_table_columns` over the table's `system_schema.columns` rows.
fn shape(cols: &[Row], flags: Option<Vec<String>>, view: bool) -> TableShape {
    let (compact_static, compact, dense) = match flags {
        _ if view => (false, false, false),
        Some(f) if !f.is_empty() => {
            let dense = f.iter().any(|x| x == "dense");
            let sup = f.iter().any(|x| x == "super");
            let compound = f.iter().any(|x| x == "compound");
            (!dense && !sup && !compound, dense || sup || !compound, dense)
        }
        _ => (true, true, false),
    };
    let col = |r: &Row| Column {
        name: r.text("column_name").unwrap_or_default(),
        ty: r.text("type").unwrap_or_default(),
        is_static: r.text("kind").as_deref() == Some("static"),
        reversed: r.text("clustering_order").is_some_and(|o| o.eq_ignore_ascii_case("desc")),
    };
    let mut columns: Vec<Column> = vec![];
    let keyed = |kind: &str, columns: &mut Vec<Column>| -> Vec<String> {
        let mut rs: Vec<&Row> = cols.iter().filter(|r| r.text("kind").as_deref() == Some(kind)).collect();
        rs.sort_by_key(|r| r.int("position").unwrap_or(0));
        rs.iter()
            .map(|r| {
                let c = col(r);
                let n = c.name.clone();
                columns.push(c);
                n
            })
            .collect()
    };
    let partition = keyed("partition_key", &mut columns);
    let clustering = if compact_static { vec![] } else { keyed("clustering", &mut columns) };
    for r in cols {
        let kind = r.text("kind");
        if kind.as_deref() == Some("partition_key") {
            continue;
        }
        let mut c = col(r);
        if columns.iter().any(|x| x.name == c.name) {
            continue;
        }
        if dense && c.ty == "empty" {
            continue;
        }
        if compact_static && !c.is_static {
            continue;
        }
        if compact_static {
            c.is_static = false;
        }
        columns.push(c);
    }
    TableShape { columns, partition, clustering, compact }
}

fn property_string(shape: &TableShape, opts: Vec<String>) -> String {
    let mut props = vec![];
    if shape.compact {
        props.push("COMPACT STORAGE".to_string());
    }
    if !shape.clustering.is_empty() {
        let inner: Vec<String> = shape
            .clustering
            .iter()
            .map(|n| {
                let rev = shape.columns.iter().find(|c| &c.name == n).is_some_and(|c| c.reversed);
                format!("{} {}", protect_name(n), if rev { "DESC" } else { "ASC" })
            })
            .collect();
        props.push(format!("CLUSTERING ORDER BY ({})", inner.join(", ")));
    }
    props.extend(opts);
    props.join("\n    AND ")
}

fn table_cql(ks: &str, name: &str, shape: &TableShape, opts: Vec<String>) -> String {
    let mut cols: Vec<String> = shape
        .columns
        .iter()
        .map(|c| format!("{} {}{}", protect_name(&c.name), c.ty, if c.is_static { " static" } else { "" }))
        .collect();
    if shape.partition.len() == 1 && shape.clustering.is_empty() && !cols.is_empty() {
        cols[0].push_str(" PRIMARY KEY");
    }
    let mut ret = format!("CREATE TABLE {}.{} (\n", protect_name(ks), protect_name(name));
    ret.push_str(&cols.iter().map(|c| format!("    {c}")).collect::<Vec<_>>().join(",\n"));
    if shape.partition.len() > 1 || !shape.clustering.is_empty() {
        ret.push_str(",\n    PRIMARY KEY (");
        if shape.partition.len() > 1 {
            ret.push_str(&format!("({})", shape.partition.iter().map(|n| protect_name(n)).collect::<Vec<_>>().join(", ")));
        } else if let Some(p) = shape.partition.first() {
            ret.push_str(&protect_name(p));
        }
        if !shape.clustering.is_empty() {
            ret.push_str(&format!(", {}", shape.clustering.iter().map(|n| protect_name(n)).collect::<Vec<_>>().join(", ")));
        }
        ret.push(')');
    }
    ret.push_str("\n) WITH ");
    ret.push_str(&property_string(shape, opts));
    ret
}

fn index_cql(ks: &str, r: &Row) -> Option<String> {
    let name = r.text("index_name")?;
    let table = r.text("table_name").unwrap_or_default();
    let mut options = r.map("options").unwrap_or_default();
    let target = options.iter().position(|(k, _)| k == "target").map(|i| options.remove(i).1).unwrap_or_default();
    if r.text("kind").as_deref() != Some("CUSTOM") {
        return Some(format!("CREATE INDEX {} ON {}.{} ({target})", protect_name(&name), protect_name(ks), protect_name(&table)));
    }
    let class = options.iter().position(|(k, _)| k == "class_name").map(|i| options.remove(i).1).unwrap_or_default();
    let mut ret = format!("CREATE CUSTOM INDEX {} ON {}.{} ({target}) USING '{class}'", protect_name(&name), protect_name(ks), protect_name(&table));
    if !options.is_empty() {
        let parts: Vec<String> = options.iter().map(|(k, v)| format!("{}: {}", lit(k), lit(v))).collect();
        ret.push_str(&format!(" WITH OPTIONS = {{{}}}", parts.join(", ")));
    }
    Some(ret)
}

/// Everything about one keyspace the exports need.
struct Schema {
    tables: Option<Rows>,
    columns: Option<Rows>,
    indexes: Option<Rows>,
    views: Option<Rows>,
}

async fn schema(shell: &Shell, ks: &str) -> Schema {
    let w = format!(" WHERE keyspace_name = {}", lit(ks));
    Schema {
        tables: rows(shell, format!("SELECT * FROM system_schema.tables{w}")).await,
        columns: rows(shell, format!("SELECT * FROM system_schema.columns{w}")).await,
        indexes: rows(shell, format!("SELECT * FROM system_schema.indexes{w}")).await,
        views: rows(shell, format!("SELECT * FROM system_schema.views{w}")).await,
    }
}

impl Schema {
    fn columns_of(&self, table: &str) -> Vec<Row<'_>> {
        self.columns.as_ref().map(each).unwrap_or_default().into_iter().filter(|r| r.text("table_name").as_deref() == Some(table)).collect()
    }

    fn view_cql(&self, ks: &str, r: &Row) -> String {
        let name = r.text("view_name").unwrap_or_default();
        let cols = self.columns_of(&name);
        let shape = shape(&cols, None, true);
        let selected = if r.boolean("include_all_columns").unwrap_or(false) {
            "*".to_string()
        } else {
            shape.columns.iter().map(|c| protect_name(&c.name)).collect::<Vec<_>>().join(", ")
        };
        let part = shape.partition.iter().map(|n| protect_name(n)).collect::<Vec<_>>().join(", ");
        let mut pk = if shape.partition.len() > 1 { format!("(({part})") } else { format!("({part}") };
        if !shape.clustering.is_empty() {
            pk.push_str(&format!(", {}", shape.clustering.iter().map(|n| protect_name(n)).collect::<Vec<_>>().join(", ")));
        }
        pk.push(')');
        let props = property_string(&TableShape { compact: false, ..shape }, option_strings(r));
        let sep = "\n    ";
        format!(
            "CREATE MATERIALIZED VIEW {k}.{n} AS{sep}SELECT {selected}{sep}FROM {k}.{base}{sep}WHERE {wc}{sep}PRIMARY KEY {pk}{sep}WITH {props}",
            k = protect_name(ks),
            n = protect_name(&name),
            base = protect_name(&r.text("base_table_name").unwrap_or_default()),
            wc = r.text("where_clause").unwrap_or_default(),
        )
    }

    /// `TableMetadata._all_as_cql`: the table, its indexes and its views.
    fn table_export(&self, shell: &Shell, ks: &str, r: &Row) -> String {
        let name = r.text("table_name").unwrap_or_default();
        let cols = self.columns_of(&name);
        let flags = r.has("flags").then(|| r.texts("flags"));
        let shape = shape(&cols, flags, false);
        let mut ret = table_cql(ks, &name, &shape, option_strings(r));
        ret.push(';');
        for i in self.indexes.as_ref().map(each).unwrap_or_default() {
            if i.text("table_name").as_deref() == Some(&name)
                && let Some(q) = index_cql(ks, &i)
            {
                ret.push_str(&format!("\n{q};"));
            }
        }
        let views = self.views.as_ref().map(each).unwrap_or_default();
        let names: Vec<String> = views.iter().filter_map(|v| v.text("view_name")).collect();
        for vn in shell.meta.tables(ks, names) {
            let Some(v) = views.iter().find(|v| v.text("view_name").as_deref() == Some(&vn)) else { continue };
            if v.text("base_table_name").as_deref() == Some(&name) && shell.meta.knows_view(ks, &vn) {
                ret.push_str(&format!("\n\n{};", self.view_cql(ks, v)));
            }
        }
        ret
    }
}

/// `UserType.export_as_string`.
fn type_cql(ks: &str, r: &Row) -> String {
    let names = r.texts("field_names");
    let types = r.texts("field_types");
    let fields: Vec<String> = names.iter().zip(&types).map(|(n, t)| format!("    {} {t}", protect_name(n))).collect();
    format!(
        "CREATE TYPE {}.{} (\n{}\n);",
        protect_name(ks),
        protect_name(&r.text("type_name").unwrap_or_default()),
        fields.join(",\n")
    )
}

/// The replication map as `export_for_schema` writes it.
fn replication(map: &[(String, String)]) -> String {
    let class = map.iter().find(|(k, _)| k == "class").map(|(_, v)| v.as_str()).unwrap_or("");
    let class = class.strip_prefix("org.apache.cassandra.locator.").unwrap_or(class);
    let rest: Vec<&(String, String)> = map.iter().filter(|(k, _)| k != "class").collect();
    match class {
        "SimpleStrategy" => {
            let rf = rest.iter().find(|(k, _)| k == "replication_factor").map(|(_, v)| v.as_str()).unwrap_or("");
            format!("{{'class': 'SimpleStrategy', 'replication_factor': '{rf}'}}")
        }
        "NetworkTopologyStrategy" => {
            let mut dcs: Vec<&&(String, String)> = rest.iter().filter(|(k, _)| k != "replication_factor").collect();
            dcs.sort();
            let mut s = "{'class': 'NetworkTopologyStrategy'".to_string();
            for (dc, rf) in dcs.iter().map(|x| (&x.0, &x.1)) {
                s.push_str(&format!(", '{dc}': '{rf}'"));
            }
            s + "}"
        }
        "LocalStrategy" => "{'class': 'LocalStrategy'}".into(),
        other => {
            let mut parts: Vec<String> = rest.iter().map(|(k, v)| format!("'{k}': '{v}'")).collect();
            parts.push(format!("'class': '{other}'"));
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// `KeyspaceMetadata.export_as_string`, plus the newline `print_recreate_keyspace` writes.
pub async fn keyspace(shell: &Shell, ks: &str, newline: bool) -> String {
    let Some(krows) = rows(shell, format!("SELECT * FROM system_schema.keyspaces WHERE keyspace_name = {}", lit(ks))).await else {
        return String::new();
    };
    let Some(k) = each(&krows).into_iter().next() else { return String::new() };
    let mut parts = vec![format!(
        "CREATE KEYSPACE {} WITH replication = {}  AND durable_writes = {};",
        protect_name(ks),
        replication(&k.map("replication").unwrap_or_default()),
        if k.boolean("durable_writes").unwrap_or(true) { "true" } else { "false" }
    )];
    // User types: sorted, each after the types it uses.
    if let Some(trows) = rows(shell, format!("SELECT * FROM system_schema.types WHERE keyspace_name = {}", lit(ks))).await {
        let types = each(&trows);
        let mut left: Vec<(String, &Row)> = types.iter().map(|r| (r.text("type_name").unwrap_or_default(), r)).collect();
        left.sort_by(|a, b| a.0.cmp(&b.0));
        fn resolve(name: &str, left: &mut Vec<(String, &Row)>, out: &mut Vec<String>, ks: &str) {
            let Some(i) = left.iter().position(|(n, _)| n == name) else { return };
            let (_, r) = left.remove(i);
            for t in r.texts("field_types") {
                for word in t.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()) {
                    if left.iter().any(|(n, _)| n == word) {
                        resolve(word, left, out, ks);
                    }
                }
            }
            out.push(type_cql(ks, r));
        }
        let names: Vec<String> = left.iter().map(|(n, _)| n.clone()).collect();
        for n in names {
            resolve(&n, &mut left, &mut parts, ks);
        }
    }
    let s = schema(shell, ks).await;
    let tables = s.tables.as_ref().map(each).unwrap_or_default();
    let names: Vec<String> = tables.iter().filter_map(|t| t.text("table_name")).collect();
    for name in shell.meta.tables(ks, names) {
        if let Some(t) = tables.iter().find(|t| t.text("table_name").as_deref() == Some(&name)) {
            parts.push(s.table_export(shell, ks, t));
        }
    }
    let mut out = parts.join("\n\n");
    if newline {
        out.push('\n');
    }
    out
}

/// A table's export, `None` when there is no such table.
pub async fn table(shell: &Shell, ks: &str, name: &str, newline: bool) -> Option<String> {
    let s = schema(shell, ks).await;
    let t = s.tables.as_ref().map(each).unwrap_or_default().into_iter().find(|r| r.text("table_name").as_deref() == Some(name))?;
    let mut out = s.table_export(shell, ks, &t);
    if newline {
        out.push('\n');
    }
    Some(out)
}

pub async fn index(shell: &Shell, ks: &str, name: &str) -> Option<String> {
    let r = rows(shell, format!("SELECT * FROM system_schema.indexes WHERE keyspace_name = {}", lit(ks))).await?;
    let row = each(&r).into_iter().find(|x| x.text("index_name").as_deref() == Some(name))?;
    Some(format!("{};\n", index_cql(ks, &row)?))
}

pub async fn view(shell: &Shell, ks: &str, name: &str, newline: bool) -> Option<String> {
    let s = schema(shell, ks).await;
    let v = s.views.as_ref().map(each).unwrap_or_default().into_iter().find(|r| r.text("view_name").as_deref() == Some(name))?;
    let mut out = format!("{};", s.view_cql(ks, &v));
    if newline {
        out.push('\n');
    }
    Some(out)
}

pub async fn user_type(shell: &Shell, ks: &str, name: &str) -> Option<String> {
    let r = rows(shell, format!("SELECT * FROM system_schema.types WHERE keyspace_name = {}", lit(ks))).await?;
    let row = each(&r).into_iter().find(|x| x.text("type_name").as_deref() == Some(name))?;
    Some(format!("{}\n", type_cql(ks, &row)))
}

/// The node's tokens, in ring order.
pub async fn local_tokens(shell: &Shell) -> Vec<String> {
    let Some(r) = rows(shell, "SELECT tokens FROM system.local WHERE key = 'local'".into()).await else { return vec![] };
    let mut toks: Vec<i64> = each(&r).first().map(|row| row.texts("tokens")).unwrap_or_default().iter().filter_map(|t| t.parse().ok()).collect();
    toks.sort();
    toks.iter().map(|t| t.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replication_maps() {
        let m = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>();
        assert_eq!(
            replication(&m(&[("class", "org.apache.cassandra.locator.NetworkTopologyStrategy"), ("dc2", "3"), ("datacenter1", "1")])),
            "{'class': 'NetworkTopologyStrategy', 'datacenter1': '1', 'dc2': '3'}"
        );
        assert_eq!(
            replication(&m(&[("class", "org.apache.cassandra.locator.SimpleStrategy"), ("replication_factor", "2")])),
            "{'class': 'SimpleStrategy', 'replication_factor': '2'}"
        );
        assert_eq!(replication(&m(&[("class", "org.apache.cassandra.locator.LocalStrategy")])), "{'class': 'LocalStrategy'}");
        assert_eq!(replication(&m(&[("class", "org.apache.cassandra.locator.EverywhereStrategy")])), "{'class': 'EverywhereStrategy'}");
    }

    #[test]
    fn tables() {
        let shape = TableShape {
            columns: vec![
                Column { name: "k".into(), ty: "int".into(), is_static: false, reversed: false },
                Column { name: "c".into(), ty: "text".into(), is_static: false, reversed: true },
                Column { name: "s".into(), ty: "int".into(), is_static: true, reversed: false },
            ],
            partition: vec!["k".into()],
            clustering: vec!["c".into()],
            compact: false,
        };
        assert_eq!(
            table_cql("ks", "T", &shape, vec!["comment = ''".into()]),
            "CREATE TABLE ks.\"T\" (\n    k int,\n    c text,\n    s int static,\n    PRIMARY KEY (k, c)\n) WITH CLUSTERING ORDER BY (c DESC)\n    AND comment = ''"
        );
    }
}
