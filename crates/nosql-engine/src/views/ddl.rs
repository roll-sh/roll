//! `CREATE` / `ALTER` / `DROP MATERIALIZED VIEW`.
//!
//! Cassandra's rules are ported from `CreateViewStatement`, `AlterViewStatement` and
//! `DropViewStatement` (Apache-2.0), in their order; Cassandra refuses views unless
//! `materialized_views_enabled`. ScyllaDB's rules and messages were learned from the reference.
//! A new view is backfilled from its base before the statement answers.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::future::BoxFuture;
use nosql_cql::ast::{
    self, Direction, Relation, Selectable, Statement, StatementKind, TableOption,
};
use nosql_types::Dialect;

use super::java::{HashOrder, identifier_hash};
use super::msg as vmsg;
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::{Access, StatementHandler};
use crate::messages as msg;
use crate::prepare::PrepareCtx;
use crate::prepare::restrictions::{self, StmtKind};
use crate::response::{ChangeTarget, ChangeType, QueryParams, Response, SchemaChange};
use crate::schema::ddl::{Change, commit};
use crate::schema::params::{self, Properties, TableParams};
use crate::schema::{Column, ColumnKind, Table, TableFlags, TableKind, ViewMeta};
use crate::session::SessionState;

/// Executes the materialized view statements.
pub struct Handler;

impl StatementHandler for Handler {
    fn handles(&self, stmt: &StatementKind<'_>) -> bool {
        matches!(
            stmt,
            StatementKind::CreateView(_) | StatementKind::AlterView(_) | StatementKind::DropView(_)
        )
    }

    fn execute<'a>(
        &'a self,
        db: &'a Arc<Database>,
        session: &'a mut SessionState,
        stmt: &'a Statement<'static>,
        params: &'a QueryParams,
    ) -> BoxFuture<'a, Result<Response>> {
        Box::pin(async move {
            let current = params.keyspace.clone().or_else(|| session.keyspace.clone());
            match &stmt.kind {
                StatementKind::CreateView(c) => {
                    create(db, session, stmt, c, current.as_deref()).await
                }
                StatementKind::AlterView(a) => alter(db, session, a, current.as_deref()).await,
                StatementKind::DropView(x) => drop_view(db, session, x, current.as_deref()).await,
                other => Err(crate::hooks::unsupported(other)),
            }
        })
    }
}

fn keyspace_of(name: &ast::QualifiedName, current: Option<&str>) -> Result<String> {
    match (&name.keyspace, current) {
        (Some(k), _) => Ok(k.name().to_string()),
        (None, Some(k)) => Ok(k.to_string()),
        (None, None) => Err(msg::no_keyspace()),
    }
}

/// The `where_clause` a reference stores: ScyllaDB re-renders each relation (`IS NOT null`),
/// Cassandra prints the parsed clause.
pub fn where_text(d: Dialect, rels: &[Relation]) -> String {
    let parts: Vec<String> = rels
        .iter()
        .map(|r| {
            let s = r.to_string();
            match d {
                Dialect::Scylla => s
                    .replace(" IS NOT NULL", " IS NOT null")
                    .replace("TOKEN(", "token("),
                Dialect::Cassandra => s,
            }
        })
        .collect();
    parts.join(" AND ")
}

/// A Java object as `Object.toString` prints it: class name and an identity hash (which
/// varies by run on the reference too).
fn java_object(class: &str) -> String {
    format!(
        "org.apache.cassandra.cql3.selection.{class}@{:x}",
        rand::random::<u32>() >> 1
    )
}

/// The `Selectable.Raw` class Cassandra's parser builds for a selector.
fn raw_class(s: &Selectable) -> &'static str {
    match s {
        Selectable::Column(_) => "Selectable$RawIdentifier",
        Selectable::Term(_) => "Selectable$WithTerm$Raw",
        Selectable::CountRows
        | Selectable::Function(..)
        | Selectable::Binary(_)
        | Selectable::Negate(_) => "Selectable$WithFunction$Raw",
        Selectable::Metadata(..) => "Selectable$WritetimeOrTTL$Raw",
        Selectable::Cast(_) => "Selectable$WithCast$Raw",
        Selectable::Field(..) => "Selectable$WithFieldSelection$Raw",
        Selectable::Element(..) => "Selectable$WithElementSelection$Raw",
        Selectable::Slice(_) => "Selectable$WithSliceSelection$Raw",
        Selectable::TypeHint(..) => "Selectable$WithTypeHint$Raw",
        Selectable::List(_) => "Selectable$WithArrayLiteral$Raw",
        Selectable::Set(_) => "Selectable$WithSet$Raw",
        Selectable::MapOrUdt(_) => "Selectable$WithMapOrUdt$Raw",
        Selectable::Tuple(_) => "Selectable$BetweenParenthesesOrWithTuple$Raw",
    }
}

/// The base column a selector names (`None`: not a plain column).
fn selected_column(s: &ast::Selector) -> Option<String> {
    match &s.expr {
        Selectable::Column(id) => Some(id.name().to_string()),
        _ => None,
    }
}

async fn create(
    db: &Arc<Database>,
    session: &SessionState,
    stmt: &Statement<'static>,
    c: &ast::CreateView<'static>,
    current: Option<&str>,
) -> Result<Response> {
    let d = db.dialect();
    let ks_name = keyspace_of(&c.name, current)?;
    let base_ks = match &c.base.keyspace {
        Some(k) => k.name().to_string(),
        None => ks_name.clone(),
    };
    let view_name = c.name.name.name().to_string();
    let base_name = c.base.name.name().to_string();
    db.hooks().authorizer.authorize(
        db,
        session,
        &Access::Schema {
            keyspace: &base_ks,
            table: &base_name,
            statement: "CREATE MATERIALIZED VIEW",
        },
    )?;
    if !stmt.bind_markers.is_empty() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => "Cannot use query parameters in CREATE MATERIALIZED VIEW statements",
            Dialect::Cassandra => {
                "Bind variables are not allowed in CREATE MATERIALIZED VIEW statements"
            }
        }));
    }
    if base_ks != ks_name {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!(
                "Cannot create a materialized view on a table in a separate keyspace ('{base_ks}' != '{ks_name}')"
            ),
            Dialect::Cassandra => {
                "Cannot create a materialized view on a table in a different keyspace".to_string()
            }
        }));
    }
    if !d.is_scylla() && !db.settings().materialized_views_enabled {
        return Err(Error::invalid(
            "Materialized views are disabled. Enable in cassandra.yaml to use.",
        ));
    }
    let _g = db.ddl.lock().await;
    let schema = db.schema();
    let Some(ks) = schema.keyspace(&ks_name) else {
        return Err(match d {
            Dialect::Scylla => msg::ddl_unknown_keyspace(d, &ks_name),
            Dialect::Cassandra => Error::invalid(format!("Keyspace '{ks_name}' doesn't exist")),
        });
    };
    if ks.is_builtin() {
        return Err(Error::unauthorized(format!(
            "{ks_name} keyspace is not user-modifiable."
        )));
    }
    // ScyllaDB checks the id, the storage options and the name before the base table.
    let scylla_id = if d.is_scylla() {
        scylla_checks(&schema, c, &view_name)?
    } else {
        None
    };
    let Some(base) = ks
        .tables
        .get(&base_name)
        .filter(|t| !t.is_view() || d.is_scylla())
        .cloned()
    else {
        return Err(match d {
            Dialect::Scylla => msg::unconfigured_table(d, &base_name),
            Dialect::Cassandra => Error::invalid(format!("Base table '{base_name}' doesn't exist")),
        });
    };
    let tablets_warning = || -> Vec<String> {
        match d {
            Dialect::Scylla if ks.tablets.is_some() => vec![vmsg::TABLETS_VIEW_WARNING.to_string()],
            Dialect::Scylla => Vec::new(),
            Dialect::Cassandra => vec![vmsg::VIEW_WARNING.to_string()],
        }
    };
    // ScyllaDB validates the statement before looking for an existing view.
    let exists = |ks: &crate::schema::Keyspace| -> Option<Result<Response>> {
        let existing = ks.tables.get(&view_name)?;
        if c.if_not_exists && existing.is_view() {
            return Some(Ok(Response::new(
                crate::response::ResponseBody::SchemaChange(SchemaChange::object(
                    ChangeType::Created,
                    ChangeTarget::Table,
                    &ks_name,
                    &view_name,
                )),
            )
            .with_warnings(tablets_warning())));
        }
        Some(Err(msg::table_exists(&ks_name, &view_name)))
    };
    if let Some(existing) = ks.tables.get(&view_name).filter(|_| !d.is_scylla()) {
        if !existing.is_view() && !d.is_scylla() {
            return Err(Error::invalid(format!(
                "Cannot create materialized view '{view_name}' - a table with the same name already exists"
            )));
        }
        if c.if_not_exists && existing.is_view() {
            // ScyllaDB announces the (unchanged) view.
            return Ok(if d.is_scylla() {
                Response::new(crate::response::ResponseBody::SchemaChange(
                    SchemaChange::object(
                        ChangeType::Created,
                        ChangeTarget::Table,
                        &ks_name,
                        &view_name,
                    ),
                ))
                .with_warnings(tablets_warning())
            } else {
                // `CreateViewStatement.clientWarnings` warns whatever the statement does.
                Response::void().with_warnings(tablets_warning())
            });
        }
        return Err(msg::table_exists(&ks_name, &view_name));
    }
    if base.is_counter() {
        return Err(Error::invalid(
            "Materialized views are not supported on counter tables",
        ));
    }
    if base.is_view() {
        return Err(Error::invalid(
            "Materialized views cannot be created against other materialized views",
        ));
    }
    if !d.is_scylla() && base.params.gc_grace_seconds == 0 {
        return Err(Error::invalid(format!(
            "Cannot create materialized view '{view_name}' for base table '{base_name}' with gc_grace_seconds of 0, since this value is used to TTL undelivered updates. Setting gc_grace_seconds too low might cause undelivered updates to expire before being replayed."
        )));
    }
    // SELECT clause.
    let include_all = c.selectors.is_empty();
    let mut selected: Vec<usize> = Vec::new();
    if include_all {
        selected = (0..base.columns.len()).collect();
    }
    for s in &c.selectors {
        if s.alias.is_some() {
            return Err(Error::invalid(match d {
                Dialect::Scylla => "Cannot use alias when defining a materialized view".to_string(),
                // Cassandra prints the selector object (`RawSelector`, no `toString`).
                Dialect::Cassandra => format!(
                    "Cannot use aliases when defining a materialized view (got {})",
                    java_object("RawSelector")
                ),
            }));
        }
        let Some(name) = selected_column(s) else {
            return Err(Error::invalid(match d {
                Dialect::Scylla => {
                    "Cannot use general expressions when defining a materialized view".to_string()
                }
                Dialect::Cassandra => format!(
                    "Can only select columns by name when defining a materialized view (got {})",
                    java_object(raw_class(&s.expr))
                ),
            }));
        };
        let Some(ci) = base.column_index(&name) else {
            return Err(match d {
                Dialect::Scylla => Error::invalid(format!(
                    "Unknown column name detected in CREATE MATERIALIZED VIEW statement: {name}"
                )),
                Dialect::Cassandra => msg::undefined_column(d, &ks_name, &base_name, &name),
            });
        };
        if !selected.contains(&ci) {
            selected.push(ci);
        }
    }
    if let Some(&s) = selected.iter().find(|&&ci| base.columns[ci].is_static()) {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!(
                "Unable to include static column '{}' which would be included by Materialized View SELECT * statement",
                base.columns[s].name
            ),
            Dialect::Cassandra => format!(
                "Cannot include static column '{}' in materialized view '{view_name}'",
                base.columns[s].name
            ),
        }));
    }
    // PRIMARY KEY and CLUSTERING ORDER BY.
    let pk = &c.primary_key;
    if pk.partition.is_empty() {
        return Err(Error::invalid(format!(
            "Must provide at least one partition key column for materialized view '{view_name}'"
        )));
    }
    let mut key_cols: Vec<usize> = Vec::new();
    for id in pk.partition.iter().chain(&pk.clustering) {
        let name = id.name();
        // ScyllaDB adds unselected key columns to the view.
        let ci = base
            .column_index(&name)
            .filter(|ci| selected.contains(ci) || d.is_scylla());
        let Some(ci) = ci else {
            return Err(Error::invalid(match d {
                Dialect::Scylla => format!(
                    "Unknown column name detected in CREATE MATERIALIZED VIEW statement: {name}"
                ),
                Dialect::Cassandra => format!(
                    "Unknown column '{name}' referenced in PRIMARY KEY for materialized view '{view_name}'"
                ),
            }));
        };
        if key_cols.contains(&ci) {
            return Err(Error::invalid(match d {
                Dialect::Scylla => format!("Duplicate entry found in PRIMARY KEY: {name}"),
                Dialect::Cassandra => format!(
                    "Duplicate column '{name}' in PRIMARY KEY clause for materialized view '{view_name}'"
                ),
            }));
        }
        let ty = &base.columns[ci].ty;
        if ty.is_multicell() && !d.is_scylla() {
            // `AbstractType.toString`: the marshal class.
            let marshal = nosql_types::marshal_name(ty, false, d);
            return Err(Error::invalid(if ty.is_collection() {
                format!(
                    "Invalid non-frozen collection type '{marshal}' for PRIMARY KEY column '{name}'"
                )
            } else {
                format!(
                    "Invalid non-frozen user-defined type '{marshal}' for PRIMARY KEY column '{name}'"
                )
            }));
        }
        if ty.is_counter() {
            return Err(Error::invalid(format!(
                "counter type is not supported for PRIMARY KEY column '{name}'"
            )));
        }
        if ty.references_duration() {
            return Err(Error::invalid(format!(
                "duration type is not supported for PRIMARY KEY column '{name}'"
            )));
        }
        key_cols.push(ci);
    }
    let mut order: Vec<(String, bool)> = Vec::new();
    for o in &c.options {
        if let TableOption::ClusteringOrder(cols) = o {
            for (id, dir) in cols {
                order.push((id.name().to_string(), *dir == Direction::Desc));
            }
        }
    }
    let ck_names: Vec<String> = pk.clustering.iter().map(|i| i.name().to_string()).collect();
    if let Some((bad, _)) = order.iter().find(|(n, _)| !ck_names.contains(n))
        && d.is_scylla()
    {
        return Err(Error::invalid(format!(
            "CLUSTERING ORDER BY lists {bad} which is not a clustering column in the view"
        )));
    }
    let order_mismatch = || -> Result<()> {
        if order.is_empty() {
            return Ok(());
        }
        let names: Vec<&String> = order.iter().map(|(n, _)| n).collect();
        let same =
            names.len() == ck_names.len() && names.iter().zip(&ck_names).all(|(a, b)| *a == b);
        if same {
            return Ok(());
        }
        Err(Error::invalid(
            "Clustering key columns must exactly match columns in CLUSTERING ORDER BY directive",
        ))
    };
    if !d.is_scylla() {
        order_mismatch()?;
    }
    let base_pk: Vec<usize> = base
        .partition_key
        .iter()
        .chain(&base.clustering)
        .copied()
        .collect();
    let missing: Vec<&str> = base_pk
        .iter()
        .filter(|ci| !key_cols.contains(ci))
        .map(|&ci| base.columns[ci].name.as_str())
        .collect();
    // WHERE clause: restrictions as a base SELECT for a view.
    if c.where_clause
        .iter()
        .any(|r| matches!(r, Relation::Token(_)))
        && !d.is_scylla()
    {
        return Err(Error::invalid(
            "Cannot use token relation when defining a materialized view",
        ));
    }
    if c.where_clause
        .iter()
        .any(|r| matches!(r, Relation::Expression(_)))
    {
        return Err(Error::invalid(format!(
            "WHERE clause for materialized view '{view_name}' cannot contain custom index expressions"
        )));
    }
    let mut cx = PrepareCtx::new(d, &schema, &ks_name, &db.functions, &stmt.bind_markers);
    let r = restrictions::analyze(
        &base,
        StmtKind::Select,
        &c.where_clause,
        true,
        false,
        true,
        &mut cx,
    )?;
    if !missing.is_empty() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!(
                "Cannot create Materialized View {view_name} without primary key columns from base {base_name} ({})",
                missing.join(", ")
            ),
            Dialect::Cassandra => format!(
                "Cannot create materialized view '{view_name}' without primary key columns {} from base table '{base_name}'",
                missing.join(", ")
            ),
        }));
    }
    // Cassandra lists key columns in the order of its `HashSet` of them.
    let key_order: Vec<usize> = match d {
        Dialect::Scylla => key_cols.clone(),
        Dialect::Cassandra => {
            let mut set = HashOrder::new(identifier_hash);
            for &ci in &key_cols {
                set.insert(&base.columns[ci].name);
            }
            set.keys().filter_map(|n| base.column_index(n)).collect()
        }
    };
    let regular_in_pk: Vec<&str> = key_order
        .iter()
        .filter(|ci| !base_pk.contains(ci))
        .map(|&ci| base.columns[ci].name.as_str())
        .collect();
    if regular_in_pk.len() > 1 {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!(
                "Cannot include more than one non-primary key column '{}' in materialized view primary key",
                regular_in_pk[1]
            ),
            Dialect::Cassandra => format!(
                "Cannot include more than one non-primary key column in materialized view primary key (got {})",
                regular_in_pk.join(", ")
            ),
        }));
    }
    // Every view key column is restricted (`IS NOT NULL` or otherwise).
    let restricted = |ci: usize| -> bool {
        let col = &base.columns[ci];
        r.not_null.contains(&ci)
            || match col.kind {
                ColumnKind::PartitionKey => !r.pk[col.position as usize].is_empty(),
                ColumnKind::Clustering => {
                    !r.ck[col.position as usize].is_empty()
                        || r.multi
                            .iter()
                            .any(|m| m.positions.contains(&(col.position as usize)))
                }
                _ => r.others.iter().any(|(c, _)| *c == ci),
            }
    };
    let unrestricted: Vec<&str> = key_order
        .iter()
        .filter(|&&ci| !restricted(ci))
        // ScyllaDB checks the base key columns later (after looking for an existing view).
        .filter(|&&ci| !(d.is_scylla() && base.columns[ci].is_primary_key()))
        .map(|&ci| base.columns[ci].name.as_str())
        .collect();
    if let Some(first) = unrestricted.first() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => {
                format!("Primary key column '{first}' is required to be filtered by 'IS NOT NULL'")
            }
            Dialect::Cassandra => format!(
                "Primary key columns {} must be restricted with 'IS NOT NULL' or otherwise",
                unrestricted.join(", ")
            ),
        }));
    }
    if d.is_scylla() {
        let stray: Vec<&str> = r
            .not_null
            .iter()
            .filter(|ci| !key_cols.contains(ci))
            .map(|&ci| base.columns[ci].name.as_str())
            .collect();
        if !stray.is_empty() {
            return Err(Error::invalid(format!(
                "The IS NOT NULL restriction is allowed only columns which are part of the view's primary key, found columns: {}. The flag strict_is_not_null_in_views can be used to turn this error into a warning, or to silence it. (true - error, warn - warning, false - silent)",
                stray.join(", ")
            )));
        }
    }
    // ScyllaDB filters on the view's key columns (a regular base column in the view key
    // included); Cassandra only takes `IS NOT NULL` there.
    let non_pk: Vec<&str> = r
        .others
        .iter()
        .filter(|(ci, _)| !(d.is_scylla() && key_cols.contains(ci)))
        .map(|(ci, _)| base.columns[*ci].name.as_str())
        .collect();
    if !non_pk.is_empty() {
        return Err(Error::invalid(match d {
            Dialect::Scylla => format!(
                "Non-primary key columns cannot be restricted in the SELECT statement used for materialized view {view_name} creation (got restrictions on: {})",
                non_pk.join(", ")
            ),
            Dialect::Cassandra => format!(
                "Non-primary key columns can only be restricted with 'IS NOT NULL' (got: {} restricted illegally)",
                non_pk.join(",")
            ),
        }));
    }
    // WITH options.
    let props: Vec<&ast::Property> = c
        .options
        .iter()
        .filter_map(|o| match o {
            TableOption::Property(p) => Some(p),
            _ => None,
        })
        .collect();
    let p = Properties::new(props.iter().copied(), params::valid_table_options(d), d)?;
    let mut vparams = TableParams::defaults(d, false);
    // Cassandra validates every property (`TableAttributes.validate`) before the TTL check.
    if !d.is_scylla() {
        params::apply(&mut vparams, &p, true)?;
    }
    if p.int("default_time_to_live")?.unwrap_or(0) != 0 {
        return Err(Error::invalid(match d {
            Dialect::Scylla => {
                "Cannot set or alter default_time_to_live for a materialized view. Data in a materialized view always expire at the same time than the corresponding data in the parent table."
            }
            Dialect::Cassandra => {
                "Cannot set default_time_to_live for a materialized view. Data in a materialized view always expire at the same time than the corresponding data in the parent table."
            }
        }));
    }
    if d.is_scylla() {
        params::apply(&mut vparams, &p, true)?;
    }
    if !d.is_scylla()
        && let Some(e) = msg::table_name_invalid(d, &ks_name, &view_name)
    {
        return Err(e);
    }
    if d.is_scylla() {
        if let Some(r) = exists(ks) {
            return r;
        }
        if let Some(&ci) = key_cols.iter().rev().find(|&&ci| !restricted(ci)) {
            return Err(Error::invalid(format!(
                "Primary key column '{}' is required to be filtered by 'IS NOT NULL'",
                base.columns[ci].name
            )));
        }
        order_mismatch()?;
    }
    let id = match (scylla_id, p.string("id")?) {
        (Some(id), _) => id,
        (None, Some(s)) => {
            crate::uuid::parse(s.trim()).ok_or_else(|| Error::config("Invalid table id"))?
        }
        (None, None) => crate::uuid::timeuuid(),
    };
    // Cassandra fails building its id map (a server error, as the reference answers).
    if let Some(t) = schema.table_by_id(&id) {
        let id = crate::uuid::format(&id);
        return Err(Error::server(format!(
            "java.lang.IllegalArgumentException: Multiple entries with same key: {id}={}.{} and {id}={ks_name}.{view_name}",
            t.keyspace, t.name
        )));
    }
    // Build the view: key columns as declared, then the other selected columns.
    let mut cols: Vec<Column> = Vec::new();
    for (i, id) in pk.partition.iter().enumerate() {
        let b = base.column(&id.name()).expect("checked");
        cols.push(Column {
            name: b.name.clone(),
            ty: b.ty.clone(),
            kind: ColumnKind::PartitionKey,
            position: i as i32,
            reversed: false,
            id: 0,
        });
    }
    for (i, id) in pk.clustering.iter().enumerate() {
        let b = base.column(&id.name()).expect("checked");
        let reversed = match order.iter().find(|(n, _)| *n == b.name) {
            Some((_, desc)) => *desc,
            // Cassandra keeps the base column's (reversed) type, ScyllaDB sorts ascending.
            None => !d.is_scylla() && b.reversed,
        };
        cols.push(Column {
            name: b.name.clone(),
            ty: b.ty.clone(),
            kind: ColumnKind::Clustering,
            position: i as i32,
            reversed,
            id: 0,
        });
    }
    for &ci in &selected {
        if key_cols.contains(&ci) {
            continue;
        }
        let b = &base.columns[ci];
        cols.push(Column {
            name: b.name.clone(),
            ty: b.ty.clone(),
            kind: ColumnKind::Regular,
            position: -1,
            reversed: false,
            id: 0,
        });
    }
    cols.sort_by(crate::schema::canonical_order);
    for (i, c) in cols.iter_mut().enumerate() {
        c.id = i as u32;
    }
    let mut view = Table::new(
        &ks_name,
        &view_name,
        id,
        crate::uuid::timeuuid(),
        cols,
        vparams,
        TableFlags {
            compound: true,
            ..Default::default()
        },
        TableKind::Regular,
    );
    view.view = Some(ViewMeta {
        base_table_id: base.id,
        base_table_name: base.name.clone(),
        where_clause: where_text(d, &c.where_clause),
        include_all_columns: include_all,
        computed: Vec::new(),
    });
    view.masks = crate::masking::view_masks(&base, &view);
    let view = Arc::new(view);
    let mut new_ks = (**ks).clone();
    new_ks.tables.insert(view_name.clone(), view.clone());
    let change = Change {
        keyspace: Some(new_ks),
        attach: vec![view.clone()],
        events: vec![SchemaChange::object(
            ChangeType::Created,
            ChangeTarget::Table,
            &ks_name,
            &view_name,
        )],
        warnings: tablets_warning(),
        ..Default::default()
    };
    let resp = commit(db, &schema, change).await?;
    for o in db.hooks().observers {
        o.barrier().await;
    }
    // `commit` may have moved the view to fresh storage (a dropped id reused).
    let installed = db.schema();
    let view = installed
        .table(&ks_name, &view_name)
        .cloned()
        .unwrap_or(view);
    let spec = super::ViewSpec::view(db, &installed, &base, &view)?;
    super::build(db, &spec, &base).await?;
    Ok(resp)
}

/// ScyllaDB's first checks of a new view, in its order: the `id` (well formed and not taken),
/// `COMPACT STORAGE`, `cdc` (any value), and the name. Returns the given id.
fn scylla_checks(
    schema: &crate::schema::Schema,
    c: &ast::CreateView<'static>,
    name: &str,
) -> Result<Option<[u8; 16]>> {
    // Unknown and repeated properties are refused while parsing.
    let props = c.options.iter().filter_map(|o| match o {
        TableOption::Property(p) => Some(p),
        _ => None,
    });
    Properties::new(
        props,
        params::valid_table_options(Dialect::Scylla),
        Dialect::Scylla,
    )?;
    let prop = |n: &str| {
        c.options.iter().find_map(|o| match o {
            TableOption::Property(p) if p.name.name() == n => Some(&p.value),
            _ => None,
        })
    };
    let id = match prop("id") {
        Some(ast::PropertyValue::Simple(l)) => {
            let id = crate::uuid::parse(l.text.trim())
                .ok_or_else(|| Error::config("Invalid table id"))?;
            if let Some(t) = schema.table_by_id(&id) {
                return Err(Error::invalid(format!(
                    "Table with ID {} already exists: {}.{}",
                    crate::uuid::format(&id),
                    t.keyspace,
                    t.name
                )));
            }
            Some(id)
        }
        _ => None,
    };
    if c.options
        .iter()
        .any(|o| matches!(o, TableOption::CompactStorage))
    {
        return Err(Error::invalid(
            "Cannot use 'COMPACT STORAGE' when defining a materialized view",
        ));
    }
    if prop("cdc").is_some() {
        return Err(Error::invalid(vmsg::VIEW_CDC));
    }
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(Error::invalid(format!(
            "\"{name}\" is not a valid materialized view name (must contain alphanumeric character only: [0-9A-Za-z] or the underscore)"
        )));
    }
    if name.len() > 192 {
        return Err(Error::invalid(format!(
            "materialized view names shouldn't be more than 192 characters long (got \"{name}\")"
        )));
    }
    Ok(id)
}

async fn alter(
    db: &Arc<Database>,
    session: &SessionState,
    a: &ast::AlterView<'static>,
    current: Option<&str>,
) -> Result<Response> {
    let d = db.dialect();
    let ks_name = keyspace_of(&a.name, current)?;
    let name = a.name.name.name().to_string();
    let _g = db.ddl.lock().await;
    let schema = db.schema();
    let ks = schema.keyspace(&ks_name);
    let table = ks.and_then(|k| k.tables.get(&name)).cloned();
    let view = match (&table, d) {
        (Some(t), _) if t.is_view() => t.clone(),
        (Some(t), Dialect::Scylla) if !is_index_view(ks, t) => {
            return Err(Error::invalid(
                "Cannot use ALTER MATERIALIZED VIEW on Table. (Did you mean ALTER TABLE)?",
            ));
        }
        _ => {
            if a.if_exists {
                return Ok(Response::void());
            }
            return Err(match d {
                Dialect::Scylla => msg::unconfigured_table(d, &name),
                Dialect::Cassandra => Error::invalid(format!(
                    "Materialized view '{ks_name}.{name}' doesn't exist"
                )),
            });
        }
    };
    let base_name = view
        .view
        .as_ref()
        .map(|v| v.base_table_name.clone())
        .unwrap_or_default();
    db.hooks().authorizer.authorize(
        db,
        session,
        &Access::Schema {
            keyspace: &ks_name,
            table: &base_name,
            statement: "ALTER MATERIALIZED VIEW",
        },
    )?;
    let p = Properties::new(a.properties.iter(), params::valid_table_options(d), d)?;
    if p.has("cdc") && d.is_scylla() {
        return Err(Error::invalid(vmsg::VIEW_CDC));
    }
    if p.has("id") && !d.is_scylla() {
        return Err(Error::config("Cannot alter table id."));
    }
    let mut vparams = view.params.clone();
    params::apply(&mut vparams, &p, true)?;
    if vparams.gc_grace_seconds == 0 {
        return Err(Error::invalid(
            "Cannot alter gc_grace_seconds of a materialized view to 0, since this value is used to TTL undelivered updates. Setting gc_grace_seconds too low might cause undelivered updates to expire before being replayed.",
        ));
    }
    if vparams.default_time_to_live > 0 {
        return Err(Error::invalid(match d {
            Dialect::Scylla => {
                "Cannot set or alter default_time_to_live for a materialized view. Data in a materialized view always expire at the same time than the corresponding data in the parent table."
            }
            Dialect::Cassandra => {
                "Forbidden default_time_to_live detected for a materialized view. Data in a materialized view always expire at the same time than the corresponding data in the parent table. default_time_to_live must be set to zero, see CASSANDRA-12868 for more information"
            }
        }));
    }
    // Cassandra answers a change that changes nothing with VOID (`cdc`, which we don't store,
    // only counts when turned on).
    let cdc_on = p
        .string("cdc")?
        .is_some_and(|s| s.eq_ignore_ascii_case("true"));
    if !d.is_scylla() && vparams == view.params && !cdc_on {
        return Ok(Response::void());
    }
    let t = Arc::new(view.rebuild(view.columns.clone(), vparams));
    let Some(ks) = ks else {
        return Err(msg::ddl_unknown_keyspace(d, &ks_name));
    };
    let mut new_ks = (**ks).clone();
    new_ks.tables.insert(name.clone(), t.clone());
    let change = Change {
        keyspace: Some(new_ks),
        attach: vec![t],
        events: vec![SchemaChange::object(
            ChangeType::Updated,
            ChangeTarget::Table,
            &ks_name,
            &name,
        )],
        ..Default::default()
    };
    commit(db, &schema, change).await
}

/// A ScyllaDB index view (`<index>_index` of an index of its base table).
fn is_index_view(ks: Option<&Arc<crate::schema::Keyspace>>, t: &Table) -> bool {
    let (Some(ks), Some(v)) = (ks, &t.view) else {
        return false;
    };
    ks.tables.values().any(|b| {
        b.id == v.base_table_id
            && b.indexes
                .iter()
                .any(|i| format!("{}_index", i.name) == t.name)
    })
}

async fn drop_view(
    db: &Arc<Database>,
    session: &SessionState,
    x: &ast::DropObject<'static>,
    current: Option<&str>,
) -> Result<Response> {
    let d = db.dialect();
    let ks_name = keyspace_of(&x.name, current)?;
    let name = x.name.name.name().to_string();
    let schema = db.schema();
    let ks = schema.keyspace(&ks_name);
    let table = ks.and_then(|k| k.tables.get(&name)).cloned();
    let view = match table {
        Some(t) if d.is_scylla() && is_index_view(ks, &t) => {
            return Err(Error::invalid(
                "Cannot use DROP MATERIALIZED VIEW on Index. (Did you mean DROP INDEX)?",
            ));
        }
        Some(t) if t.is_view() => t,
        Some(_) if d.is_scylla() => {
            return Err(Error::invalid(
                "Cannot use DROP MATERIALIZED VIEW on Table. (Did you mean DROP TABLE)?",
            ));
        }
        _ => {
            if x.if_exists {
                return Ok(Response::void());
            }
            return Err(match d {
                Dialect::Scylla => Error::config(format!(
                    "Cannot drop non existing materialized view '{name}' in keyspace '{ks_name}'."
                )),
                Dialect::Cassandra => Error::invalid(format!(
                    "Materialized view '{ks_name}.{name}' doesn't exist"
                )),
            });
        }
    };
    let base_name = view
        .view
        .as_ref()
        .map(|v| v.base_table_name.clone())
        .unwrap_or_default();
    db.hooks().authorizer.authorize(
        db,
        session,
        &Access::Schema {
            keyspace: &ks_name,
            table: &base_name,
            statement: "DROP MATERIALIZED VIEW",
        },
    )?;
    let _g = db.ddl.lock().await;
    let schema = db.schema();
    let Some(ks) = schema.keyspace(&ks_name) else {
        return Err(msg::ddl_unknown_keyspace(d, &ks_name));
    };
    let mut new_ks = (**ks).clone();
    let Some(v) = new_ks.tables.remove(&name) else {
        return Ok(Response::void());
    };
    let change = Change {
        keyspace: Some(new_ks),
        drop_tables: vec![v.table_id()],
        events: vec![SchemaChange::object(
            ChangeType::Dropped,
            ChangeTarget::Table,
            &ks_name,
            &name,
        )],
        ..Default::default()
    };
    commit(db, &schema, change).await
}

/// `(view, base)` names of the materialized views of a keyspace (index views excluded).
pub fn materialized_views(ks: &crate::schema::Keyspace) -> BTreeMap<String, String> {
    ks.tables
        .values()
        .filter(|t| t.is_view() && !is_index_view_of(ks, t))
        .filter_map(|t| {
            t.view
                .as_ref()
                .map(|v| (t.name.clone(), v.base_table_name.clone()))
        })
        .collect()
}

fn is_index_view_of(ks: &crate::schema::Keyspace, t: &Table) -> bool {
    let Some(v) = &t.view else {
        return false;
    };
    ks.tables.values().any(|b| {
        b.id == v.base_table_id
            && b.indexes
                .iter()
                .any(|i| format!("{}_index", i.name) == t.name)
    })
}
