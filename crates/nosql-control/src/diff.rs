//! `GET /databases/{id}/diff`: a branch's schema difference against its parent, as the CQL
//! that turns the parent's schema into the branch's (`CREATE` / `ALTER` / `DROP`).
//!
//! Both schemas are compared structurally (keyspaces, user types, tables with their columns,
//! primary keys and options, indexes, views, functions, aggregates). Statements creating
//! objects are the branch's own `DESCRIBE` output, so they read exactly as `DESCRIBE` prints
//! them; when the engine can't describe (its DESCRIBE subsystem is optional), they are
//! generated from the schema instead.
//!
//! Changes CQL can't express in place (a column's type, a primary key) are written as a
//! comment, or as `DROP` + `CREATE` for primary keys and views, so nothing destructive is
//! implied silently.

use crate::control::{Control, ControlError, Result};
use nosql_engine as eng;
use nosql_engine::schema::{ColumnKind, Keyspace, Schema, Table};
use nosql_types::{Dialect, quote_identifier};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

/// `DESCRIBE` rows (`keyspace_name, type, name, create_statement`) joined as cqlsh prints them.
pub fn describe_text(rows: &eng::Rows) -> String {
    let idx = rows.metadata.columns.iter().position(|c| c.name == "create_statement").unwrap_or(rows.metadata.columns.len().saturating_sub(1));
    let mut out = String::new();
    for row in &rows.rows {
        if let Some(Some(b)) = row.get(idx) {
            out.push_str(&String::from_utf8_lossy(b));
            out.push_str("\n\n");
        }
    }
    out
}

/// One described element: keyspace, kind (`table`, `index`, `materialized_view`, …), name.
type Key = (String, String, String);

/// `DESCRIBE SCHEMA` of one side, by element, in output order.
#[derive(Clone, Debug, Default)]
pub struct Described {
    pub order: Vec<Key>,
    pub text: HashMap<Key, String>,
}

impl Described {
    pub fn from_rows(rows: &eng::Rows) -> Self {
        let col = |n: &str| rows.metadata.columns.iter().position(|c| c.name == n);
        let (Some(ks), Some(kind), Some(name), Some(stmt)) = (col("keyspace_name"), col("type"), col("name"), col("create_statement")) else {
            return Described::default();
        };
        let text = |row: &[Option<bytes::Bytes>], i: usize| row.get(i).and_then(|c| c.as_ref()).map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default();
        let mut d = Described::default();
        for row in &rows.rows {
            let kind = match text(row, kind).to_ascii_lowercase().replace(' ', "_").as_str() {
                "view" => "materialized_view".to_string(),
                k => k.to_string(),
            };
            let key = (unquote(&text(row, ks)), kind, unquote(&text(row, name)));
            d.order.push(key.clone());
            d.text.insert(key, text(row, stmt));
        }
        d
    }

    /// From the engine's `DESCRIBE SCHEMA` elements.
    pub fn from_elements(elements: &[eng::describe::Element]) -> Self {
        use eng::describe::Kind;
        let mut d = Described::default();
        for e in elements {
            let (Some(ks), Some(stmt)) = (&e.keyspace, &e.statement) else { continue };
            let kind = match e.kind {
                Kind::Keyspace => "keyspace",
                Kind::Type => "type",
                Kind::Function => "function",
                Kind::Aggregate => "aggregate",
                Kind::Table => "table",
                Kind::Index => "index",
                Kind::View => "materialized_view",
                _ => continue,
            };
            let key = (ks.clone(), kind.to_string(), format!("{}{}", e.name, e.signature.as_deref().unwrap_or("")));
            d.order.push(key.clone());
            d.text.insert(key, stmt.clone());
        }
        d
    }

    fn get(&self, ks: &str, kind: &str, name: &str) -> Option<&String> {
        self.text.get(&(ks.to_string(), kind.to_string(), name.to_string()))
    }
}

/// `"Name"` → `Name` (doubled quotes undone); anything else as is.
fn unquote(s: &str) -> String {
    match s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => s.to_string(),
    }
}

fn stmt(s: &str) -> String {
    let s = s.trim();
    if s.ends_with(';') { s.to_string() } else { format!("{s};") }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn map_lit(m: &BTreeMap<String, String>) -> String {
    let parts: Vec<String> = m.iter().map(|(k, v)| format!("{}: {}", lit(k), lit(v))).collect();
    format!("{{{}}}", parts.join(", "))
}

struct Q(Dialect);

impl Q {
    fn id(&self, s: &str) -> String {
        quote_identifier(s, self.0).into_owned()
    }

    fn qual(&self, ks: &str, name: &str) -> String {
        format!("{}.{}", self.id(ks), self.id(name))
    }
}

/// Index backing views (ScyllaDB's `<index>_index`) and CDC log tables come with their
/// index or table option and aren't diffed on their own.
fn is_internal(ks: &Keyspace, t: &Table) -> bool {
    if t.name.ends_with("_scylla_cdc_log") {
        return true;
    }
    t.is_view() && ks.tables.values().any(|b| b.indexes.iter().any(|i| format!("{}_index", i.name) == t.name))
}

fn tables(ks: &Keyspace) -> impl Iterator<Item = &Table> {
    ks.tables.values().map(|t| t.as_ref()).filter(|t| !t.is_view() && !is_internal(ks, t))
}

fn views(ks: &Keyspace) -> impl Iterator<Item = &Table> {
    ks.tables.values().map(|t| t.as_ref()).filter(|t| t.is_view() && !is_internal(ks, t))
}

fn col_type(d: Dialect, t: &nosql_types::CqlType) -> String {
    nosql_types::cql_name_in(t, d)
}

// ---- statements generated from the schema (when DESCRIBE isn't available) ----------------

fn gen_keyspace(q: &Q, k: &Keyspace) -> String {
    format!("CREATE KEYSPACE {} WITH replication = {} AND durable_writes = {};", q.id(&k.name), map_lit(&k.replication.as_map()), k.durable_writes)
}

fn gen_type(q: &Q, ks: &str, t: &nosql_types::UdtDef) -> String {
    let fields: Vec<String> = t.fields.iter().map(|(n, ty)| format!("    {} {}", q.id(n), col_type(q.0, ty))).collect();
    format!("CREATE TYPE {} (\n{}\n);", q.qual(ks, &t.name), fields.join(",\n"))
}

fn primary_key(q: &Q, t: &Table) -> String {
    let pk: Vec<String> = t.partition_key_columns().map(|c| q.id(&c.name)).collect();
    let ck: Vec<String> = t.clustering_columns().map(|c| q.id(&c.name)).collect();
    let pk = if pk.len() == 1 { pk[0].clone() } else { format!("({})", pk.join(", ")) };
    if ck.is_empty() { format!("PRIMARY KEY ({pk})") } else { format!("PRIMARY KEY ({pk}, {})", ck.join(", ")) }
}

fn clustering_order(q: &Q, t: &Table) -> Option<String> {
    let ck: Vec<String> = t.clustering_columns().map(|c| format!("{} {}", q.id(&c.name), if c.reversed { "DESC" } else { "ASC" })).collect();
    (!ck.is_empty() && t.clustering_columns().any(|c| c.reversed)).then(|| format!("CLUSTERING ORDER BY ({})", ck.join(", ")))
}

fn gen_table(q: &Q, t: &Table) -> String {
    let mut s = format!("CREATE TABLE {} (\n", q.qual(&t.keyspace, &t.name));
    for c in &t.columns {
        let _ = writeln!(s, "    {} {}{},", q.id(&c.name), col_type(q.0, &c.ty), if c.kind == ColumnKind::Static { " static" } else { "" });
    }
    let _ = write!(s, "    {}\n)", primary_key(q, t));
    if let Some(o) = clustering_order(q, t) {
        let _ = write!(s, " WITH {o}");
    }
    s.push(';');
    s
}

fn gen_index(q: &Q, t: &Table, i: &eng::schema::IndexMeta) -> String {
    let target = i.options.get("target").cloned().unwrap_or_default();
    match (i.kind.as_str(), i.options.get("class_name")) {
        ("CUSTOM", Some(class)) => format!("CREATE CUSTOM INDEX {} ON {} ({target}) USING {};", q.id(&i.name), q.qual(&t.keyspace, &t.name), lit(class)),
        _ => format!("CREATE INDEX {} ON {} ({target});", q.id(&i.name), q.qual(&t.keyspace, &t.name)),
    }
}

fn gen_view(q: &Q, v: &Table) -> String {
    let Some(meta) = &v.view else { return String::new() };
    let cols: Vec<String> = if meta.include_all_columns { vec!["*".into()] } else { v.columns.iter().map(|c| q.id(&c.name)).collect() };
    let mut s = format!(
        "CREATE MATERIALIZED VIEW {} AS\n    SELECT {}\n    FROM {}\n    WHERE {}\n    {}",
        q.qual(&v.keyspace, &v.name),
        cols.join(", "),
        q.qual(&v.keyspace, &meta.base_table_name),
        meta.where_clause,
        primary_key(q, v)
    );
    if let Some(o) = clustering_order(q, v) {
        let _ = write!(s, "\n    WITH {o}");
    }
    s.push(';');
    s
}

// ---- table options -----------------------------------------------------------------------

/// `name = value` of every option that differs.
fn option_changes(a: &eng::schema::params::TableParams, b: &eng::schema::params::TableParams) -> Vec<String> {
    let mut out = Vec::new();
    let mut num = |name: &str, x: String, y: String| {
        if x != y {
            out.push(format!("{name} = {y}"));
        }
    };
    num("bloom_filter_fp_chance", a.bloom_filter_fp_chance.to_string(), b.bloom_filter_fp_chance.to_string());
    num("crc_check_chance", a.crc_check_chance.to_string(), b.crc_check_chance.to_string());
    num("default_time_to_live", a.default_time_to_live.to_string(), b.default_time_to_live.to_string());
    num("gc_grace_seconds", a.gc_grace_seconds.to_string(), b.gc_grace_seconds.to_string());
    num("max_index_interval", a.max_index_interval.to_string(), b.max_index_interval.to_string());
    num("min_index_interval", a.min_index_interval.to_string(), b.min_index_interval.to_string());
    num("memtable_flush_period_in_ms", a.memtable_flush_period_in_ms.to_string(), b.memtable_flush_period_in_ms.to_string());
    if let (Some(x), Some(y)) = (a.paxos_grace_seconds, b.paxos_grace_seconds)
        && x != y
    {
        out.push(format!("paxos_grace_seconds = {y}"));
    }
    let mut text = |name: &str, x: &str, y: &str| {
        if x != y {
            out.push(format!("{name} = {}", lit(y)));
        }
    };
    text("comment", &a.comment, &b.comment);
    text("speculative_retry", &a.speculative_retry, &b.speculative_retry);
    if !a.additional_write_policy.is_empty() || !b.additional_write_policy.is_empty() {
        text("additional_write_policy", &a.additional_write_policy, &b.additional_write_policy);
    }
    if !a.read_repair.is_empty() || !b.read_repair.is_empty() {
        text("read_repair", &a.read_repair, &b.read_repair);
    }
    let mut map = |name: &str, x: &BTreeMap<String, String>, y: &BTreeMap<String, String>| {
        if x != y && !y.is_empty() {
            out.push(format!("{name} = {}", map_lit(y)));
        }
    };
    map("caching", &a.caching, &b.caching);
    map("compaction", &a.compaction, &b.compaction);
    map("compression", &a.compression, &b.compression);
    map("tombstone_gc", &a.tombstone_gc, &b.tombstone_gc);
    map("cdc", &a.scylla_cdc, &b.scylla_cdc);
    map("per_partition_rate_limit", &a.per_partition_rate_limit, &b.per_partition_rate_limit);
    out
}

fn same_primary_key(a: &Table, b: &Table) -> bool {
    let key = |t: &Table| -> Vec<(String, ColumnKind, String, bool)> {
        t.columns.iter().filter(|c| c.is_primary_key()).map(|c| (c.name.clone(), c.kind, format!("{:?}", c.ty), c.reversed)).collect()
    };
    let mut x = key(a);
    let mut y = key(b);
    x.sort_by(|p, q| p.0.cmp(&q.0));
    y.sort_by(|p, q| p.0.cmp(&q.0));
    x == y && a.partition_key_columns().map(|c| &c.name).eq(b.partition_key_columns().map(|c| &c.name)) && a.clustering_columns().map(|c| &c.name).eq(b.clustering_columns().map(|c| &c.name))
}

/// The CQL turning `parent` into `branch`. `desc` is the branch's `DESCRIBE SCHEMA` (empty
/// when unavailable). Statements in dependency order: creations, alterations, then drops.
pub fn schema_diff(parent: &Schema, branch: &Schema, desc: &Described, d: Dialect) -> Vec<String> {
    let q = Q(d);
    let user = |s: &Schema| -> BTreeMap<String, std::sync::Arc<Keyspace>> { s.keyspaces.iter().filter(|(_, k)| !k.is_builtin()).map(|(n, k)| (n.clone(), k.clone())).collect() };
    let (pk, bk) = (user(parent), user(branch));
    let mut o = Out::default();
    let described = |ks: &str, kind: &str, name: &str| desc.get(ks, kind, name).map(|s| stmt(s));

    for (name, b) in &bk {
        let Some(p) = pk.get(name) else {
            // A new keyspace: everything in it.
            let mine: Vec<&Key> = desc.order.iter().filter(|k| k.0 == *name).collect();
            if !mine.is_empty() {
                o.creates.extend(mine.into_iter().map(|k| stmt(&desc.text[k])));
            } else {
                o.creates.push(gen_keyspace(&q, b));
                o.creates.extend(b.types.values().map(|t| gen_type(&q, name, t)));
                for t in tables(b) {
                    o.creates.push(gen_table(&q, t));
                    o.creates.extend(t.indexes.iter().map(|i| gen_index(&q, t, i)));
                }
                o.creates.extend(views(b).map(|v| gen_view(&q, v)));
            }
            continue;
        };
        if p.replication.as_map() != b.replication.as_map() || p.durable_writes != b.durable_writes {
            o.alters.push(format!("ALTER KEYSPACE {} WITH replication = {} AND durable_writes = {};", q.id(name), map_lit(&b.replication.as_map()), b.durable_writes));
        }
        diff_keyspace(&q, name, p, b, &described, &mut o);
    }
    for name in pk.keys().filter(|n| !bk.contains_key(*n)) {
        o.drops.push(format!("DROP KEYSPACE {};", q.id(name)));
    }
    o.creates.into_iter().chain(o.alters).chain(o.drops).chain(o.recreates).collect()
}

/// Statements by phase: creations, alterations, drops (dependents first), then what is
/// created again after its drop.
#[derive(Default)]
struct Out {
    creates: Vec<String>,
    alters: Vec<String>,
    drops: Vec<String>,
    recreates: Vec<String>,
}

fn diff_keyspace(q: &Q, ks: &str, p: &Keyspace, b: &Keyspace, described: &dyn Fn(&str, &str, &str) -> Option<String>, o: &mut Out) {
    // User types.
    let mut type_drops = Vec::new();
    for (n, t) in &b.types {
        match p.types.get(n) {
            None => o.creates.push(described(ks, "type", n).unwrap_or_else(|| gen_type(q, ks, t))),
            Some(old) if old.fields != t.fields => {
                let prefix = old.fields.len() <= t.fields.len() && old.fields.iter().zip(&t.fields).all(|(x, y)| x == y);
                if prefix {
                    for (f, ty) in &t.fields[old.fields.len()..] {
                        o.alters.push(format!("ALTER TYPE {} ADD {} {};", q.qual(ks, n), q.id(f), col_type(q.0, ty)));
                    }
                } else {
                    o.alters.push(format!("-- {}: fields changed beyond appending; recreate the type", q.qual(ks, n)));
                }
            }
            _ => {}
        }
    }
    for n in p.types.keys().filter(|n| !b.types.contains_key(*n)) {
        type_drops.push(format!("DROP TYPE {};", q.qual(ks, n)));
    }

    // Tables.
    let ptables: BTreeMap<&str, &Table> = tables(p).map(|t| (t.name.as_str(), t)).collect();
    let btables: BTreeMap<&str, &Table> = tables(b).map(|t| (t.name.as_str(), t)).collect();
    let mut table_drops = Vec::new();
    let mut recreated = BTreeSet::new();
    for (n, t) in &btables {
        let create = || described(ks, "table", n).unwrap_or_else(|| gen_table(q, t));
        match ptables.get(n) {
            None => o.creates.push(create()),
            Some(old) if !same_primary_key(old, t) => {
                table_drops.push(format!("-- {}: the primary key changed", q.qual(ks, n)));
                table_drops.push(format!("DROP TABLE {};", q.qual(ks, n)));
                o.recreates.push(create());
                recreated.insert(n.to_string());
            }
            Some(old) => {
                for c in t.columns.iter().filter(|c| !c.is_primary_key()) {
                    match old.column(&c.name) {
                        None => o.alters.push(format!(
                            "ALTER TABLE {} ADD {} {}{};",
                            q.qual(ks, n),
                            q.id(&c.name),
                            col_type(q.0, &c.ty),
                            if c.kind == ColumnKind::Static { " static" } else { "" }
                        )),
                        Some(prev) if prev.ty != c.ty || prev.kind != c.kind => o.alters.push(format!(
                            "-- {}.{}: changed from {}{} to {}{}; drop and add the column to apply",
                            q.qual(ks, n),
                            q.id(&c.name),
                            col_type(q.0, &prev.ty),
                            if prev.kind == ColumnKind::Static { " static" } else { "" },
                            col_type(q.0, &c.ty),
                            if c.kind == ColumnKind::Static { " static" } else { "" }
                        )),
                        _ => {}
                    }
                }
                for c in old.columns.iter().filter(|c| !c.is_primary_key() && t.column(&c.name).is_none()) {
                    o.alters.push(format!("ALTER TABLE {} DROP {};", q.qual(ks, n), q.id(&c.name)));
                }
                let opts = option_changes(&old.params, &t.params);
                if !opts.is_empty() {
                    o.alters.push(format!("ALTER TABLE {} WITH {};", q.qual(ks, n), opts.join("\n    AND ")));
                }
            }
        }
    }
    for n in ptables.keys().filter(|n| !btables.contains_key(*n)) {
        table_drops.push(format!("DROP TABLE {};", q.qual(ks, n)));
    }

    // Indexes (by name). A recreated table's come back with it.
    let idx = |tabs: &BTreeMap<&str, &Table>| -> BTreeMap<String, (String, String)> {
        let mut m = BTreeMap::new();
        for t in tabs.values() {
            for i in &t.indexes {
                m.insert(i.name.clone(), (t.name.clone(), gen_index(q, t, i)));
            }
        }
        m
    };
    let (pi, bi) = (idx(&ptables), idx(&btables));
    let mut index_drops = Vec::new();
    for (n, (table, generated)) in &bi {
        let text = || described(ks, "index", n).unwrap_or_else(|| generated.clone());
        match pi.get(n) {
            _ if recreated.contains(table) => o.recreates.push(text()),
            None => o.creates.push(text()),
            Some((pt, pg)) if pt != table || pg != generated => {
                if !recreated.contains(pt) {
                    index_drops.push(format!("DROP INDEX {};", q.qual(ks, n)));
                }
                o.recreates.push(text());
            }
            _ => {}
        }
    }
    for (n, (table, _)) in &pi {
        if !bi.contains_key(n) && btables.contains_key(table.as_str()) && !recreated.contains(table) {
            index_drops.push(format!("DROP INDEX {};", q.qual(ks, n)));
        }
    }

    // Views (changed ones are dropped and created again).
    let pv: BTreeMap<&str, &Table> = views(p).map(|t| (t.name.as_str(), t)).collect();
    let bv: BTreeMap<&str, &Table> = views(b).map(|t| (t.name.as_str(), t)).collect();
    let mut view_drops = Vec::new();
    for (n, v) in &bv {
        let create = || described(ks, "materialized_view", n).unwrap_or_else(|| gen_view(q, v));
        let base = v.view.as_ref().map(|m| m.base_table_name.clone()).unwrap_or_default();
        match pv.get(n) {
            None => o.creates.push(create()),
            Some(old) => {
                let shape = |t: &Table| (t.view.as_ref().map(|m| (m.base_table_name.clone(), m.where_clause.clone(), m.include_all_columns)), t.columns.iter().map(|c| (c.name.clone(), c.ty.clone(), c.kind)).collect::<Vec<_>>());
                if shape(old) != shape(v) || recreated.contains(&base) {
                    view_drops.push(format!("DROP MATERIALIZED VIEW {};", q.qual(ks, n)));
                    o.recreates.push(create());
                } else {
                    let opts = option_changes(&old.params, &v.params);
                    if !opts.is_empty() {
                        o.alters.push(format!("ALTER MATERIALIZED VIEW {} WITH {};", q.qual(ks, n), opts.join("\n    AND ")));
                    }
                }
            }
        }
    }
    for n in pv.keys().filter(|n| !bv.contains_key(*n)) {
        view_drops.push(format!("DROP MATERIALIZED VIEW {};", q.qual(ks, n)));
    }

    // Functions and aggregates (by signature; changed ones are replaced).
    let fsig = |name: &str, args: &[String]| format!("{}({})", q.qual(ks, name), args.join(", "));
    let mut fn_drops = Vec::new();
    for (kind, pf, bf) in [
        ("function", p.functions.iter().map(|f| (fsig(&f.name, &f.argument_types), format!("{f:?}"), f.name.clone())).collect::<Vec<_>>(), b.functions.iter().map(|f| (fsig(&f.name, &f.argument_types), format!("{f:?}"), f.name.clone())).collect::<Vec<_>>()),
        ("aggregate", p.aggregates.iter().map(|a| (fsig(&a.name, &a.argument_types), format!("{a:?}"), a.name.clone())).collect(), b.aggregates.iter().map(|a| (fsig(&a.name, &a.argument_types), format!("{a:?}"), a.name.clone())).collect()),
    ] {
        let upper = kind.to_ascii_uppercase();
        for (sig, body, name) in &bf {
            let old = pf.iter().find(|(s, _, _)| s == sig);
            if old.is_some_and(|(_, b2, _)| b2 == body) {
                continue;
            }
            let text = described(ks, kind, &format!("{name}({})", sig.split_once('(').map_or("", |x| x.1).trim_end_matches(')')))
                .or_else(|| described(ks, kind, name))
                .map(|t| if old.is_some() { t.replacen(&format!("CREATE {upper}"), &format!("CREATE OR REPLACE {upper}"), 1) } else { t })
                .unwrap_or_else(|| format!("-- {kind} {sig} {}; DESCRIBE is unavailable to print it", if old.is_some() { "changed" } else { "was added" }));
            o.creates.push(text);
        }
        for (sig, _, _) in pf.iter().filter(|(s, _, _)| !bf.iter().any(|(x, _, _)| x == s)) {
            fn_drops.push(format!("DROP {upper} {sig};"));
        }
    }

    // Drops: dependents first.
    o.drops.extend(view_drops);
    o.drops.extend(index_drops);
    o.drops.extend(table_drops);
    o.drops.extend(fn_drops);
    o.drops.extend(type_drops);
}

impl Control {
    async fn side(&self, d: &crate::store::NosqlDb) -> Result<(std::sync::Arc<Schema>, Described, Dialect)> {
        let lease = self.lease(d).await?;
        let db = lease.db();
        let schema = db.schema();
        let desc = Described::from_elements(&eng::describe::schema(db, &schema, false, eng::describe::Options::default()));
        Ok((schema, desc, db.dialect()))
    }

    /// The schema difference of a branch against its parent.
    pub async fn diff(&self, id: &str) -> Result<Value> {
        let d = self.get(id)?;
        let Some(pid) = d.parent_id.clone() else { return Err(ControlError::Conflict(format!("{} is not a branch", d.name))) };
        let Some(p) = self.store.db(&pid)? else { return Err(ControlError::Conflict(format!("the parent of {} ({pid}) was deleted", d.name))) };
        let (parent, _, _) = self.side(&p).await?;
        let (branch, desc, dialect) = self.side(&d).await?;
        let statements = schema_diff(&parent, &branch, &desc, dialect);
        let text = statements.iter().map(|s| format!("{s}\n")).collect::<Vec<_>>().join("\n");
        Ok(json!({ "parent_id": pid, "diff": text, "statements": statements, "described": !desc.order.is_empty() }))
    }
}
