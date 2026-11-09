//! Schema changes: CREATE / ALTER / DROP of keyspaces, tables and user types, and TRUNCATE.
//!
//! Each statement validates against the current snapshot with the dialect's rules and messages
//! (Cassandra's `cql3.statements.schema.*Statement`, Apache-2.0; ScyllaDB's from the image),
//! builds the new keyspace definition, then [`commit`]s it: the keyspace's `system_schema` rows are
//! rewritten in one atomic batch, new tables are attached to storage, dropped ones detached, the
//! snapshot is swapped and the change is broadcast. Changes are serialised by the database's DDL
//! lock.

use std::collections::BTreeMap;
use std::sync::Arc;

use nosql_cql::ast::{
    self, AlterTableAction, AlterTypeAction, Property, PropertyValue, Statement, StatementKind,
    TableOption, UsingClause,
};
use nosql_storage::manifest::TableId;
use nosql_types::{CqlType, Dialect, UdtDef};

use super::params::{self, Properties, TableParams};
use super::replication::{self, Replication};
use super::{
    Column, ColumnKind, DroppedColumn, Keyspace, KeyspaceKind, Schema, Table, TableFlags, TableKind,
};
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::Access;
use crate::messages as msg;
use crate::prepare::terms::{Bound, EvalCtx, Term};
use crate::response::{ChangeTarget, ChangeType, Response, ResponseBody, SchemaChange};
use crate::session::SessionState;

/// What a schema change does.
#[derive(Default)]
pub struct Change {
    /// The keyspace after the change (`None` with `drop_keyspace`).
    pub keyspace: Option<Keyspace>,
    pub drop_keyspace: Option<String>,
    /// Tables to attach (new or with changed options).
    pub attach: Vec<Arc<Table>>,
    /// Storage tables to drop.
    pub drop_tables: Vec<TableId>,
    pub events: Vec<SchemaChange>,
    pub warnings: Vec<String>,
    /// The RESULT (the first event when `None`).
    pub result: Option<SchemaChange>,
}

/// Writes a change: storage tables, schema rows, the new snapshot, events.
///
/// Atomic: tables are attached first (new ones on fresh storage, see [`Table::assign_storage`]),
/// so a failure before the schema rows are written undoes them and leaves the schema as it was.
/// Once the rows are written the change is installed; dropping storage tables can't fail it.
pub async fn commit(db: &Arc<Database>, schema: &Schema, mut change: Change) -> Result<Response> {
    if let Some(ks) = &mut change.keyspace {
        assign_storage(db, schema, ks, &mut change.attach);
    }
    if let Some(ks) = &mut change.keyspace {
        crate::masking::carry_stale(schema.keyspace(&ks.name).map(|k| &**k), ks);
    }
    let ts = db.timestamp();
    let now = db.now_secs();
    let mutations = match (&change.keyspace, &change.drop_keyspace) {
        (Some(ks), _) => super::persist::keyspace_mutations(ks, &db.schema_tables, ts, now),
        (None, Some(name)) => {
            super::persist::drop_keyspace_mutations(name, &db.schema_tables, ts, now)
        }
        (None, None) => Vec::new(),
    };
    let mut attached: Vec<&Arc<Table>> = Vec::new();
    let mut failed = None;
    for t in &change.attach {
        let existed = db.store.get_table(t.table_id()).is_some();
        match crate::storage::attach(&db.store, t, db.dialect(), db.memtable_flush_bytes()).await {
            Ok(_) => attached.push(t),
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
        if existed {
            attached.pop();
        }
    }
    if failed.is_none()
        && let Err(e) = db.store.apply_batch(mutations).await
    {
        failed = Some(Error::internal(e));
    }
    if let Some(e) = failed {
        // Undo: new storage tables are dropped (their ids are never used again), changed ones
        // get their options back.
        for t in attached {
            if let Err(e) = db.store.drop_table(t.table_id()).await {
                tracing::warn!("undoing {}.{}: {e}", t.keyspace, t.name);
            }
        }
        for t in &change.attach {
            if let Some(old) = schema.table(&t.keyspace, &t.name)
                && old.table_id() == t.table_id()
            {
                let _ =
                    crate::storage::attach(&db.store, old, db.dialect(), db.memtable_flush_bytes())
                        .await;
            }
        }
        return Err(e);
    }
    for id in &change.drop_tables {
        // The schema rows are written: the change happened. Storage marks the table dropped
        // before it writes its manifest, so a failure here only delays that write.
        if let Err(e) = db.store.drop_table(*id).await {
            tracing::warn!("dropping storage table {id:?}: {e}");
        }
    }
    let next = match (change.keyspace, &change.drop_keyspace) {
        (Some(ks), _) => schema.with_keyspace(ks),
        (None, Some(name)) => schema.without_keyspace(name),
        (None, None) => schema.clone(),
    };
    db.install_schema(next, &change.events);
    let result = change.result.or_else(|| change.events.first().cloned());
    let body = match result {
        Some(c) => ResponseBody::SchemaChange(c),
        None => ResponseBody::Void,
    };
    Ok(Response::new(body).with_warnings(change.warnings))
}

/// Gives the tables and indexes `ks` gains fresh storage generations (past dropped ids), keeps
/// the existing ones' storage, and adds the hidden index tables of new Cassandra indexes to
/// `attach`.
fn assign_storage(db: &Database, schema: &Schema, ks: &mut Keyspace, attach: &mut Vec<Arc<Table>>) {
    let old = schema.keyspace(&ks.name);
    let d = db.dialect();
    let mut dropped = None;
    for t in ks.tables.values_mut() {
        let o = old
            .and_then(|k| k.tables.get(&t.name))
            .filter(|o| o.id == t.id)
            .cloned();
        let mut n: Option<Table> = None;
        if let Some(o) = &o {
            let kept = |name: &String| t.indexes.iter().any(|m| &m.name == name);
            let carried = t.generation == o.generation
                && o.index_generations
                    .iter()
                    .all(|(k, g)| !kept(k) || t.index_generations.get(k) == Some(g));
            if !carried {
                let mut c = (**t).clone();
                c.generation = o.generation;
                for (k, g) in &o.index_generations {
                    if kept(k) {
                        c.index_generations.insert(k.clone(), *g);
                    }
                }
                n = Some(c);
            }
        }
        let added: Vec<String> = t
            .indexes
            .iter()
            .filter(|m| {
                o.as_ref()
                    .is_none_or(|o| !o.indexes.iter().any(|i| i.name == m.name))
            })
            .map(|m| m.name.clone())
            .collect();
        if o.is_none() || !added.is_empty() {
            let dropped = dropped.get_or_insert_with(|| db.store.manifest().dropped);
            let c = n.get_or_insert_with(|| (**t).clone());
            c.assign_storage(dropped);
        }
        if let Some(c) = n {
            let c = Arc::new(c);
            for a in attach.iter_mut() {
                if a.keyspace == c.keyspace && a.name == c.name && a.id == c.id {
                    *a = c.clone();
                }
            }
            *t = c;
        }
        for def in crate::index::defs(t) {
            if added.contains(&def.name)
                && let Some(h) = crate::index::hidden_table(t, &def, d)
            {
                attach.push(Arc::new(h));
            }
        }
    }
}

fn void(warnings: Vec<String>) -> Response {
    Response::void().with_warnings(warnings)
}

/// A RESULT announcing a change that didn't happen (ScyllaDB answers `IF NOT EXISTS` on an
/// existing object this way).
fn announce(c: SchemaChange, warnings: Vec<String>) -> Response {
    Response::new(ResponseBody::SchemaChange(c)).with_warnings(warnings)
}

fn keyspace_of(name: &ast::QualifiedName, session_ks: Option<&str>) -> Result<String> {
    match &name.keyspace {
        Some(k) => Ok(k.name().to_string()),
        None => session_ks.map(str::to_string).ok_or_else(msg::no_keyspace),
    }
}

pub(crate) fn is_system(name: &str) -> bool {
    matches!(
        name,
        "system"
            | "system_schema"
            | "system_auth"
            | "system_distributed"
            | "system_traces"
            | "system_views"
            | "system_virtual_schema"
            | "system_replicated_keys"
            | "system_distributed_everywhere"
            | "audit"
    )
}

fn system_unmodifiable(ks: &str) -> Error {
    Error::unauthorized(format!("{ks} keyspace is not user-modifiable."))
}

/// Executes a DDL statement.
/// Boxed so the future is checked `Send` here, with named lifetimes (rust-lang/rust#110338).
pub fn execute<'a>(
    db: &'a Arc<Database>,
    session: &'a mut SessionState,
    stmt: &'a Statement<'static>,
    keyspace: Option<&'a str>,
    values: &'a [crate::response::Param],
) -> futures::future::BoxFuture<'a, Result<Response>> {
    Box::pin(execute_inner(db, session, stmt, keyspace, values))
}

async fn execute_inner(
    db: &Arc<Database>,
    session: &mut SessionState,
    stmt: &Statement<'static>,
    keyspace: Option<&str>,
    values: &[crate::response::Param],
) -> Result<Response> {
    let _g = db.ddl.lock().await;
    let schema = db.schema();
    let d = db.dialect();
    let session_ks = keyspace
        .map(str::to_string)
        .or_else(|| session.keyspace.clone());
    let session_ks = session_ks.as_deref();
    let hooks = db.hooks();
    let access = |ks: &str, t: &str, s: &'static str| -> Result<()> {
        hooks.authorizer.authorize(
            db,
            session,
            &Access::Schema {
                keyspace: ks,
                table: t,
                statement: s,
            },
        )
    };
    match &stmt.kind {
        StatementKind::CreateKeyspace(c) => {
            let name = c.name.name().to_string();
            access(&name, "", "CREATE KEYSPACE")?;
            create_keyspace(db, &schema, c, d).await
        }
        StatementKind::AlterKeyspace(a) => {
            let name = a.name.name().to_string();
            access(&name, "", "ALTER KEYSPACE")?;
            alter_keyspace(db, &schema, a, d).await
        }
        StatementKind::DropKeyspace(x) => {
            let name = x.name.name.name().to_string();
            access(&name, "", "DROP KEYSPACE")?;
            drop_keyspace(db, &schema, &name, x.if_exists, d).await
        }
        StatementKind::CreateTable(c) => {
            let ks = keyspace_of(&c.name, session_ks)?;
            access(&ks, &c.name.name.name(), "CREATE TABLE")?;
            create_table(db, &schema, c, &ks, d).await
        }
        StatementKind::AlterTable(a) => {
            let ks = keyspace_of(&a.name, session_ks)?;
            if let Some(r) = alter_system_flush_period(&schema, a, &ks, d) {
                return r;
            }
            access(&ks, &a.name.name.name(), "ALTER TABLE")?;
            alter_table(db, &schema, a, &ks, d, values).await
        }
        StatementKind::DropTable(x) => {
            let ks = keyspace_of(&x.name, session_ks)?;
            access(&ks, &x.name.name.name(), "DROP TABLE")?;
            drop_table(db, &schema, &ks, &x.name.name.name(), x.if_exists, d).await
        }
        StatementKind::CreateType(c) => {
            let ks = keyspace_of(&c.name, session_ks)?;
            access(&ks, "", "CREATE TYPE")?;
            create_type(db, &schema, c, &ks, d).await
        }
        StatementKind::AlterType(a) => {
            let ks = keyspace_of(&a.name, session_ks)?;
            access(&ks, "", "ALTER TYPE")?;
            alter_type(db, &schema, a, &ks, d).await
        }
        StatementKind::DropType(x) => {
            let ks = keyspace_of(&x.name, session_ks)?;
            access(&ks, "", "DROP TYPE")?;
            drop_type(db, &schema, &ks, &x.name.name.name(), x.if_exists, d).await
        }
        other => Err(crate::hooks::unsupported(other)),
    }
}

// ---- keyspaces -------------------------------------------------------------------------------

fn keyspace_options(d: Dialect) -> &'static [&'static str] {
    match d {
        Dialect::Scylla => &["replication", "durable_writes", "tablets", "storage"],
        Dialect::Cassandra => &["replication", "durable_writes"],
    }
}

fn durable_writes(p: &Properties, d: Dialect) -> Result<Option<bool>> {
    match d {
        Dialect::Cassandra => p.boolean("durable_writes"),
        Dialect::Scylla => Ok(p
            .string("durable_writes")?
            .map(|s| s.eq_ignore_ascii_case("true"))),
    }
}

/// ScyllaDB's `tablets = {…}`: `Some(initial)` for a tablets keyspace.
fn tablets_option(
    p: &Properties,
    default: bool,
    warnings: &mut Vec<String>,
) -> Result<Option<i32>> {
    let Some(m) = p.map("tablets")? else {
        return Ok(default.then_some(0));
    };
    let (enabled, initial) = tablets_map(&m)?;
    if enabled == Some(false) {
        return Ok(None);
    }
    if let Some(n) = initial {
        warnings.push("Keyspace `initial` tablets option is deprecated.  Use per-table tablet options instead.".into());
        return Ok(Some(n));
    }
    Ok(Some(0))
}

/// ScyllaDB's keyspace `tablets = {'enabled': …, 'initial': …}`: `(enabled, initial)`.
fn tablets_map(m: &BTreeMap<String, String>) -> Result<(Option<bool>, Option<i32>)> {
    let mut enabled = None;
    let mut initial = None;
    for (k, v) in m {
        match k.as_str() {
            "enabled" => {
                enabled = Some(match v.to_ascii_lowercase().as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(Error::config(format!(
                            "Tablets enabled value must be true or false; found: {v}"
                        )));
                    }
                })
            }
            "initial" => {
                let n: i64 = v.trim().parse().map_err(|_| {
                    Error::config(format!("Initial tablets value should be numeric; found {v}"))
                })?;
                initial = Some(n as i32);
            }
            _ => return Err(Error::config(format!("Unrecognized tablets option {k}"))),
        }
    }
    Ok((enabled, initial))
}

async fn create_keyspace(
    db: &Arc<Database>,
    schema: &Schema,
    c: &ast::CreateKeyspace<'static>,
    d: Dialect,
) -> Result<Response> {
    let name = c.name.name().to_string();
    if let Some(e) = msg::keyspace_name_invalid(d, &name) {
        return Err(e);
    }
    let p = Properties::new(c.properties.iter(), keyspace_options(d), d)?;
    let replication_map = p.map("replication")?;
    if d.is_scylla() {
        replication::check_storage(p.map("storage")?.as_ref(), false)?;
    }
    if !d.is_scylla() && replication_map.is_none() {
        return Err(Error::invalid("Missing mandatory option 'replication'"));
    }
    let mut warnings = Vec::new();
    let class_is_nts = replication_map
        .as_ref()
        .and_then(|m| m.get("class"))
        .is_none_or(|c| c.rsplit('.').next() == Some("NetworkTopologyStrategy"));
    let tablets = if d.is_scylla() {
        let default = db.settings().tablets_by_default && class_is_nts;
        let default = default
            || (db.settings().tablets_by_default
                && replication_map
                    .as_ref()
                    .and_then(|m| m.get("class"))
                    .is_some()
                && p.map("tablets")?.is_none());
        tablets_option(&p, default, &mut warnings)?
    } else {
        None
    };
    let exists = schema.keyspace(&name).is_some();
    if exists && !c.if_not_exists {
        return Err(msg::keyspace_exists(&name));
    }
    let replication = replication::prepare(
        d,
        replication::Request {
            keyspace: &name,
            replication: replication_map,
            previous: None,
            tablets: tablets.is_some(),
            datacenter: &db.cfg.datacenter,
            rack: &db.cfg.rack,
            racks: 1,
            nodes: 1,
            guard: replication::Guard::of(&db.settings(), Some(db.guardrail_metrics())),
        },
        &mut warnings,
    )?;
    if exists {
        return Ok(if d.is_scylla() {
            announce(SchemaChange::keyspace(ChangeType::Created, &name), warnings)
        } else {
            void(Vec::new())
        });
    }
    if is_system(&name) {
        return Err(msg::keyspace_exists(&name));
    }
    let mut ks = Keyspace::new(name.clone(), replication, KeyspaceKind::User);
    ks.durable_writes = durable_writes(&p, d)?.unwrap_or(true);
    ks.tablets = tablets;
    let change = Change {
        keyspace: Some(ks),
        events: vec![SchemaChange::keyspace(ChangeType::Created, &name)],
        warnings,
        ..Default::default()
    };
    commit(db, schema, change).await
}

async fn alter_keyspace(
    db: &Arc<Database>,
    schema: &Schema,
    a: &ast::AlterKeyspace<'static>,
    d: Dialect,
) -> Result<Response> {
    let name = a.name.name().to_string();
    let p = Properties::new(a.properties.iter(), keyspace_options(d), d)?;
    let Some(current) = schema.keyspace(&name) else {
        if a.if_exists {
            return Ok(void(Vec::new()));
        }
        return Err(msg::ddl_unknown_keyspace(d, &name));
    };
    if current.is_builtin() {
        return Err(system_unmodifiable(&name));
    }
    let mut ks = (**current).clone();
    let mut warnings = Vec::new();
    if d.is_scylla() {
        replication::check_storage(p.map("storage")?.as_ref(), true)?;
    }
    // `tablets = {'initial': n}` changes a tablets keyspace's initial count (`{'enabled':
    // true}` resets it); nothing changes vnodes to tablets or back.
    if d.is_scylla()
        && let Some(m) = p.map("tablets")?
    {
        let (enabled, initial) = tablets_map(&m)?;
        let flavor = || Error::invalid("Cannot alter replication strategy vnode/tablets flavor");
        match current.tablets {
            Some(_) if enabled == Some(false) => return Err(flavor()),
            Some(_) => ks.tablets = Some(initial.unwrap_or(0)),
            None if enabled != Some(false) || initial.is_some() => return Err(flavor()),
            None => {}
        }
    }
    if d.is_scylla()
        && current.tablets.is_some()
        && let Some(m) = p.map("replication")?
    {
        {
            if let Some(c) = m.get("class")
                && !replication::known_class(c)
            {
                return Err(Error::config(format!("unable to find class '{c}'")));
            }
            let class = m
                .get("class")
                .map(|c| c.rsplit('.').next().unwrap_or(c).to_string());
            if class
                .as_deref()
                .is_some_and(|c| c != current.replication.short_class())
            {
                return Err(Error::invalid(
                    "Cannot alter replication strategy vnode/tablets flavor",
                ));
            }
            for (k, v) in &m {
                if k == "class" {
                    continue;
                }
                let new: i64 = match replication::rack_list(v) {
                    Some(l) => l.len() as i64,
                    None => v.trim().parse().unwrap_or(0),
                };
                let old: i64 = current
                    .replication
                    .options
                    .get(k)
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(0);
                if (new - old).abs() > 1 {
                    return Err(Error::invalid(
                        "Only one DC's RF can be changed at a time and not by more than 1",
                    ));
                }
            }
        }
    }
    if let Some(m) = p.map("replication")? {
        let r = replication::prepare(
            d,
            replication::Request {
                keyspace: &name,
                replication: Some(m),
                previous: Some(&current.replication),
                tablets: current.tablets.is_some(),
                datacenter: &db.cfg.datacenter,
                rack: &db.cfg.rack,
                racks: 1,
                nodes: 1,
                guard: replication::Guard::of(&db.settings(), Some(db.guardrail_metrics())),
            },
            &mut warnings,
        )?;
        if !d.is_scylla() {
            let old_total: i64 = current
                .replication
                .options
                .values()
                .filter_map(|v| v.parse::<i64>().ok())
                .sum();
            let new_total: i64 = r
                .options
                .values()
                .filter_map(|v| v.parse::<i64>().ok())
                .sum();
            if new_total > old_total {
                warnings.push("When increasing replication factor you need to run a full (-full) repair to distribute the data.".into());
            }
        }
        ks.replication = r;
    }
    if let Some(dw) = durable_writes(&p, d)? {
        ks.durable_writes = dw;
    }
    // Cassandra reports no change when nothing changed.
    if !d.is_scylla()
        && ks.replication == current.replication
        && ks.durable_writes == current.durable_writes
    {
        return Ok(void(warnings));
    }
    let change = Change {
        keyspace: Some(ks),
        events: vec![SchemaChange::keyspace(ChangeType::Updated, &name)],
        warnings,
        ..Default::default()
    };
    commit(db, schema, change).await
}

async fn drop_keyspace(
    db: &Arc<Database>,
    schema: &Schema,
    name: &str,
    if_exists: bool,
    d: Dialect,
) -> Result<Response> {
    let Some(ks) = schema.keyspace(name) else {
        if if_exists {
            return Ok(void(Vec::new()));
        }
        return Err(msg::drop_unknown_keyspace(d, name));
    };
    if ks.is_builtin() {
        return Err(system_unmodifiable(name));
    }
    let mut drop_tables: Vec<TableId> = ks.tables.values().map(|t| t.table_id()).collect();
    for t in ks.tables.values() {
        drop_tables.extend(crate::index::hidden_tables(t));
    }
    let change = Change {
        drop_keyspace: Some(name.to_string()),
        drop_tables,
        events: vec![SchemaChange::keyspace(ChangeType::Dropped, name)],
        ..Default::default()
    };
    commit(db, schema, change).await
}

// ---- tables ----------------------------------------------------------------------------------

fn resolver<'a>(ks: Option<&'a Keyspace>) -> impl Fn(&str) -> Option<Arc<UdtDef>> + 'a {
    move |name| ks.and_then(|k| k.types.get(name).cloned())
}

async fn create_table(
    db: &Arc<Database>,
    schema: &Schema,
    c: &ast::CreateTable<'static>,
    ks_name: &str,
    d: Dialect,
) -> Result<Response> {
    let tname = c.name.name.name().to_string();
    // `CreateTableStatement.Raw.prepare`: column declarations and the primary key.
    let mut declared: Vec<&ast::ColumnDef> = Vec::new();
    for col in &c.columns {
        if declared.iter().any(|x| x.name.name() == col.name.name()) {
            return Err(msg::duplicate_column_declaration(
                d,
                ks_name,
                &tname,
                &col.name.name(),
            ));
        }
        declared.push(col);
    }
    // The parser records inline `PRIMARY KEY`s as single-column keys too.
    if c.primary_keys.is_empty() {
        return Err(msg::no_primary_key(d, ks_name, &tname));
    }
    if c.primary_keys.len() > 1 {
        return Err(msg::multiple_primary_keys(d, ks_name, &tname));
    }
    // `ColumnProperties.Raw.validate`, before the statement applies.
    if c.columns.iter().any(|x| x.mask.is_some()) {
        crate::masking::ensure_enabled(db)?;
    }
    let pk = &c.primary_keys[0];
    let (pk_names, ck_names): (Vec<String>, Vec<String>) = (
        pk.partition.iter().map(|i| i.name().to_string()).collect(),
        pk.clustering.iter().map(|i| i.name().to_string()).collect(),
    );
    // `apply`: the keyspace and existing names.
    let Some(ks) = schema.keyspace(ks_name) else {
        return Err(msg::ddl_unknown_keyspace(d, ks_name));
    };
    if ks.is_builtin() {
        return Err(system_unmodifiable(ks_name));
    }
    if ks.tables.contains_key(&tname) {
        if c.if_not_exists {
            return Ok(if d.is_scylla() {
                announce(
                    SchemaChange::object(ChangeType::Created, ChangeTarget::Table, ks_name, &tname),
                    Vec::new(),
                )
            } else {
                void(Vec::new())
            });
        }
        return Err(msg::table_exists(ks_name, &tname));
    }
    if let Some(e) = msg::table_name_invalid(d, ks_name, &tname) {
        return Err(e);
    }
    // Options.
    let props: Vec<&Property> = c
        .options
        .iter()
        .filter_map(|o| {
            if let TableOption::Property(p) = o {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    let p = Properties::new(props.iter().copied(), params::valid_table_options(d), d)?;
    let mut tparams = TableParams::defaults(d, false);
    params::apply(&mut tparams, &p, false)?;
    check_tablet_hints(&p, ks)?;
    let compact = c
        .options
        .iter()
        .any(|o| matches!(o, TableOption::CompactStorage));
    if compact && d.is_scylla() && !db.settings().compact_storage_enabled {
        return Err(msg::compact_storage_disabled());
    }
    let id = match p.string("id")? {
        Some(s) => crate::uuid::parse(s.trim()).ok_or_else(|| Error::config("Invalid table id"))?,
        None => crate::uuid::timeuuid(),
    };
    if schema.table_by_id(&id).is_some() {
        return Err(Error::invalid(format!(
            "Table with ID {} already exists",
            crate::uuid::format(&id)
        )));
    }
    // Column types (in name order, as Cassandra's TreeMap).
    let resolve = resolver(Some(ks));
    let mut sorted: Vec<&ast::ColumnDef> = declared.clone();
    sorted.sort_by(|a, b| a.name.name().as_bytes().cmp(b.name.name().as_bytes()));
    let mut types: BTreeMap<String, CqlType> = BTreeMap::new();
    let mut masks: BTreeMap<String, super::ColumnMask> = BTreeMap::new();
    for col in &sorted {
        if col.mask.is_some() && d.is_scylla() {
            return Err(Error::invalid("Column masking is not supported"));
        }
        let ty = super::types::resolve(&col.ty, ks_name, &resolve, d)?;
        if let Some(m) = &col.mask {
            let (mask, _) = crate::masking::prepare(
                db,
                schema,
                ks_name,
                &tname,
                &col.name.name(),
                &ty,
                m,
            )?;
            masks.insert(col.name.name().to_string(), mask);
        }
        if let CqlType::Udt(def, false) = &ty
            && def.fields.iter().any(|(_, f)| f.is_multicell())
        {
            return Err(Error::invalid(
                "Non-frozen UDTs with nested non-frozen collections are not supported",
            ));
        }
        types.insert(col.name.name().to_string(), ty);
    }
    // ScyllaDB's per-row TTL column, in declaration order.
    let mut ttl_column: Option<String> = None;
    for col in declared.iter().filter(|x| x.ttl) {
        let name = col.name.name();
        if let Some(ty) = types.get(name.as_ref()) {
            crate::rowttl::check_declared(&mut ttl_column, &name, ty)?;
        }
    }
    // Primary key columns.
    let mut seen: Vec<&str> = Vec::new();
    for name in pk_names.iter().chain(ck_names.iter()) {
        let Some(ty) = types.get(name) else {
            return Err(msg::pk_unknown_column(d, &tname, name));
        };
        if seen.contains(&name.as_str()) {
            return Err(msg::pk_duplicate_column(d, &tname, name));
        }
        seen.push(name);
        if let Some(e) = ty.primary_key_error(name, d) {
            // Cassandra names a UDT by its prepared `UTName` (`ks.name`).
            if let (Dialect::Cassandra, CqlType::Udt(def, false)) = (d, ty) {
                return Err(Error::invalid(format!(
                    "Invalid non-frozen user-defined type \"{}.{}\" for PRIMARY KEY column '{name}'",
                    def.keyspace, def.name
                )));
            }
            return Err(Error::invalid(e));
        }
        if declared
            .iter()
            .any(|x| x.name.name() == name.as_str() && x.is_static)
        {
            return Err(msg::static_in_primary_key(d, name));
        }
    }
    // CLUSTERING ORDER BY.
    let mut order: Vec<(String, bool)> = Vec::new();
    for o in &c.options {
        if let TableOption::ClusteringOrder(cols) = o {
            for (id, dir) in cols {
                let n = id.name().to_string();
                if order.iter().any(|(x, _)| x == &n) {
                    return Err(Error::invalid(format!(
                        "Duplicate column '{n}' in CLUSTERING ORDER BY clause for table '{tname}'"
                    )));
                }
                order.push((n, *dir == ast::Direction::Desc));
            }
        }
    }
    let non_ck: Vec<String> = order
        .iter()
        .filter(|(n, _)| !ck_names.contains(n))
        .map(|(n, _)| n.clone())
        .collect();
    if !non_ck.is_empty() {
        return Err(msg::clustering_order_not_clustering(d, &non_ck));
    }
    for (i, (n, _)) in order.iter().enumerate() {
        let expected = &ck_names[i];
        if n != expected {
            if order.iter().any(|(x, _)| x == expected) {
                return Err(msg::clustering_order_wrong_order(d, expected, n));
            }
            return Err(Error::invalid(format!(
                "Missing CLUSTERING ORDER for column {expected}"
            )));
        }
    }
    let statics: Vec<&str> = declared
        .iter()
        .filter(|x| x.is_static)
        .map(|x| x.name.text.as_ref())
        .collect();
    if ck_names.is_empty() && !statics.is_empty() && !compact {
        return Err(msg::static_without_clustering());
    }
    // Counters.
    let has_counters = types.values().any(|t| t.is_counter());
    if has_counters {
        let regular: Vec<&String> = types
            .keys()
            .filter(|n| !pk_names.contains(n) && !ck_names.contains(n))
            .collect();
        if let Some(bad) = regular.iter().find(|n| !types[**n].is_counter()) {
            return Err(msg::counter_mixed(d, bad));
        }
        if tparams.default_time_to_live > 0 {
            return Err(msg::counter_with_ttl());
        }
    }
    // Scylla: tablets keyspaces need enough nodes for their RF.
    if d.is_scylla() && ks.tablets.is_some() {
        for (dc, rf) in &ks.replication.options {
            if rf.parse::<u32>().unwrap_or(0) > 1 {
                return Err(Error::config(format!(
                    "Datacenter {dc} doesn't have enough token-owning nodes for replication_factor={rf}"
                )));
            }
        }
    }
    // Build.
    let mut columns = Vec::with_capacity(types.len());
    let canonical = |name: &str| -> (ColumnKind, i32, bool) {
        if let Some(i) = pk_names.iter().position(|n| n == name) {
            (ColumnKind::PartitionKey, i as i32, false)
        } else if let Some(i) = ck_names.iter().position(|n| n == name) {
            let rev = order
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, r)| *r)
                .unwrap_or(false);
            (ColumnKind::Clustering, i as i32, rev)
        } else if declared
            .iter()
            .any(|x| x.name.name() == name && x.is_static)
        {
            (ColumnKind::Static, -1, false)
        } else {
            (ColumnKind::Regular, -1, false)
        }
    };
    if let Some(name) = &ttl_column {
        crate::rowttl::check_kind(name, canonical(name).0)?;
        crate::rowttl::set_tag(&mut tparams, Some(name));
    }
    if compact {
        let cols: Vec<(&str, ColumnKind, bool)> = types
            .iter()
            .map(|(n, t)| (n.as_str(), canonical(n).0, t.is_multicell()))
            .collect();
        super::compact::check_create(&cols, !ck_names.is_empty())?;
    }
    let mut ordered: Vec<(String, CqlType)> = types.into_iter().collect();
    // Ids in canonical order.
    ordered.sort_by(|a, b| {
        let (ka, pa, _) = canonical(&a.0);
        let (kb, pb, _) = canonical(&b.0);
        ka.cmp(&kb)
            .then(pa.cmp(&pb))
            .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
    });
    for (id, (name, ty)) in (0u32..).zip(ordered) {
        let (kind, position, reversed) = canonical(&name);
        // Clustering columns sort by their non-frozen-normalised type; collections in keys are frozen.
        columns.push(Column {
            name,
            ty,
            kind,
            position,
            reversed,
            id,
        });
    }
    let flags = TableFlags {
        compound: !compact,
        dense: compact && !ck_names.is_empty(),
        counter: has_counters,
        super_: false,
    };
    let mut table = Table::new(
        ks_name,
        &tname,
        id,
        crate::uuid::timeuuid(),
        columns,
        tparams,
        flags,
        TableKind::Regular,
    );
    table.masks = masks;
    let table = Arc::new(table);
    let mut new_ks = (**ks).clone();
    new_ks.tables.insert(tname.clone(), table.clone());
    let mut attach = vec![table.clone()];
    // ScyllaDB CDC: the log table comes with its base.
    if d.is_scylla() && crate::cdc::enabled(&table) {
        let log = crate::cdc::log_table(&table, d)?;
        if new_ks.tables.contains_key(&log.name) {
            return Err(crate::cdc::log_name_taken(ks_name, &tname));
        }
        let log = Arc::new(log);
        new_ks.tables.insert(log.name.clone(), log.clone());
        attach.push(log);
    }
    let change = Change {
        keyspace: Some(new_ks),
        attach,
        events: vec![SchemaChange::object(
            ChangeType::Created,
            ChangeTarget::Table,
            ks_name,
            &tname,
        )],
        ..Default::default()
    };
    commit(db, schema, change).await
}

/// ScyllaDB lets `memtable_flush_period_in_ms` alone be set on a system table (#21223). roll's
/// system tables have no memtables to flush: the change is validated and announced, and
/// changes nothing (so it needs no permission either).
fn alter_system_flush_period(
    schema: &Schema,
    a: &ast::AlterTable<'static>,
    ks_name: &str,
    d: Dialect,
) -> Option<Result<Response>> {
    let tname = a.name.name.name().to_string();
    let AlterTableAction::With(props) = &a.action else {
        return None;
    };
    let only_flush_period =
        props.len() == 1 && props[0].name.name() == "memtable_flush_period_in_ms";
    let table = schema.keyspace(ks_name).and_then(|k| k.tables.get(&tname))?;
    let virtual_ = crate::guardrails::config_table::SCYLLA_VIRTUAL.contains(&table.name.as_str());
    if !d.is_scylla() || ks_name != "system" || !only_flush_period || virtual_ {
        return None;
    }
    Some((|| {
        let p = Properties::new(props.iter(), params::valid_table_options(d), d)?;
        p.int("memtable_flush_period_in_ms")?;
        Ok(announce(
            SchemaChange::object(ChangeType::Updated, ChangeTarget::Table, ks_name, &tname),
            Vec::new(),
        ))
    })())
}

/// ScyllaDB validates CREATE TABLE's options when the statement is prepared (so a prepared
/// statement never holds unchecked options).
pub fn validate_create_options(d: Dialect, c: &ast::CreateTable<'_>) -> Result<()> {
    if !d.is_scylla() {
        return Ok(());
    }
    let props: Vec<&Property> = c
        .options
        .iter()
        .filter_map(|o| match o {
            TableOption::Property(p) => Some(p),
            _ => None,
        })
        .collect();
    let p = Properties::new(props.iter().copied(), params::valid_table_options(d), d)?;
    params::apply(&mut TableParams::defaults(d, false), &p, false)
}

/// ScyllaDB: a table's `tablets` hints need a tablets keyspace.
fn check_tablet_hints(p: &Properties, ks: &Keyspace) -> Result<()> {
    if p.dialect().is_scylla() && p.has("tablets") && ks.tablets.is_none() {
        return Err(Error::config(
            "tablet options cannot be used when tablets are disabled for the keyspace",
        ));
    }
    Ok(())
}

fn lookup_table_for_alter<'a>(
    schema: &'a Schema,
    ks: &str,
    name: &str,
    d: Dialect,
) -> Result<Option<&'a Arc<Table>>> {
    let Some(k) = schema.keyspace(ks) else {
        return Err(match d {
            Dialect::Scylla => msg::ddl_unknown_keyspace(d, ks),
            Dialect::Cassandra => Error::invalid(format!("Keyspace '{ks}' doesn't exist")),
        });
    };
    if k.is_builtin() {
        return Err(system_unmodifiable(ks));
    }
    Ok(k.tables.get(name))
}

async fn alter_table(
    db: &Arc<Database>,
    schema: &Schema,
    a: &ast::AlterTable<'static>,
    ks_name: &str,
    d: Dialect,
    values: &[crate::response::Param],
) -> Result<Response> {
    let tname = a.name.name.name().to_string();
    // Cassandra validates the WITH properties when preparing the statement.
    if !d.is_scylla()
        && let AlterTableAction::With(props) = &a.action
    {
        let p = Properties::new(props.iter(), params::valid_table_options(d), d)?;
        params::apply(&mut TableParams::defaults(d, false), &p, false)?;
    }
    // `MaskColumn.validate`: masks are created only while enabled (dropped at any time).
    if let AlterTableAction::Mask { mask: Some(_), .. } = &a.action {
        crate::masking::ensure_enabled(db)?;
    }
    let Some(table) = lookup_table_for_alter(schema, ks_name, &tname, d)? else {
        if a.if_exists {
            return Ok(void(Vec::new()));
        }
        return Err(msg::alter_unknown_table(d, ks_name, &tname));
    };
    if table.is_view() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => {
                "Cannot use ALTER TABLE on Materialized View. (Did you mean ALTER MATERIALIZED VIEW)?"
            }
            Dialect::Cassandra => {
                "Cannot use ALTER TABLE on a materialized view; use ALTER MATERIALIZED VIEW instead"
            }
        }));
    }
    let ks = schema
        .keyspace(ks_name)
        .ok_or_else(|| msg::ddl_unknown_keyspace(d, ks_name))?;
    if d.is_scylla() {
        crate::cdc::check_log_alter(ks, table, &a.action)?;
    }
    let mut columns = table.columns.clone();
    let mut params_ = table.params.clone();
    let mut dropped = table.dropped.clone();
    let mut ids = table.column_ids.clone();
    let mut next_id = table.next_column_id;
    let mut changed = true;
    let mut masks = table.masks.clone();
    let mut renamed: Vec<(String, String)> = Vec::new();
    match &a.action {
        AlterTableAction::Mask {
            if_exists,
            column,
            mask,
        } => {
            let name = column.name().to_string();
            let Some(col) = columns.iter().find(|c| c.name == name) else {
                if *if_exists {
                    return Ok(void(Vec::new()));
                }
                return Err(Error::invalid(format!(
                    "Column with name '{name}' doesn't exist on table '{tname}'"
                )));
            };
            let new = match mask {
                Some(m) => Some(
                    crate::masking::prepare(db, schema, ks_name, &tname, &name, &col.ty, m)?.0,
                ),
                None => None,
            };
            if masks.get(&name) == new.as_ref() {
                return Ok(void(Vec::new()));
            }
            match new {
                Some(m) => masks.insert(name, m),
                None => masks.remove(&name),
            };
        }
        AlterTableAction::Add {
            if_not_exists,
            columns: adds,
            ..
        } => {
            let resolve = resolver(Some(ks));
            changed = false;
            for add in adds {
                let name = add.name.name().to_string();
                let ty = super::types::resolve(&add.ty, ks_name, &resolve, d)?;
                let mask = match &add.mask {
                    Some(m) => {
                        if d.is_scylla() {
                            return Err(Error::invalid("Column masking is not supported"));
                        }
                        Some(crate::masking::prepare(db, schema, ks_name, &tname, &name, &ty, m)?.0)
                    }
                    None => None,
                };
                if columns.iter().any(|c| c.name == name) {
                    if *if_not_exists {
                        continue;
                    }
                    return Err(msg::alter_add_existing(d, &name));
                }
                if add.is_static && table.clustering.is_empty() {
                    return Err(msg::static_without_clustering());
                }
                // ScyllaDB checks this in CREATE TABLE only.
                if let CqlType::Udt(def, false) = &ty
                    && def.fields.iter().any(|(_, f)| f.is_multicell())
                    && !d.is_scylla()
                {
                    return Err(Error::invalid(format!(
                        "Non-frozen UDTs with nested non-frozen collections are not supported for column {name}"
                    )));
                }
                if ty.is_counter() && !table.is_counter() {
                    return Err(msg::alter_add_counter_to_regular(d, ks_name, &tname, &name));
                }
                if !ty.is_counter() && table.is_counter() {
                    return Err(msg::alter_add_regular_to_counter(d, ks_name, &tname, &name));
                }
                if let Some(prev) = dropped.get(&name) {
                    if !ty.is_serialization_compatible_with(&prev.ty) {
                        return Err(Error::invalid(format!(
                            "Cannot add a column '{name}' of type {}, incompatible with previously dropped column '{name}' of type {}",
                            nosql_types::cql_name(&ty),
                            nosql_types::cql_name(&prev.ty)
                        )));
                    }
                    let kind = if add.is_static {
                        ColumnKind::Static
                    } else {
                        ColumnKind::Regular
                    };
                    if prev.kind != kind {
                        return Err(Error::invalid(format!(
                            "Cannot re-add previously dropped column '{name}' of kind {}, incompatible with previous kind {}",
                            kind.name().to_uppercase(),
                            prev.kind.name().to_uppercase()
                        )));
                    }
                    if table.is_counter() {
                        return Err(Error::invalid(format!(
                            "Cannot re-add previously dropped counter column {name}"
                        )));
                    }
                }
                let id = match ids.get(&name) {
                    Some(id) => *id,
                    None => {
                        let id = next_id;
                        next_id += 1;
                        ids.insert(name.clone(), id);
                        id
                    }
                };
                columns.push(Column {
                    name,
                    ty,
                    kind: if add.is_static {
                        ColumnKind::Static
                    } else {
                        ColumnKind::Regular
                    },
                    position: -1,
                    reversed: false,
                    id,
                });
                if let Some(m) = mask {
                    masks.insert(columns[columns.len() - 1].name.clone(), m);
                }
                changed = true;
            }
        }
        AlterTableAction::DropObject {
            if_exists,
            columns: drops,
            timestamp,
            ..
        } => {
            changed = false;
            let ts = match timestamp {
                Some(ast::Term::Bind(m)) => match values.get(m.index as usize) {
                    Some(crate::response::Param::Set(b)) => <[u8; 8]>::try_from(&b[..])
                        .map(i64::from_be_bytes)
                        .map_err(|_| Error::invalid("Invalid timestamp value"))?,
                    Some(_) => return Err(Error::invalid("Invalid null value of timestamp")),
                    None => {
                        return Err(Error::invalid(format!(
                            "Missing value for bind variable {}",
                            m.index
                        )));
                    }
                },
                Some(t) => {
                    let text = t.to_string();
                    text.trim()
                        .parse::<i64>()
                        .map_err(|_| Error::invalid(format!("Invalid timestamp {text}")))?
                }
                None => db.timestamp(),
            };
            for c in drops {
                let name = c.name().to_string();
                let Some(pos) = columns.iter().position(|x| x.name == name) else {
                    if *if_exists {
                        continue;
                    }
                    return Err(msg::alter_drop_unknown(d, ks_name, &tname, &name));
                };
                let col = &columns[pos];
                if col.is_primary_key() {
                    return Err(msg::alter_drop_pk(d, &name));
                }
                if !d.is_scylla() && col.ty.is_udt() && col.ty.is_multicell() {
                    return Err(Error::invalid(format!(
                        "Cannot drop non-frozen column {name} of user type {}",
                        nosql_types::cql_name(&col.ty)
                    )));
                }
                // ScyllaDB: the oldest view (index views included) with the column, or with it
                // as a virtual column.
                if d.is_scylla()
                    && let Some(v) = crate::views::views_of(ks, table)
                        .filter(|v| {
                            v.column(&name).is_some()
                                || crate::views::virtual_columns(table, v)
                                    .iter()
                                    .any(|(c, _)| *c == name)
                        })
                        .min_by(|a, b| nosql_types::compare_timeuuid(&a.id, &b.id))
                {
                    return Err(Error::invalid(format!(
                        "Cannot drop column {name} from base table {ks_name}.{tname}: materialized view {} needs this column",
                        v.name
                    )));
                }
                let dependent: Vec<&str> = table
                    .indexes
                    .iter()
                    .filter(|i| index_targets(i, &name))
                    .map(|i| i.name.as_str())
                    .collect();
                if !dependent.is_empty() {
                    return Err(Error::invalid(match d {
                        Dialect::Scylla => format!(
                            "Cannot drop column {name} from base table {ks_name}.{tname}: materialized view {}_index needs this column",
                            dependent[0]
                        ),
                        Dialect::Cassandra => format!(
                            "Cannot drop column {name} because it has dependent secondary indexes ({})",
                            dependent.join(", ")
                        ),
                    }));
                }
                // Cassandra refuses any drop from a table with views (ScyllaDB only the views
                // that need the column, above).
                if !d.is_scylla()
                    && crate::views::views_of(ks, table).next().is_some()
                {
                    return Err(Error::invalid(format!(
                        "Cannot drop column {name} on base table {tname} with materialized views"
                    )));
                }
                let col = columns.remove(pos);
                dropped.insert(
                    name.clone(),
                    DroppedColumn {
                        dropped_at: ts,
                        ty: col.ty.clone(),
                        kind: col.kind,
                    },
                );
                changed = true;
            }
        }
        AlterTableAction::Rename { if_exists, renames } => {
            changed = false;
            for (from, to) in renames {
                let (from, to) = (from.name().to_string(), to.name().to_string());
                let Some(pos) = columns.iter().position(|x| x.name == from) else {
                    if *if_exists {
                        continue;
                    }
                    return Err(msg::alter_rename_unknown(d, ks_name, &tname, &from));
                };
                if !columns[pos].is_primary_key() {
                    return Err(msg::alter_rename_regular(d, &from));
                }
                if columns.iter().any(|x| x.name == to) {
                    return Err(msg::alter_rename_to_existing(
                        d, ks_name, &tname, &from, &to,
                    ));
                }
                let dependent: Vec<&str> = table
                    .indexes
                    .iter()
                    .filter(|i| index_targets(i, &from))
                    .map(|i| i.name.as_str())
                    .collect();
                if !dependent.is_empty() {
                    return Err(Error::invalid(format!(
                        "{} rename column {from} because it has dependent secondary indexes ({})",
                        if d.is_scylla() { "Cannot" } else { "Can't" },
                        dependent.join(", ")
                    )));
                }
                let id = columns[pos].id;
                columns[pos].name = to.clone();
                ids.remove(&from);
                renamed.push((from, to.clone()));
                ids.insert(to, id);
                changed = true;
            }
        }
        AlterTableAction::AlterType { column, ty } => {
            let name = column.name().to_string();
            let Some(pos) = columns.iter().position(|x| x.name == name) else {
                return Err(match d {
                    Dialect::Scylla => {
                        Error::invalid(format!("Column {name} was not found in table {tname}"))
                    }
                    Dialect::Cassandra => {
                        Error::invalid("Altering column types is no longer supported")
                    }
                });
            };
            let resolve = resolver(Some(ks));
            let new_ty = super::types::resolve(ty, ks_name, &resolve, d)?;
            let old = &columns[pos];
            let ok = d.is_scylla()
                && if old.kind == ColumnKind::Clustering {
                    new_ty.is_compatible_with(&old.ty)
                } else {
                    new_ty.is_value_compatible_with(&old.ty)
                };
            if !ok {
                return Err(msg::alter_column_type(
                    d,
                    &name,
                    &nosql_types::cql_name(&old.ty),
                    &nosql_types::cql_name(&new_ty),
                ));
            }
            columns[pos].ty = new_ty;
        }
        AlterTableAction::With(props) => {
            let p = Properties::new(props.iter(), params::valid_table_options(d), d)?;
            if p.has("id") {
                return Err(Error::config("Cannot alter table id."));
            }
            params::apply(&mut params_, &p, false)?;
            check_tablet_hints(&p, ks)?;
            // A CDC table's tablets split or merge: a new stream generation.
            if d.is_scylla() && p.has("tablets") {
                crate::cdc::note_tablet_resize(ks, table, &mut params_, db.timestamp() / 1000);
            }
            if table.is_counter() && params_.default_time_to_live > 0 {
                return Err(msg::counter_with_ttl());
            }
            // `AlterTableStatement.AlterOptions`.
            if !d.is_scylla()
                && params_.gc_grace_seconds == 0
                && crate::views::views_of(ks, table).next().is_some()
            {
                return Err(Error::invalid(
                    "Cannot alter gc_grace_seconds of the base table of a materialized view to 0, since this value is used to TTL undelivered updates. Setting gc_grace_seconds too low might cause undelivered updates to expire before being replayed.",
                ));
            }
        }
        AlterTableAction::DropCompactStorage => {
            return Err(Error::invalid(
                "DROP COMPACT STORAGE is disabled. Enable in cassandra.yaml to use.",
            ));
        }
        AlterTableAction::Ttl(target) => {
            params_ = crate::rowttl::alter(table, &columns, target.as_ref())?;
        }
    }
    if !changed {
        // `ADD IF NOT EXISTS` / `DROP IF EXISTS` with nothing to do.
        return Ok(void(Vec::new()));
    }
    // Dropping the per-row TTL column turns per-row TTL off.
    if let Some(c) = crate::rowttl::tagged(&params_)
        && table.column(&c).is_some()
        && !columns.iter().any(|x| x.name == c)
    {
        crate::rowttl::set_tag(&mut params_, None);
    }
    let renamed_views = renamed.clone();
    for (from, to) in renamed {
        if let Some(m) = masks.remove(&from) {
            masks.insert(to, m);
        }
    }
    let mut t = table.rebuild(columns, params_);
    t.masks = masks
        .into_iter()
        .filter(|(c, _)| t.by_name.contains_key(c))
        .collect();
    t.dropped = dropped;
    t.column_ids = ids;
    t.next_column_id = t.next_column_id.max(next_id);
    if d.is_scylla() {
        crate::cdc::check_vector_alter(&t)?;
    }
    let t = Arc::new(t);
    let mut new_ks = (**ks).clone();
    new_ks.tables.insert(tname.clone(), t.clone());
    // `SELECT *` views follow the base's new columns, Cassandra views its renamed key columns.
    let mut attach = vec![t.clone()];
    attach.extend(crate::views::follow_base(&mut new_ks, &t));
    if !d.is_scylla() && !renamed_views.is_empty() {
        attach.extend(crate::views::follow_rename(&mut new_ks, &t, &renamed_views));
    }
    crate::masking::sync_views(&mut new_ks, &t);
    // ScyllaDB CDC: enabling creates the log, which follows the base's columns (disabling keeps
    // it).
    if d.is_scylla() {
        let log_name = crate::cdc::log_name(&tname);
        match new_ks.tables.get(&log_name).cloned() {
            // A log whose base has CDC disabled is detached: it catches up when re-enabled.
            Some(log) if crate::cdc::is_log(&new_ks, &log) && crate::cdc::active(&t) => {
                let at = t
                    .dropped
                    .iter()
                    .filter(|(n, dc)| table.dropped.get(*n) != Some(*dc))
                    .map(|(_, dc)| dc.dropped_at)
                    .max()
                    .unwrap_or_else(|| db.timestamp());
                if let Some(l) = crate::cdc::follow_base(&log, &t, d, at)? {
                    let l = Arc::new(l);
                    new_ks.tables.insert(log_name, l.clone());
                    attach.push(l);
                }
            }
            Some(_) if crate::cdc::enabled(&t) => {
                return Err(crate::cdc::log_name_taken(ks_name, &tname));
            }
            None if crate::cdc::enabled(&t) => {
                let log = Arc::new(crate::cdc::log_table(&t, d)?);
                new_ks.tables.insert(log_name, log.clone());
                attach.push(log);
            }
            _ => {}
        }
    }
    let change = Change {
        keyspace: Some(new_ks),
        attach,
        events: vec![SchemaChange::object(
            ChangeType::Updated,
            ChangeTarget::Table,
            ks_name,
            &tname,
        )],
        ..Default::default()
    };
    commit(db, schema, change).await
}

/// Whether an index's target names `column` (`col`, `keys(col)`, `"Quoted"`, …).
fn index_targets(i: &super::IndexMeta, column: &str) -> bool {
    let Some(target) = i.options.get("target") else {
        return false;
    };
    let inner = target
        .find('(')
        .and_then(|s| target.strip_suffix(')').map(|t| &t[s + 1..]))
        .unwrap_or(target);
    let unquoted = inner.trim_matches('"');
    unquoted == column || target.contains(&format!("\"{column}\""))
}

async fn drop_table(
    db: &Arc<Database>,
    schema: &Schema,
    ks_name: &str,
    tname: &str,
    if_exists: bool,
    d: Dialect,
) -> Result<Response> {
    let ks = match schema.keyspace(ks_name) {
        Some(k) => k,
        None => {
            if if_exists {
                return Ok(void(Vec::new()));
            }
            // Both references name the table (Cassandra's `DropTableStatement` looks the
            // table up, not the keyspace).
            return Err(msg::drop_unknown_table(d, ks_name, tname));
        }
    };
    if ks.is_builtin() {
        return Err(system_unmodifiable(ks_name));
    }
    let Some(table) = ks.tables.get(tname) else {
        if if_exists {
            return Ok(void(Vec::new()));
        }
        return Err(msg::drop_unknown_table(d, ks_name, tname));
    };
    if table.is_view() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => {
                "Cannot use DROP TABLE on Materialized View. (Did you mean DROP MATERIALIZED VIEW)?"
            }
            Dialect::Cassandra => {
                "Cannot use DROP TABLE on a materialized view. Please use DROP MATERIALIZED VIEW instead."
            }
        }));
    }
    if d.is_scylla() && crate::cdc::is_log(ks, table) {
        return Err(Error::unauthorized(format!(
            "Cannot DROP cdc log table {tname}"
        )));
    }
    let mut view_tables: Vec<&Arc<Table>> = ks
        .tables
        .values()
        .filter(|t| {
            t.view.as_ref().is_some_and(|v| v.base_table_id == table.id)
                && !t.name.ends_with("_index")
        })
        .collect();
    // ScyllaDB lists them in creation order.
    if d.is_scylla() {
        view_tables.sort_by(|a, b| nosql_types::compare_timeuuid(&a.id, &b.id));
    }
    let views: Vec<&str> = view_tables.iter().map(|t| t.name.as_str()).collect();
    if !views.is_empty() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!(
                "Cannot drop table when materialized views still depend on it ({ks_name}.{{{}}})",
                views.join(", ")
            ),
            // (sic: `DropTableStatement` prints the keyspace name.)
            Dialect::Cassandra => format!(
                "Cannot drop a table when materialized views still depend on it ({ks_name})"
            ),
        }));
    }
    let mut new_ks = (**ks).clone();
    new_ks.tables.remove(tname);
    // Index views of the table go with it.
    let deps: Vec<String> = new_ks
        .tables
        .values()
        .filter(|t| t.view.as_ref().is_some_and(|v| v.base_table_id == table.id))
        .map(|t| t.name.clone())
        .collect();
    let mut drop_tables = vec![table.table_id()];
    drop_tables.extend(crate::index::hidden_tables(table));
    // The CDC log goes with its base.
    if d.is_scylla()
        && let Some(log) = crate::cdc::log_of(&new_ks, tname).cloned()
    {
        new_ks.tables.remove(&log.name);
        drop_tables.push(log.table_id());
    }
    for dep in deps {
        if let Some(t) = new_ks.tables.remove(&dep) {
            drop_tables.push(t.table_id());
        }
    }
    let change = Change {
        keyspace: Some(new_ks),
        drop_tables,
        events: vec![SchemaChange::object(
            ChangeType::Dropped,
            ChangeTarget::Table,
            ks_name,
            tname,
        )],
        ..Default::default()
    };
    commit(db, schema, change).await
}

/// TRUNCATE.
/// Boxed so the future is checked `Send` here, with named lifetimes (rust-lang/rust#110338).
pub fn truncate<'a>(
    db: &'a Arc<Database>,
    ks_name: &'a str,
    tname: &'a str,
) -> futures::future::BoxFuture<'a, Result<Response>> {
    Box::pin(truncate_inner(db, ks_name, tname, false))
}

/// TRUNCATE with its `USING TIMEOUT` (ScyllaDB): validated, and a zero timeout fails it.
pub fn truncate_using<'a>(
    db: &'a Arc<Database>,
    ks_name: &'a str,
    tname: &'a str,
    using: &'a UsingClause<'static>,
    values: &'a [crate::response::Param],
) -> futures::future::BoxFuture<'a, Result<Response>> {
    Box::pin(async move {
        let t = truncate_timeout(using, values, db.dialect())?;
        truncate_inner(db, ks_name, tname, no_time(t.as_ref())).await
    })
}

async fn truncate_inner(
    db: &Arc<Database>,
    ks_name: &str,
    tname: &str,
    no_time: bool,
) -> Result<Response> {
    let d = db.dialect();
    let schema = db.schema();
    let Some(ks) = schema.keyspace(ks_name) else {
        return Err(msg::keyspace_does_not_exist(d, ks_name));
    };
    let Some(table) = ks.tables.get(tname) else {
        return Err(msg::unconfigured_table(d, tname));
    };
    if table.is_view() {
        return Err(Error::invalid(
            "Cannot TRUNCATE materialized view directly; must truncate base table instead",
        ));
    }
    if table.kind != TableKind::Regular {
        return Err(system_unmodifiable(ks_name));
    }
    if no_time {
        return Err(Error::new(
            crate::error::ErrorCode::Truncate,
            format!(
                "Error during truncate: std::runtime_error (Timeout during TRUNCATE TABLE of {ks_name}.{tname})"
            ),
        ));
    }
    db.store
        .truncate(table.table_id())
        .await
        .map_err(Error::internal)?;
    crate::index::truncate_hidden(db, table).await?;
    for t in ks.tables.values() {
        if t.view.as_ref().is_some_and(|v| v.base_table_id == table.id) {
            db.store
                .truncate(t.table_id())
                .await
                .map_err(Error::internal)?;
        }
    }
    Ok(Response::void())
}

// ---- user types ------------------------------------------------------------------------------

async fn create_type(
    db: &Arc<Database>,
    schema: &Schema,
    c: &ast::CreateType<'static>,
    ks_name: &str,
    d: Dialect,
) -> Result<Response> {
    let name = c.name.name.name().to_string();
    let Some(ks) = schema.keyspace(ks_name) else {
        return Err(msg::ddl_unknown_keyspace(d, ks_name));
    };
    if ks.is_builtin() {
        return Err(system_unmodifiable(ks_name));
    }
    if ks.types.contains_key(&name) {
        if c.if_not_exists {
            return Ok(if d.is_scylla() {
                announce(
                    SchemaChange::object(ChangeType::Created, ChangeTarget::Type, ks_name, &name),
                    Vec::new(),
                )
            } else {
                void(Vec::new())
            });
        }
        return Err(msg::type_exists(d, ks_name, &name));
    }
    let resolve = resolver(Some(ks));
    let mut fields: Vec<(String, CqlType)> = Vec::new();
    for (f, raw) in &c.fields {
        let fname = f.name().to_string();
        if fields.iter().any(|(n, _)| n == &fname) {
            return Err(Error::invalid(match d {
                Dialect::Scylla => format!("Duplicate field name {fname} in type {name}"),
                Dialect::Cassandra => format!("Duplicate field name '{fname}' in type '{name}'"),
            }));
        }
        let ty = super::types::resolve(raw, ks_name, &resolve, d)?;
        if ty.is_counter() {
            return Err(Error::invalid("A user type cannot contain counters"));
        }
        if ty.is_udt() && ty.is_multicell() {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "A user type cannot contain non-frozen user type fields",
                Dialect::Cassandra => "A user type cannot contain non-frozen UDTs",
            }));
        }
        // Fields keep their declared types (`system_schema.types` shows `list<int>`); a value
        // of the type holds its collections whole.
        fields.push((fname, ty));
    }
    let mut new_ks = (**ks).clone();
    new_ks
        .types
        .insert(name.clone(), Arc::new(UdtDef::new(ks_name, &name, fields)));
    let change = Change {
        keyspace: Some(new_ks),
        events: vec![SchemaChange::object(
            ChangeType::Created,
            ChangeTarget::Type,
            ks_name,
            &name,
        )],
        ..Default::default()
    };
    commit(db, schema, change).await
}

/// The tables (in creation order) with a partition key column of type `name`, and that column.
fn partition_key_users<'k>(
    ks: &'k Keyspace,
    ks_name: &str,
    name: &str,
) -> Vec<(&'k Arc<Table>, &'k Column)> {
    let mut users: Vec<(&Arc<Table>, &Column)> = ks
        .tables
        .values()
        .filter_map(|tb| {
            tb.partition_key_columns()
                .find(|c| c.ty.references_udt(ks_name, name))
                .map(|c| (tb, c))
        })
        .collect();
    users.sort_by(|a, b| nosql_types::compare_timeuuid(&a.0.id, &b.0.id));
    users
}

/// ScyllaDB names the most recently created such table.
fn scylla_type_in_partition_key(ks_name: &str, name: &str, table: &Table, col: &Column) -> String {
    format!(
        "Cannot add new field to type {ks_name}.{name} because it is used in the partition key column {} of table {ks_name}.{}",
        col.name, table.name
    )
}

async fn alter_type(
    db: &Arc<Database>,
    schema: &Schema,
    a: &ast::AlterType<'static>,
    ks_name: &str,
    d: Dialect,
) -> Result<Response> {
    let name = a.name.name.name().to_string();
    let Some(ks) = schema.keyspace(ks_name) else {
        return Err(msg::ddl_unknown_keyspace(d, ks_name));
    };
    let Some(def) = ks.types.get(&name) else {
        if a.if_exists {
            return Ok(void(Vec::new()));
        }
        return Err(msg::unknown_type_alter(d, ks_name, &name));
    };
    let mut fields = def.fields.clone();
    let resolve = resolver(Some(ks));
    match &a.action {
        AlterTypeAction::Add {
            if_not_exists,
            field,
            ty,
        } => {
            let f = field.name().to_string();
            if fields.iter().any(|(n, _)| n == &f) {
                if *if_not_exists {
                    return Ok(void(Vec::new()));
                }
                return Err(msg::alter_type_add_existing(d, ks_name, &name, &f));
            }
            let t = super::types::resolve(ty, ks_name, &resolve, d)?;
            if t.is_counter() {
                return Err(Error::invalid("A user type cannot contain counters"));
            }
            // ScyllaDB refuses a non-frozen UDT field in CREATE TYPE only.
            if t.is_udt() && t.is_multicell() && !d.is_scylla() {
                return Err(Error::invalid("A user type cannot contain non-frozen UDTs"));
            }
            if t.references_udt(ks_name, &name) {
                return Err(Error::invalid(format!(
                    "Cannot add new field {f} of type {} to user type {ks_name}.{name} as it would create a circular reference",
                    nosql_types::cql_name(&t)
                )));
            }
            // A partition key's values (and tokens) would change (`AlterTypeStatement.AddField`).
            let users = partition_key_users(ks, ks_name, &name);
            if let Some((last, col)) = users.last() {
                return Err(Error::invalid(match d {
                    Dialect::Scylla => scylla_type_in_partition_key(ks_name, &name, last, col),
                    Dialect::Cassandra => {
                        let names: Vec<String> = users
                            .iter()
                            .map(|(tb, _)| format!("{ks_name}.{}", tb.name))
                            .collect();
                        format!(
                            "Cannot add new field {f} of type {} to user type {ks_name}.{name} as the type is being used in partition key by the following tables: {}",
                            nosql_types::cql_name(&t),
                            names.join(", ")
                        )
                    }
                }));
            }
            fields.push((f, t));
        }
        AlterTypeAction::Rename { if_exists, renames } => {
            // ScyllaDB refuses renames there too, with the same message.
            if d.is_scylla()
                && let Some((last, col)) = partition_key_users(ks, ks_name, &name).last()
            {
                return Err(Error::invalid(scylla_type_in_partition_key(
                    ks_name, &name, last, col,
                )));
            }
            for (from, to) in renames {
                let (from, to) = (from.name().to_string(), to.name().to_string());
                let Some(pos) = fields.iter().position(|(n, _)| n == &from) else {
                    if *if_exists {
                        continue;
                    }
                    return Err(msg::alter_type_unknown_field(d, ks_name, &name, &from));
                };
                if fields.iter().any(|(n, _)| n == &to) {
                    return Err(Error::invalid(format!(
                        "Duplicate field name {to} in type {ks_name}.{name}"
                    )));
                }
                fields[pos].0 = to;
            }
        }
        AlterTypeAction::AlterField { field, ty } => {
            let f = field.name().to_string();
            let Some(pos) = fields.iter().position(|(n, _)| n == &f) else {
                return Err(msg::alter_type_unknown_field(d, ks_name, &name, &f));
            };
            let t = super::types::resolve(ty, ks_name, &resolve, d)?;
            if !d.is_scylla() || !t.is_compatible_with(&fields[pos].1) {
                return Err(msg::alter_type_field_type(
                    d,
                    ks_name,
                    &name,
                    &f,
                    &nosql_types::cql_name(&fields[pos].1),
                    &nosql_types::cql_name(&t),
                ));
            }
            fields[pos].1 = t;
        }
    }
    let new_def = Arc::new(UdtDef::new(ks_name, &name, fields));
    let mut new_ks = (**ks).clone();
    new_ks.types.insert(name.clone(), new_def.clone());
    // Types and tables that use it see the new definition.
    for (n, other) in ks.types.iter() {
        if n != &name
            && other
                .fields
                .iter()
                .any(|(_, t)| t.references_udt(ks_name, &name))
        {
            let fs = other
                .fields
                .iter()
                .map(|(fname, t)| (fname.clone(), t.with_updated_udt(&new_def)))
                .collect();
            new_ks
                .types
                .insert(n.clone(), Arc::new(UdtDef::new(ks_name, n, fs)));
        }
    }
    let mut attach = Vec::new();
    let mut events = vec![SchemaChange::object(
        ChangeType::Updated,
        ChangeTarget::Type,
        ks_name,
        &name,
    )];
    for (tn, t) in ks.tables.iter() {
        if t.columns
            .iter()
            .any(|c| c.ty.references_udt(ks_name, &name))
        {
            let cols = t
                .columns
                .iter()
                .map(|c| Column {
                    ty: c.ty.with_updated_udt(&new_def),
                    ..c.clone()
                })
                .collect();
            let nt = Arc::new(t.rebuild(cols, t.params.clone()));
            new_ks.tables.insert(tn.clone(), nt.clone());
            attach.push(nt);
            events.push(SchemaChange::object(
                ChangeType::Updated,
                ChangeTarget::Table,
                ks_name,
                tn,
            ));
        }
    }
    let result = events.first().cloned();
    let change = Change {
        keyspace: Some(new_ks),
        attach,
        events,
        result,
        ..Default::default()
    };
    commit(db, schema, change).await
}

async fn drop_type(
    db: &Arc<Database>,
    schema: &Schema,
    ks_name: &str,
    name: &str,
    if_exists: bool,
    d: Dialect,
) -> Result<Response> {
    let Some(ks) = schema.keyspace(ks_name) else {
        if if_exists {
            return Ok(void(Vec::new()));
        }
        return Err(msg::ddl_unknown_keyspace(d, ks_name));
    };
    if !ks.types.contains_key(name) {
        if if_exists {
            return Ok(void(Vec::new()));
        }
        return Err(msg::unknown_type_drop(d, ks_name, name));
    }
    // Functions first (`DropTypeStatement`; ScyllaDB's wording verified by `captured_udf`).
    let functions = crate::udf::ddl::functions_using_type(ks, name);
    if !functions.is_empty() {
        return Err(match d {
            Dialect::Scylla => Error::invalid(format!(
                "Cannot drop user type {ks_name}.{name} as it is still used by function {}",
                functions[0]
            )),
            Dialect::Cassandra => {
                crate::udf::messages::type_used_by_functions(ks_name, name, &functions)
            }
        });
    }
    let users: Vec<&str> = ks
        .types
        .iter()
        .filter(|(n, t)| {
            n.as_str() != name
                && t.fields
                    .iter()
                    .any(|(_, f)| f.references_udt(ks_name, name))
        })
        .map(|(n, _)| n.as_str())
        .collect();
    if !users.is_empty() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!(
                "Cannot drop user type {ks_name}.{name} as it is still used by user type {}",
                users[0]
            ),
            Dialect::Cassandra => format!(
                "Cannot drop user type '{ks_name}.{name}' as it is still used by user types {}",
                users.join(", ")
            ),
        }));
    }
    let tables = ks.tables_using_type(name);
    if !tables.is_empty() {
        return Err(msg::type_in_use(d, ks_name, name, &tables));
    }
    let mut new_ks = (**ks).clone();
    new_ks.types.remove(name);
    let change = Change {
        keyspace: Some(new_ks),
        events: vec![SchemaChange::object(
            ChangeType::Dropped,
            ChangeTarget::Type,
            ks_name,
            name,
        )],
        ..Default::default()
    };
    commit(db, schema, change).await
}

// ---- USING TIMEOUT ---------------------------------------------------------------------------

/// ScyllaDB's checks of a timeout, in its order: whole milliseconds only, not negative.
fn duration_check(dur: &nosql_types::Duration) -> Result<()> {
    if dur.months != 0 || dur.days != 0 {
        return Err(Error::invalid(
            "Timeout values cannot be expressed in days/months",
        ));
    }
    if dur.nanos % 1_000_000 != 0 {
        return Err(Error::invalid(
            "Timeout values cannot have granularity finer than milliseconds",
        ));
    }
    if dur.nanos < 0 {
        return Err(Error::invalid("Timeout values must be non-negative"));
    }
    Ok(())
}

/// Validates a literal `USING TIMEOUT` (ScyllaDB).
pub fn check_timeout(using: &UsingClause, d: Dialect) -> Result<()> {
    let (lit, negated) = match &using.timeout {
        Some(ast::Term::Literal(l)) => (l, false),
        Some(ast::Term::Negate(inner)) => match inner.as_ref() {
            ast::Term::Literal(l) => (l, true),
            _ => return Ok(()),
        },
        _ => return Ok(()),
    };
    if let Ok(dur) = nosql_types::Duration::parse(&lit.text, d) {
        let dur = if negated {
            nosql_types::Duration::new(-dur.months, -dur.days, -dur.nanos)
        } else {
            dur
        };
        duration_check(&dur)?;
    }
    Ok(())
}

/// Validates a bound `USING TIMEOUT` value; returns it.
pub fn check_timeout_value(t: &Term, cx: &EvalCtx) -> Result<Option<nosql_types::Duration>> {
    match t.eval(cx)? {
        Bound::Val(b) => {
            if let Ok(nosql_types::Value::Duration(dur)) =
                nosql_types::decode(&CqlType::Duration, &b)
            {
                duration_check(&dur)?;
                return Ok(Some(dur));
            }
            Ok(None)
        }
        Bound::Null => Err(Error::invalid("Timeout value cannot be null")),
        Bound::Unset => Ok(None),
    }
}

/// The `USING TIMEOUT` of a TRUNCATE (a literal or a bind marker), validated.
fn truncate_timeout(
    using: &UsingClause,
    values: &[crate::response::Param],
    d: Dialect,
) -> Result<Option<nosql_types::Duration>> {
    check_timeout(using, d)?;
    match &using.timeout {
        Some(ast::Term::Literal(l)) => Ok(nosql_types::Duration::parse(&l.text, d).ok()),
        Some(ast::Term::Bind(m)) => match values.get(m.index as usize) {
            Some(crate::response::Param::Set(b)) => {
                let dur = nosql_types::Duration::decode(b)
                    .ok_or_else(|| Error::invalid("Invalid duration value"))?;
                duration_check(&dur)?;
                Ok(Some(dur))
            }
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// Whether a timeout leaves no time at all: the write times out before any replica answers
/// (roll answers faster than any other timeout).
pub fn no_time(t: Option<&nosql_types::Duration>) -> bool {
    t.is_some_and(|d| d.months == 0 && d.days == 0 && d.nanos == 0)
}

/// The write timeout of a write that had no time (`table`: `ks.t`).
pub fn write_timed_out(table: &str, cl: crate::error::Consistency, write_type: &str) -> Error {
    write_timed_out_in(Dialect::Scylla, table, cl, write_type)
}

/// A write timeout as `dialect` words it: ScyllaDB names the table and the consistency,
/// Cassandra only the responses (`RequestTimeoutException`).
pub fn write_timed_out_in(
    dialect: Dialect,
    table: &str,
    cl: crate::error::Consistency,
    write_type: &str,
) -> Error {
    let message = if dialect.is_scylla() {
        format!(
            "Operation timed out for {table} - received only 0 responses from 1 CL={}.",
            cl.name()
        )
    } else {
        "Operation timed out - received only 0 responses.".to_string()
    };
    Error {
        detail: crate::error::ErrorDetail::WriteTimeout {
            consistency: cl,
            received: 0,
            block_for: 1,
            write_type: write_type.to_string(),
            contentions: None,
        },
        ..Error::new(crate::error::ErrorCode::WriteTimeout, message)
    }
}

#[allow(dead_code)]
fn property_text(p: &PropertyValue) -> String {
    p.to_string()
}

#[allow(dead_code)]
fn unused(_: Replication) {}
