//! Cassandra's `toCqlString` renderers, ported from `KeyspaceMetadata`, `TableMetadata` (and
//! `CompactTableMetadata`), `ColumnMetadata.appendCqlTo`, `TableParams.appendCqlTo`,
//! `IndexMetadata.appendCqlTo`, `ViewMetadata.appendCqlTo`, `UserType.toCqlString`,
//! `UDFunction.toCqlString` and `UDAggregate.toCqlString`.

use std::collections::BTreeMap;

use nosql_types::{CqlType, Dialect, UdtDef};

use super::Options;
use super::builder::{CqlBuilder, quote};
use crate::schema::{
    AggregateMeta, Column, ColumnKind, FunctionMeta, IndexMeta, Keyspace, Table, TableParams,
};

const D: Dialect = Dialect::Cassandra;

/// `ks.t` with each part quoted if needed (`TableMetadata.toString`).
pub fn qualified(ks: &str, name: &str) -> String {
    format!("{}.{}", quote(ks, D), quote(name, D))
}

/// `KeyspaceMetadata.toCqlString`.
pub fn keyspace(k: &Keyspace) -> String {
    let mut b = CqlBuilder::new(D);
    if super::is_virtual(k) {
        b.append("/*")
            .new_line()
            .append("Warning: Keyspace ")
            .ident(&k.name)
            .append(" is a virtual keyspace and cannot be recreated with CQL.")
            .new_line()
            .append("Structure, for reference:")
            .new_line()
            .append("VIRTUAL KEYSPACE ")
            .ident(&k.name)
            .append(";")
            .new_line()
            .append("*/");
        return b.finish();
    }
    b.append("CREATE KEYSPACE ")
        .ident(&k.name)
        .append(" WITH replication = ");
    replication(&mut b, k);
    b.append("  AND durable_writes = ")
        .append_display(k.durable_writes)
        .append(";");
    b.finish()
}

/// `ReplicationParams.appendCqlTo`: the class without the `org.apache.cassandra.locator`
/// package.
fn replication(b: &mut CqlBuilder, k: &Keyspace) {
    let class = k
        .replication
        .class
        .strip_prefix("org.apache.cassandra.locator.")
        .unwrap_or(&k.replication.class);
    b.append("{'class': ").quoted(class);
    for (key, v) in &k.replication.options {
        b.append(", ").quoted(key).append(": ").quoted(v);
    }
    b.append("}");
}

/// `UserType.toCqlString` (ScyllaDB prints types the same way).
pub fn user_type(d: Dialect, t: &UdtDef) -> String {
    let mut b = CqlBuilder::new(d);
    b.append("CREATE TYPE ")
        .ident(&t.keyspace)
        .append(".")
        .ident(&t.name)
        .append(" (")
        .new_line()
        .increase_indent();
    for (i, (f, ty)) in t.fields.iter().enumerate() {
        if i > 0 {
            b.append(",").new_line();
        }
        b.ident(f).append(" ").append(type_name(d, ty));
    }
    b.new_line().decrease_indent().append(");");
    b.finish()
}

/// A type as `DESCRIBE` prints it.
pub fn type_name(d: Dialect, ty: &CqlType) -> String {
    nosql_types::cql_name_in(ty, d)
}

/// Java's `Double.toString`.
pub fn java_double(v: f64) -> String {
    let a = v.abs();
    if v == 0.0 || (1e-3..1e7).contains(&a) {
        let s = format!("{v:?}");
        if s.contains('.') || s.contains('e') {
            s
        } else {
            format!("{s}.0")
        }
    } else {
        let s = format!("{v:e}");
        let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
        let m = if m.contains('.') {
            m.to_string()
        } else {
            format!("{m}.0")
        };
        format!("{m}E{e}")
    }
}

/// Whether a table is a legacy (`COMPACT STORAGE`) table: `!TableMetadata.isCQLTable()`.
pub fn is_compact(t: &Table) -> bool {
    t.flags.super_ || t.flags.dense || !t.flags.compound
}

/// `isStaticCompactTable`: compact, not dense, not compound.
fn is_static_compact(t: &Table) -> bool {
    !t.flags.super_ && !t.flags.dense && !t.flags.compound
}

/// `Columns` order: simple columns, then complex (multi-cell) ones, each by name bytes.
fn columns_order<'t>(cols: impl Iterator<Item = &'t Column>) -> Vec<&'t Column> {
    let mut v: Vec<&Column> = cols.collect();
    v.sort_by(|a, b| {
        a.ty.is_multicell()
            .cmp(&b.ty.is_multicell())
            .then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
    });
    v
}

/// `allColumnsInCreateOrder` (with `CompactTableMetadata`'s hidden columns removed): the column
/// and the kind it prints as.
fn create_order(t: &Table) -> Vec<(&Column, ColumnKind)> {
    let mut out: Vec<(&Column, ColumnKind)> =
        t.partition_key_columns().map(|c| (c, c.kind)).collect();
    let compact = is_compact(t);
    if compact && is_static_compact(t) {
        // The clustering column and the value column are internal (and not in the model); the
        // defined columns, static in the CQL metadata, print as regular.
        for c in columns_order(t.columns.iter().filter(|c| !c.is_primary_key())) {
            out.push((c, ColumnKind::Regular));
        }
        return out;
    }
    out.extend(t.clustering_columns().map(|c| (c, c.kind)));
    let empty_value = compact
        && t.flags.dense
        && t.columns
            .iter()
            .any(|c| c.kind == ColumnKind::Regular && c.ty == CqlType::Empty);
    if !empty_value {
        out.extend(
            columns_order(t.columns.iter().filter(|c| c.kind == ColumnKind::Static))
                .into_iter()
                .map(|c| (c, c.kind)),
        );
        out.extend(
            columns_order(t.columns.iter().filter(|c| c.kind == ColumnKind::Regular))
                .into_iter()
                .map(|c| (c, c.kind)),
        );
    }
    out
}

/// `ColumnMetadata.appendCqlTo`.
fn column_def(b: &mut CqlBuilder, name: &str, ty: &CqlType, kind: ColumnKind) {
    b.ident(name).append(" ").append(type_name(D, ty));
    if kind == ColumnKind::Static {
        b.append(" static");
    }
}

/// `TableMetadata.toCqlString(withInternals, withInternals, ifNotExists = false)`.
pub fn table(k: &Keyspace, t: &Table, opts: Options) -> String {
    let dropped = opts.internals;
    let mut b = CqlBuilder::new(D);
    let compact = is_compact(t);
    if compact {
        b.append("/*")
            .new_line()
            .append("Warning: Table ")
            .append(qualified(&k.name, &t.name))
            .append(" omitted because it has constructs not compatible with CQL (was created via legacy API).")
            .new_line()
            .append("Approximate structure, for reference:")
            .new_line()
            .append("(this should not be used to reproduce this schema)")
            .new_line()
            .new_line();
    }
    let virtual_ = t.is_virtual() && super::is_virtual(k);
    if virtual_ {
        b.append(format!(
            "/*\nWarning: Table {} is a virtual table and cannot be recreated with CQL.\nStructure, for reference:\n",
            qualified(&k.name, &t.name)
        ));
    }
    b.append(if virtual_ { "VIRTUAL" } else { "CREATE" })
        .append(" TABLE ")
        .append(qualified(&k.name, &t.name))
        .append(" (")
        .new_line()
        .increase_indent();
    let hidden_ck = crate::schema::persist::static_compact_clustering(t);
    let single = t.partition_key.len() == 1 && t.clustering.is_empty() && hidden_ck.is_none();
    let cols = create_order(t);
    let cols: Vec<_> = cols
        .into_iter()
        .filter(|(c, _)| !(dropped && t.dropped.contains_key(&c.name)))
        .collect();
    let n = cols.len();
    for (i, (c, kind)) in cols.iter().enumerate() {
        column_def(&mut b, &c.name, &c.ty, *kind);
        if let Some(m) = t.masks.get(&c.name) {
            b.append(" MASKED WITH ")
                .append(crate::masking::describe(m, k, D));
        }
        if single && c.kind == ColumnKind::PartitionKey {
            b.append(" PRIMARY KEY");
        }
        if !single || (dropped && !t.dropped.is_empty()) || i + 1 < n {
            b.append(",");
        }
        b.new_line();
    }
    if dropped {
        let n = t.dropped.len();
        for (i, (name, dc)) in t.dropped.iter().enumerate() {
            column_def(&mut b, name, &dc.ty, dc.kind);
            if !single || i + 1 < n {
                b.append(",");
            }
            b.new_line();
        }
    }
    if !single {
        primary_key(&mut b, t);
    }
    b.decrease_indent().append(")");
    b.append(" WITH ").increase_indent();
    if compact {
        b.append("COMPACT STORAGE").new_line().append("AND ");
    }
    table_options(
        &mut b,
        t,
        opts.internals,
        false,
        virtual_,
        hidden_ck.as_deref(),
    );
    b.decrease_indent();
    if virtual_ {
        b.new_line().append("*/");
    }
    if dropped {
        for (name, dc) in &t.dropped {
            b.new_line()
                .append("ALTER TABLE ")
                .append(qualified(&k.name, &t.name))
                .append(" DROP ")
                .ident(name)
                .append(" USING TIMESTAMP ")
                .append_display(dc.dropped_at / 1000 * 1000)
                .append(";");
            if let Some(c) = t.column(name) {
                b.new_line()
                    .append("ALTER TABLE ")
                    .append(qualified(&k.name, &t.name))
                    .append(" ADD ");
                column_def(&mut b, &c.name, &c.ty, c.kind);
                b.append(";");
            }
        }
    }
    if compact {
        b.new_line().append("*/");
    }
    b.finish()
}

/// `appendPrimaryKey`.
fn primary_key(b: &mut CqlBuilder, t: &Table) {
    let pk: Vec<&Column> = t.partition_key_columns().collect();
    let ck: Vec<&Column> = if is_compact(t) && is_static_compact(t) {
        Vec::new()
    } else {
        t.clustering_columns().collect()
    };
    b.append("PRIMARY KEY (");
    if pk.len() > 1 {
        b.append("(")
            .join(pk.iter(), ", ", |b, c| {
                b.ident(&c.name);
            })
            .append(")");
    } else if let Some(c) = pk.first() {
        b.ident(&c.name);
    }
    if !ck.is_empty() {
        b.append(", ").join(ck.iter(), ", ", |b, c| {
            b.ident(&c.name);
        });
    }
    b.append(")").new_line();
}

/// `appendTableOptions`: `ID`, `CLUSTERING ORDER BY`, then the parameters.
fn table_options(
    b: &mut CqlBuilder,
    t: &Table,
    internals: bool,
    view: bool,
    virtual_: bool,
    hidden_ck: Option<&str>,
) {
    if internals {
        b.append("ID = ")
            .append(crate::uuid::format(&t.id))
            .new_line()
            .append("AND ");
    }
    if let Some(ck) = hidden_ck {
        b.append("CLUSTERING ORDER BY (")
            .ident(ck)
            .append(" ASC)")
            .new_line()
            .append("AND ");
    } else if !t.clustering.is_empty() {
        b.append("CLUSTERING ORDER BY (")
            .join(t.clustering_columns(), ", ", |b, c| {
                b.ident(&c.name)
                    .append(if c.reversed { " DESC" } else { " ASC" });
            })
            .append(")")
            .new_line()
            .append("AND ");
    }
    if virtual_ {
        b.append("comment = ").quoted(&t.params.comment);
    } else {
        params(b, &t.params, view);
    }
    b.append(";");
}

/// `TableParams.appendCqlTo`.
fn params(b: &mut CqlBuilder, p: &TableParams, view: bool) {
    b.append("additional_write_policy = ")
        .quoted(&p.additional_write_policy)
        .new_line()
        .append("AND allow_auto_snapshot = ")
        .append_display(p.allow_auto_snapshot.unwrap_or(true))
        .new_line()
        .append("AND bloom_filter_fp_chance = ")
        .append(java_double(p.bloom_filter_fp_chance))
        .new_line()
        .append("AND caching = ")
        .map(&p.caching, true)
        .new_line()
        .append("AND cdc = ")
        .append_display(p.cdc.unwrap_or(false))
        .new_line()
        .append("AND comment = ")
        .quoted(&p.comment)
        .new_line()
        .append("AND compaction = ")
        .map(&p.compaction, true)
        .new_line()
        .append("AND compression = ")
        .map(&p.compression, true)
        .new_line()
        .append("AND memtable = ")
        .quoted(p.memtable.as_deref().unwrap_or("default"))
        .new_line()
        .append("AND crc_check_chance = ")
        .append(java_double(p.crc_check_chance))
        .new_line();
    if !view {
        b.append("AND default_time_to_live = ")
            .append_display(p.default_time_to_live)
            .new_line();
    }
    let ext: BTreeMap<String, String> = p
        .extensions
        .iter()
        .map(|(k, v)| (k.clone(), format!("0x{}", hex(v))))
        .collect();
    b.append("AND extensions = ")
        .map(&ext, false)
        .new_line()
        .append("AND gc_grace_seconds = ")
        .append_display(p.gc_grace_seconds)
        .new_line()
        .append("AND incremental_backups = ")
        .append_display(p.incremental_backups.unwrap_or(true))
        .new_line()
        .append("AND max_index_interval = ")
        .append_display(p.max_index_interval)
        .new_line()
        .append("AND memtable_flush_period_in_ms = ")
        .append_display(p.memtable_flush_period_in_ms)
        .new_line()
        .append("AND min_index_interval = ")
        .append_display(p.min_index_interval)
        .new_line()
        .append("AND read_repair = ")
        .quoted(&p.read_repair)
        .new_line()
        .append("AND speculative_retry = ")
        .quoted(&p.speculative_retry);
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `IndexMetadata.appendCqlTo`.
pub fn index(t: &Table, i: &IndexMeta) -> String {
    let mut b = CqlBuilder::new(D);
    let target = i.options.get("target").map(String::as_str).unwrap_or("");
    if i.kind == "CUSTOM" {
        let mut rest = i.options.clone();
        rest.remove("target");
        let class = rest.remove("class_name").unwrap_or_default();
        b.append("CREATE CUSTOM INDEX ")
            .ident(&i.name)
            .append(" ON ")
            .append(qualified(&t.keyspace, &t.name))
            .append(" (")
            .append(target)
            .append(") USING ")
            .quoted(&class);
        if !rest.is_empty() {
            b.append(" WITH OPTIONS = ").map(&rest, true);
        }
    } else {
        b.append("CREATE INDEX ")
            .ident(&i.name)
            .append(" ON ")
            .append(qualified(&t.keyspace, &t.name))
            .append(" (")
            .append(target)
            .append(")");
    }
    b.append(";");
    b.finish()
}

/// `ViewMetadata.appendCqlTo`.
pub fn view(k: &Keyspace, v: &Table, opts: Options) -> String {
    let Some(meta) = &v.view else {
        return String::new();
    };
    let mut b = CqlBuilder::new(D);
    b.append("CREATE MATERIALIZED VIEW ")
        .append(qualified(&k.name, &v.name))
        .append(" AS")
        .new_line()
        .increase_indent()
        .append("SELECT ");
    if meta.include_all_columns {
        b.append("*");
    } else {
        b.join(select_order(v), ", ", |b, c| {
            b.ident(&c.name);
        });
    }
    b.new_line()
        .append("FROM ")
        .ident(&k.name)
        .append(".")
        .ident(&meta.base_table_name)
        .new_line()
        .append("WHERE ")
        .append(&meta.where_clause)
        .new_line();
    primary_key(&mut b, v);
    b.decrease_indent().append(" WITH ").increase_indent();
    table_options(&mut b, v, opts.internals, true, false, None);
    b.finish()
}

/// `allColumnsInSelectOrder`: key columns, then the others by name.
pub fn select_order(t: &Table) -> Vec<&Column> {
    let mut v: Vec<&Column> = t.partition_key_columns().collect();
    v.extend(t.clustering_columns());
    let mut rest: Vec<&Column> = t.columns.iter().filter(|c| !c.is_primary_key()).collect();
    rest.sort_by(|a, b| {
        (a.kind != ColumnKind::Static)
            .cmp(&(b.kind != ColumnKind::Static))
            .then_with(|| a.name.as_bytes().cmp(b.name.as_bytes()))
    });
    v.extend(rest);
    v
}

/// A function name `ks.f` (`FunctionName.appendCqlTo`).
fn function_name(ks: &str, name: &str) -> String {
    format!("{}.{}", quote(ks, D), quote(name, D))
}

/// `UDFunction.toCqlString`.
pub fn function(k: &Keyspace, f: &FunctionMeta) -> String {
    let mut b = CqlBuilder::new(D);
    b.append("CREATE FUNCTION ")
        .append(function_name(&k.name, &f.name))
        .append("(");
    for (i, (n, t)) in f.argument_names.iter().zip(&f.argument_types).enumerate() {
        if i > 0 {
            b.append(", ");
        }
        b.ident(n).append(" ").append(function_type(t));
    }
    b.append(")")
        .new_line()
        .increase_indent()
        .append(if f.called_on_null_input {
            "CALLED"
        } else {
            "RETURNS NULL"
        })
        .append(" ON NULL INPUT")
        .new_line()
        .append("RETURNS ")
        .append(function_type(&f.return_type))
        .new_line()
        .append("LANGUAGE ")
        .append(&f.language)
        .new_line()
        .append("AS $$")
        .append(&f.body)
        .append("$$;");
    b.finish()
}

/// `AbstractFunction.toCqlString(type)`: tuples print without `frozen<>`.
pub(crate) fn function_type(t: &str) -> String {
    match t.strip_prefix("frozen<tuple<") {
        Some(rest) => format!("tuple<{}", rest.strip_suffix('>').unwrap_or(rest)),
        None => t.to_string(),
    }
}

/// `UDAggregate.toCqlString`.
pub fn aggregate(k: &Keyspace, a: &AggregateMeta) -> String {
    let mut b = CqlBuilder::new(D);
    b.append("CREATE AGGREGATE ")
        .append(function_name(&k.name, &a.name))
        .append("(")
        .join(a.argument_types.iter(), ", ", |b, t| {
            b.append(function_type(t));
        })
        .append(")")
        .new_line()
        .increase_indent()
        .append("SFUNC ")
        .ident(&a.state_func)
        .new_line()
        .append("STYPE ")
        .append(function_type(&a.state_type));
    if let Some(f) = &a.final_func {
        b.new_line().append("FINALFUNC ").ident(f);
    }
    if let Some(i) = &a.initcond {
        b.new_line().append("INITCOND ").append(i);
    }
    b.append(";");
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_doubles() {
        assert_eq!(java_double(1.0), "1.0");
        assert_eq!(java_double(0.01), "0.01");
        assert_eq!(java_double(0.5), "0.5");
        assert_eq!(java_double(0.0001), "1.0E-4");
        assert_eq!(java_double(1.5e-5), "1.5E-5");
        assert_eq!(java_double(12345678.0), "1.2345678E7");
        assert_eq!(java_double(0.0), "0.0");
    }

    #[test]
    fn function_types() {
        assert_eq!(
            function_type("frozen<tuple<int, text>>"),
            "tuple<int, text>"
        );
        assert_eq!(function_type("frozen<list<int>>"), "frozen<list<int>>");
    }
}
