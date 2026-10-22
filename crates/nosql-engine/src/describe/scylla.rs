//! ScyllaDB's `DESCRIBE` forms, learnt from the captures in `docs/nosql/reference/describe*.md`
//! and live probing (ScyllaDB's source is not consulted):
//!
//! - keyspaces print the full replication class and `tablets = {'enabled': …}`;
//! - tables print every column (`STATIC` upper case) and always a `PRIMARY KEY (…)` line, then
//!   the options with C++ number formatting, the ScyllaDB extensions in name order (`cdc`,
//!   `paxos_grace_seconds`, `per_partition_rate_limit`, `scylla_tags`, `tombstone_gc`) and
//!   `synchronous_updates`, ending in `;\n`; `WITH INTERNALS` adds `ID = …` and the dropped
//!   columns (as columns, then `ALTER TABLE … DROP … USING TIMESTAMP`);
//! - a secondary index prints the options of its view (`<index>_index`); vector indexes print
//!   `CREATE CUSTOM INDEX … USING 'vector_index'` without options or a semicolon;
//! - CDC log tables print as `ALTER TABLE <log> WITH <options>`;
//! - `WITH INTERNALS` on `DESCRIBE SCHEMA` adds roles, role grants, permission grants (with
//!   `CassandraAuthorizer`), service levels and their attachments, each kind sorted by name then
//!   statement; the default superuser and the `driver` service level get `IF NOT EXISTS`.

use std::collections::BTreeMap;

use nosql_types::Dialect;

use super::builder::{CqlBuilder, quote};
use super::{Element, Kind, Options};
use crate::database::Database;
use crate::schema::{
    AggregateMeta, Column, ColumnKind, FunctionMeta, IndexMeta, Keyspace, Table, TableParams,
};

const D: Dialect = Dialect::Scylla;

fn qualified(ks: &str, name: &str) -> String {
    format!("{}.{}", quote(ks, D), quote(name, D))
}

/// A `double` as ScyllaDB prints it (`1`, `0.01`).
fn double(v: f64) -> String {
    format!("{v}")
}

pub fn keyspace(k: &Keyspace) -> String {
    let mut b = CqlBuilder::new(D);
    b.append("CREATE KEYSPACE ")
        .ident(&k.name)
        .append(" WITH replication = {'class': ")
        .quoted(&k.replication.class);
    for (key, v) in &k.replication.options {
        b.append(", ").quoted(key).append(": ");
        match k.replication.racks.get(key) {
            Some(racks) => b.append(crate::schema::replication::rack_list_text(racks)),
            None => b.quoted(v),
        };
    }
    b.append("} AND durable_writes = ")
        .append_display(k.durable_writes)
        .append(" AND tablets = ");
    match k.tablets {
        None => b.append("{'enabled': false}"),
        Some(n) if n > 0 => b.append(format!("{{'enabled': true, 'initial': {n}}}")),
        Some(_) => b.append("{'enabled': true}"),
    };
    b.append(";");
    b.finish()
}

/// What precedes a CDC log table's `CREATE TABLE` (in a comment) when it is described by name.
pub const CDC_LOG_NOTE: &str = "/* Do NOT execute this statement! It's only for informational purposes.\n   A CDC log table is created automatically when creating the base with CDC\n   enabled option or creating the vector index on the base table's vector column.\n\n";

/// The CDC log table of `table`.
pub fn cdc_log_name(table: &str) -> String {
    format!("{table}_scylla_cdc_log")
}

/// Whether `t` is the CDC log of a table of `k` (Vector Search logs too).
pub fn is_cdc_log(k: &Keyspace, t: &Table) -> bool {
    crate::cdc::is_active_log(k, t)
}

/// The index whose internal view is `view` (`<index>_index`).
pub fn index_of_view<'k>(k: &'k Keyspace, view: &str) -> Option<(&'k Table, &'k IndexMeta)> {
    let idx = view.strip_suffix("_index")?;
    let v = k.tables.get(view)?;
    let base = &v.view.as_ref()?.base_table_name;
    let t = k.tables.get(base)?;
    t.indexes.iter().find(|i| i.name == idx).map(|i| (&**t, i))
}

/// The `WITH` items of a table or view (`ID`, `CLUSTERING ORDER BY`, `COMPACT STORAGE`, the
/// options), joined: after `ID` the separator is unindented for tables and views.
fn with_items(
    b: &mut CqlBuilder,
    t: &Table,
    p: &TableParams,
    opts: Options,
    id_sep: &str,
    clustering: bool,
) {
    let mut items: Vec<String> = Vec::new();
    if clustering && !t.clustering.is_empty() {
        let cols: Vec<String> = t
            .clustering_columns()
            .map(|c| {
                format!(
                    "{} {}",
                    quote(&c.name, D),
                    if c.reversed { "DESC" } else { "ASC" }
                )
            })
            .collect();
        items.push(format!("CLUSTERING ORDER BY ({})", cols.join(", ")));
    }
    if clustering && super::cassandra::is_compact(t) {
        items.push("COMPACT STORAGE".into());
    }
    items.extend(options(p));
    if opts.internals {
        b.append("ID = ")
            .append(crate::uuid::format(&t.id))
            .append(id_sep);
    }
    b.append(items.join("\n    AND "));
}

/// The options of a table, view or index view, without a separator before the first.
fn options(p: &TableParams) -> Vec<String> {
    let map = |m: &BTreeMap<String, String>| super::options_map(m, D);
    let mut v = vec![
        format!(
            "bloom_filter_fp_chance = {}",
            double(p.bloom_filter_fp_chance)
        ),
        format!("caching = {}", map(&p.caching)),
        format!("comment = '{}'", p.comment.replace('\'', "''")),
        format!("compaction = {}", map(&p.compaction)),
        format!("compression = {}", map(&p.compression)),
        format!("crc_check_chance = {}", double(p.crc_check_chance)),
        format!("default_time_to_live = {}", p.default_time_to_live),
        format!("gc_grace_seconds = {}", p.gc_grace_seconds),
        format!("max_index_interval = {}", p.max_index_interval),
        format!(
            "memtable_flush_period_in_ms = {}",
            p.memtable_flush_period_in_ms
        ),
        format!("min_index_interval = {}", p.min_index_interval),
        format!("speculative_retry = '{}'", p.speculative_retry),
    ];
    if !p.scylla_cdc.is_empty() {
        v.push(format!("cdc = {}", map(&p.scylla_cdc)));
    }
    if let Some(s) = p.paxos_grace_seconds {
        v.push(format!("paxos_grace_seconds = {s}"));
    }
    if !p.per_partition_rate_limit.is_empty() {
        v.push(format!(
            "per_partition_rate_limit = {}",
            map(&p.per_partition_rate_limit)
        ));
    }
    // Shown once set, even empty (removing the per-row TTL column empties it).
    let tags = p
        .extensions
        .get("scylla_tags")
        .and_then(|b| crate::schema::params::parse_scylla_map_blob(b));
    if let Some(tags) = &tags {
        v.push(format!("scylla_tags = {}", map(tags)));
    }
    let tags = tags.unwrap_or_default();
    let hints = crate::schema::params::shown_tablet_hints(&p.tablets);
    if !hints.is_empty() {
        v.push(format!("tablets = {}", map(&hints)));
    }
    if !p.tombstone_gc.is_empty() {
        v.push(format!("tombstone_gc = {}", map(&p.tombstone_gc)));
    }
    if let Some(sync) = tags.get("system:synchronous_view_updates") {
        v.push(format!("synchronous_updates = {sync}"));
    }
    v
}

fn column_def(name: &str, ty: &nosql_types::CqlType, kind: ColumnKind) -> String {
    column_def_ttl(name, ty, kind, false)
}

/// A column declaration; `ttl` marks the per-row TTL column.
fn column_def_ttl(name: &str, ty: &nosql_types::CqlType, kind: ColumnKind, ttl: bool) -> String {
    let mut s = format!("{} {}", quote(name, D), super::cassandra::type_name(D, ty));
    if ttl {
        s.push_str(" TTL");
    }
    if kind == ColumnKind::Static {
        s.push_str(" STATIC");
    }
    s
}

fn primary_key(t: &Table) -> String {
    let pk: Vec<String> = t
        .partition_key_columns()
        .map(|c| quote(&c.name, D))
        .collect();
    let mut s = String::from("PRIMARY KEY (");
    if pk.len() > 1 {
        s.push_str(&format!("({})", pk.join(", ")));
    } else {
        s.push_str(&pk.join(""));
    }
    for c in t.clustering_columns() {
        s.push_str(", ");
        s.push_str(&quote(&c.name, D));
    }
    s.push(')');
    s
}

/// The options `DESCRIBE` prints: ScyllaDB's large data tables (virtual tables over the
/// SSTables' records) print the system defaults with their comment, not their
/// `system_schema.tables` row.
fn describe_params<'t>(k: &Keyspace, t: &'t Table) -> std::borrow::Cow<'t, TableParams> {
    if k.name == "system"
        && matches!(
            t.name.as_str(),
            "large_cells" | "large_partitions" | "large_rows"
        )
    {
        let mut p = TableParams::defaults(D, true);
        p.comment = t.params.comment.clone();
        return std::borrow::Cow::Owned(p);
    }
    std::borrow::Cow::Borrowed(&t.params)
}

/// Dropped columns in the order ScyllaDB lists them: the iteration order of its
/// `std::unordered_map<sstring, ...>` of dropped columns, filled in name order (the
/// `system_schema.dropped_columns` rows) — see [`unordered_map_order`].
fn dropped_in_order(t: &Table) -> Vec<(&String, &crate::schema::DroppedColumn)> {
    let names: Vec<&str> = t.dropped.keys().map(String::as_str).collect();
    crate::cpp::unordered_map_order(&names)
        .into_iter()
        .filter_map(|n| t.dropped.get_key_value(n))
        .collect()
}

pub fn table(k: &Keyspace, t: &Table, opts: Options) -> String {
    let mut b = CqlBuilder::new(D);
    b.append("CREATE TABLE ")
        .append(qualified(&k.name, &t.name))
        .append(" (")
        .new_line();
    let ttl = crate::rowttl::column(t).map(|c| c.name.as_str());
    for c in &t.columns {
        if opts.internals && t.dropped.contains_key(&c.name) {
            continue;
        }
        b.append("    ")
            .append(column_def_ttl(&c.name, &c.ty, c.kind, ttl == Some(c.name.as_str())))
            .append(",")
            .new_line();
    }
    if let Some(value) = crate::schema::persist::dense_empty_value(t) {
        b.append("    ").ident(&value).append(" empty,").new_line();
    }
    let dropped = dropped_in_order(t);
    if opts.internals {
        // Dropped columns print without `STATIC`.
        for &(name, dc) in &dropped {
            b.append("    ")
                .append(column_def(name, &dc.ty, ColumnKind::Regular))
                .append(",")
                .new_line();
        }
    }
    b.append("    ")
        .append(primary_key(t))
        .new_line()
        .append(") WITH ");
    with_items(&mut b, t, &describe_params(k, t), opts, "\nAND ", true);
    b.append(";").new_line();
    if opts.internals && !t.dropped.is_empty() {
        let mut lines = Vec::new();
        // Every DROP, then the ADDs of re-added columns.
        for &(name, dc) in &dropped {
            lines.push(format!(
                "ALTER TABLE {} DROP {} USING TIMESTAMP {};",
                qualified(&k.name, &t.name),
                quote(name, D),
                dc.dropped_at
            ));
        }
        for &(name, _) in &dropped {
            if let Some(c) = t.column(name) {
                lines.push(format!(
                    "ALTER TABLE {} ADD {};",
                    qualified(&k.name, &t.name),
                    column_def(&c.name, &c.ty, c.kind)
                ));
            }
        }
        b.new_line().append(lines.join("\n"));
    }
    b.finish()
}

/// A CDC log table: its options as an `ALTER TABLE` (the table itself comes with CDC).
pub fn cdc_log(t: &Table, _opts: Options) -> String {
    format!(
        "ALTER TABLE {} WITH {};\n",
        qualified(&t.keyspace, &t.name),
        options(&t.params).join("\n    AND ")
    )
}

/// An index target as `DESCRIBE` prints it: local indexes `(pk), ck`, `full()` for frozen
/// collections, `values()` for sets (stored as `keys(...)`).
fn index_target(t: &Table, target: &str) -> String {
    if target.starts_with('{') {
        let parse = |key: &str| -> Vec<String> {
            let Some(start) = target.find(&format!("\"{key}\":[")) else {
                return Vec::new();
            };
            let rest = &target[start + key.len() + 4..];
            let end = rest.find(']').unwrap_or(rest.len());
            rest[..end]
                .split(',')
                .map(|s| s.trim().trim_matches('"').replace("\\\"", "\""))
                .filter(|s| !s.is_empty())
                .map(|s| quote(&s, D))
                .collect()
        };
        let pk = parse("pk");
        let ck = parse("ck");
        let mut s = format!("({})", pk.join(", "));
        for c in ck {
            s.push_str(", ");
            // A frozen collection is indexed whole: `full(c)`.
            let unquoted = c
                .strip_prefix('"')
                .and_then(|x| x.strip_suffix('"'))
                .map(|x| x.replace("\"\"", "\""))
                .unwrap_or_else(|| c.clone());
            if t.column(&unquoted)
                .is_some_and(|col| col.ty.is_frozen_collection())
            {
                s.push_str(&format!("full({c})"));
            } else {
                s.push_str(&c);
            }
        }
        return s;
    }
    let unquoted = |s: &str| -> String {
        s.strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .map(|s| s.replace("\"\"", "\""))
            .unwrap_or_else(|| s.to_string())
    };
    if let Some(inner) = target
        .strip_prefix("keys(")
        .and_then(|s| s.strip_suffix(')'))
    {
        let set = t
            .column(&unquoted(inner))
            .is_some_and(|c| matches!(c.ty, nosql_types::CqlType::Set { .. }));
        return format!("{}({inner})", if set { "values" } else { "keys" });
    }
    if target.contains('(') {
        return target.to_string();
    }
    let frozen_collection = t.column(&unquoted(target)).is_some_and(|c| {
        matches!(
            c.ty,
            nosql_types::CqlType::List { frozen: true, .. }
                | nosql_types::CqlType::Set { frozen: true, .. }
                | nosql_types::CqlType::Map { frozen: true, .. }
        )
    });
    if frozen_collection {
        format!("full({target})")
    } else {
        target.to_string()
    }
}

pub fn index(k: &Keyspace, t: &Table, i: &IndexMeta, opts: Options) -> String {
    let target = i.options.get("target").map(String::as_str).unwrap_or("");
    if i.kind == "CUSTOM" {
        let mut rest = i.options.clone();
        rest.remove("target");
        rest.remove("index_version");
        let mut class = rest.remove("class_name").unwrap_or_default();
        // Stored as written; shown in lower case.
        if class.eq_ignore_ascii_case("vector_index")
            || class.eq_ignore_ascii_case("fulltext_index")
        {
            class = class.to_ascii_lowercase();
        }
        let target = match crate::index::vector_ddl::VectorTarget::parse(target) {
            Some(v) => v.describe(),
            None => target.to_string(),
        };
        let mut s = format!(
            "CREATE CUSTOM INDEX {} ON {}({}) USING '{}'",
            quote(&i.name, D),
            qualified(&k.name, &t.name),
            target,
            class.replace('\'', "''")
        );
        if !rest.is_empty() {
            s.push_str(" WITH OPTIONS = ");
            s.push_str(&super::options_map(&rest, D));
        }
        return s;
    }
    let mut b = CqlBuilder::new(D);
    b.append("CREATE INDEX ")
        .ident(&i.name)
        .append(" ON ")
        .append(qualified(&k.name, &t.name))
        .append("(")
        .append(index_target(t, target))
        .append(") WITH ");
    match k.tables.get(&format!("{}_index", i.name)) {
        Some(v) => with_items(&mut b, v, &v.params, opts, "\n    AND ", false),
        None => {
            if opts.internals {
                let id = crate::uuid::v5(&t.id, i.name.as_bytes());
                b.append("ID = ")
                    .append(crate::uuid::format(&id))
                    .append("\n    AND ");
            }
            let mut p = TableParams::defaults(D, false);
            p.tombstone_gc = t.params.tombstone_gc.clone();
            b.append(options(&p).join("\n    AND "));
        }
    }
    b.append(";").new_line();
    b.finish()
}

pub fn view(k: &Keyspace, v: &Table, opts: Options) -> String {
    let Some(meta) = &v.view else {
        return String::new();
    };
    let mut b = CqlBuilder::new(D);
    let index_view = index_of_view(k, &v.name).is_some();
    if index_view {
        b.append("/* Do NOT execute this statement! It's only for informational purposes.\n   This materialized view is the underlying materialized view of a secondary\n   index. It can be restored via restoring the index.\n\n");
    }
    b.append("CREATE MATERIALIZED VIEW ")
        .append(qualified(&k.name, &v.name))
        .append(" AS")
        .new_line()
        .append("    SELECT ");
    if meta.include_all_columns {
        b.append("*");
    } else {
        let cols: Vec<String> = super::cassandra::select_order(v)
            .into_iter()
            .map(|c: &Column| quote(&c.name, D))
            .collect();
        b.append(cols.join(", "));
    }
    b.new_line()
        .append("    FROM ")
        .append(qualified(&k.name, &meta.base_table_name))
        .new_line()
        .append("    WHERE ")
        .append(&meta.where_clause)
        .new_line()
        .append("    ")
        .append(primary_key(v))
        .new_line()
        .append("    WITH ");
    with_items(&mut b, v, &v.params, opts, "\nAND ", true);
    b.append(";").new_line();
    if index_view {
        b.new_line().append("*/");
    }
    b.finish()
}

fn function_name(ks: &str, name: &str) -> String {
    format!("{}.{}", quote(ks, D), quote(name, D))
}

/// A function's type as ScyllaDB describes it: without its top-level `frozen<>` (probed).
fn unfrozen(t: &str) -> &str {
    t.strip_prefix("frozen<")
        .and_then(|r| r.strip_suffix('>'))
        .unwrap_or(t)
}

pub fn function(k: &Keyspace, f: &FunctionMeta) -> String {
    let args: Vec<String> = f
        .argument_names
        .iter()
        .zip(&f.argument_types)
        .map(|(n, t)| format!("{} {}", quote(n, D), unfrozen(t)))
        .collect();
    format!(
        "CREATE FUNCTION {}({})\n{} ON NULL INPUT\nRETURNS {}\nLANGUAGE {}\nAS $${}$$;",
        function_name(&k.name, &f.name),
        args.join(", "),
        if f.called_on_null_input {
            "CALLED"
        } else {
            "RETURNS NULL"
        },
        unfrozen(&f.return_type),
        f.language,
        f.body
    )
}

pub fn aggregate(k: &Keyspace, a: &AggregateMeta) -> String {
    let mut s = format!(
        "CREATE AGGREGATE {}({})\nSFUNC {}\nSTYPE {}",
        function_name(&k.name, &a.name),
        a.argument_types.join(", "),
        quote(&a.state_func, D),
        a.state_type
    );
    if let Some(r) = &a.reduce_func {
        s.push_str(&format!("\nREDUCEFUNC {}", quote(r, D)));
    }
    if let Some(f) = &a.final_func {
        s.push_str(&format!("\nFINALFUNC {}", quote(f, D)));
    }
    if let Some(i) = &a.initcond {
        s.push_str(&format!("\nINITCOND {i}"));
    }
    s.push(';');
    s
}

/// Roles, grants and service levels (`DESCRIBE SCHEMA WITH INTERNALS`).
pub fn auth_elements(db: &Database, opts: Options) -> Vec<Element> {
    let snap = crate::auth::snapshot(db);
    let cfg = db.config();
    let default_role = cfg
        .superuser
        .as_ref()
        .map(|s| s.name.as_str())
        .unwrap_or("cassandra");
    let passwords = opts.passwords && crate::auth::requires_authentication(cfg);
    let el = |kind: Kind, name: &str, stmt: String| Element {
        keyspace: None,
        kind,
        name: name.to_string(),
        signature: None,
        statement: Some(stmt),
    };
    let mut out = Vec::new();
    // Roles.
    for (name, r) in &snap.roles {
        let mut s = format!(
            "CREATE ROLE {}{} WITH ",
            if name == default_role {
                "IF NOT EXISTS "
            } else {
                ""
            },
            quote(name, D)
        );
        if passwords && let Some(h) = &r.salted_hash {
            s.push_str(&format!(
                "HASHED PASSWORD = '{}' AND ",
                h.replace('\'', "''")
            ));
        }
        s.push_str(&format!(
            "LOGIN = {} AND SUPERUSER = {};",
            r.can_login, r.is_superuser
        ));
        out.push(el(Kind::Role, name, s));
    }
    // Role grants: `GRANT granted TO member`.
    let mut grants: Vec<(String, String)> = Vec::new();
    for (member, r) in &snap.roles {
        for granted in &r.member_of {
            grants.push((
                granted.clone(),
                format!("GRANT {} TO {};", quote(granted, D), quote(member, D)),
            ));
        }
    }
    grants.sort();
    out.extend(grants.into_iter().map(|(n, s)| el(Kind::GrantRole, &n, s)));
    // Permission grants (only with an authorizer that stores them).
    if crate::auth::authorizer::enforcing(db) {
        let mut perms: Vec<(String, String)> = Vec::new();
        for (role, by_resource) in &snap.permissions {
            for (resource, ps) in by_resource {
                let Some(target) = grant_target(resource) else {
                    continue;
                };
                for p in ps {
                    perms.push((
                        role.clone(),
                        format!("GRANT {p} ON {target} TO {};", quote(role, D)),
                    ));
                }
            }
        }
        perms.sort();
        out.extend(
            perms
                .into_iter()
                .map(|(n, s)| el(Kind::GrantPermission, &n, s)),
        );
    }
    // Service levels: the driver's own first; created by the system, so its absence means it
    // was dropped on purpose.
    if !snap.service_levels.contains_key("driver") {
        out.push(el(
            Kind::ServiceLevel,
            "driver",
            "DROP SERVICE LEVEL IF EXISTS driver;".to_string(),
        ));
    }
    let mut levels: Vec<(&String, &crate::auth::store::ServiceLevel)> =
        snap.service_levels.iter().collect();
    levels.sort_by_key(|(n, _)| (n.as_str() != "driver", n.as_str()));
    for (name, sl) in levels {
        let with = service_level_options(sl);
        let tail = if with.is_empty() {
            String::new()
        } else {
            format!(" WITH {with}")
        };
        if name == "driver" {
            out.push(el(
                Kind::ServiceLevel,
                name,
                format!(
                    "CREATE SERVICE LEVEL IF NOT EXISTS {}{tail};",
                    quote(name, D)
                ),
            ));
            out.push(el(
                Kind::ServiceLevel,
                name,
                format!("ALTER SERVICE LEVEL {}{tail};", quote(name, D)),
            ));
        } else {
            out.push(el(
                Kind::ServiceLevel,
                name,
                format!("CREATE SERVICE LEVEL {}{tail};", quote(name, D)),
            ));
        }
    }
    // Attachments.
    let mut attached: Vec<(String, String)> = Vec::new();
    for (role, attrs) in &snap.attributes {
        if let Some(sl) = attrs.get("service_level") {
            attached.push((
                sl.clone(),
                format!(
                    "ATTACH SERVICE LEVEL {} TO {};",
                    quote(sl, D),
                    quote(role, D)
                ),
            ));
        }
    }
    attached.sort();
    out.extend(
        attached
            .into_iter()
            .map(|(n, s)| el(Kind::ServiceLevelAttachment, &n, s)),
    );
    out
}

/// `TIMEOUT = 1000ms AND WORKLOAD_TYPE = 'batch' AND SHARES = 200`.
fn service_level_options(sl: &crate::auth::store::ServiceLevel) -> String {
    let mut parts = Vec::new();
    if let Some(t) = sl
        .timeout
        .as_deref()
        .and_then(nosql_types::Duration::decode)
    {
        let ms = (t.months as i64 * 30 + t.days as i64) * 86_400_000 + t.nanos / 1_000_000;
        parts.push(format!("TIMEOUT = {ms}ms"));
    }
    if let Some(w) = &sl.workload_type {
        parts.push(format!("WORKLOAD_TYPE = '{w}'"));
    }
    if let Some(s) = sl.shares {
        parts.push(format!("SHARES = {s}"));
    }
    parts.join(" AND ")
}

/// The resource of a stored permission in `GRANT … ON <target>` form.
fn grant_target(resource: &str) -> Option<String> {
    use crate::auth::Resource as R;
    Some(match R::from_name(resource)? {
        R::DataRoot => "ALL KEYSPACES".into(),
        R::Keyspace(k) => format!("KEYSPACE {}", quote(&k, D)),
        R::AllTables(k) => format!("ALL TABLES IN KEYSPACE {}", quote(&k, D)),
        R::Table(k, t) => qualified(&k, &t),
        R::RolesRoot => "ALL ROLES".into(),
        R::Role(r) => format!("ROLE {}", quote(&r, D)),
        R::FunctionsRoot => "ALL FUNCTIONS".into(),
        R::FunctionsIn(k) => format!("ALL FUNCTIONS IN KEYSPACE {}", quote(&k, D)),
        R::Function {
            keyspace,
            name,
            cql,
            ..
        } => format!(
            "FUNCTION {}({})",
            function_name(&keyspace, &name),
            cql.iter().map(|t| unfrozen(t)).collect::<Vec<_>>().join(", ")
        ),
        R::MbeansRoot => "ALL MBEANS".into(),
        R::Mbean(n) => format!("MBEAN {n}"),
        R::ServiceLevels => "ALL SERVICE_LEVELS".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_print_like_cpp() {
        assert_eq!(double(1.0), "1");
        assert_eq!(double(0.01), "0.01");
        assert_eq!(double(0.1), "0.1");
    }

    #[test]
    fn grant_targets() {
        assert_eq!(grant_target("data").unwrap(), "ALL KEYSPACES");
        assert_eq!(grant_target("data/ks/T").unwrap(), "ks.\"T\"");
        assert_eq!(
            grant_target("functions/ks").unwrap(),
            "ALL FUNCTIONS IN KEYSPACE ks"
        );
        assert_eq!(grant_target("roles/Desc R3").unwrap(), "ROLE \"Desc R3\"");
        assert!(grant_target("nope/x").is_none());
    }
}
