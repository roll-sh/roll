//! Materialized views and the view machinery index tables share.
//!
//! A view is a [`Table`] of its keyspace with [`crate::schema::ViewMeta`]: its rows are derived
//! from the base table's, maintained on every base write by the [`observer`] (read the touched
//! base rows under a partition lock, reconcile them with the update, generate the view updates
//! with Cassandra's `ViewUpdateGenerator` rules in [`update`]) and applied atomically with it.
//! A new view is backfilled from the base table before `CREATE` returns ([`build`]), so build
//! status tables ([`status`]) always show it built.
//!
//! - [`spec`]: a view resolved against its base (key sources, copied columns, `WHERE` filter).
//! - [`ddl`]: `CREATE` / `ALTER` / `DROP MATERIALIZED VIEW` with each dialect's validation.
//!
//! Semantics follow Cassandra's for both dialects (view row liveness from the base row marker,
//! the base column in the view key, or the unselected columns; expired markers for removed
//! entries). ScyllaDB keeps unselected columns' liveness in *virtual columns*; we compute the same
//! liveness from the base row instead and only list them in `system_schema.view_virtual_columns`
//! ([`virtual_columns`]).

pub mod ddl;
pub mod java;
pub mod msg;
pub mod observer;
pub mod spec;
pub mod status;
pub mod update;

use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use nosql_storage::PartitionUpdate;
use nosql_storage::manifest::TableId;
use nosql_storage::read::RangeRead;
use nosql_types::CqlType;

pub use spec::{KeySource, ViewSpec};

use crate::config::DbConfig;
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::Hooks;
use crate::schema::{ColumnKind, Table};

/// Registers the view machinery (the write observer of views, index tables and CDC logs) and
/// the materialized view statements.
pub fn install(hooks: &mut Hooks, _cfg: &DbConfig) {
    hooks.observers.push(observer());
    hooks.statements.push(Arc::new(ddl::Handler));
}

/// The observer instance of a database's hooks.
fn observer() -> Arc<observer::Observer> {
    Arc::new(observer::Observer::default())
}

/// ScyllaDB's virtual columns of a view: when the view key has no regular base column, every
/// unselected regular base column, typed `empty` (non-frozen collections keep their keys:
/// `list<empty>`, `map<k, empty>`, `set<k>`).
pub fn virtual_columns(base: &Table, view: &Table) -> Vec<(String, String)> {
    let key_has_regular = view
        .partition_key_columns()
        .chain(view.clustering_columns())
        .any(|c| base.column(&c.name).is_some_and(|b| !b.is_primary_key()));
    if key_has_regular {
        return Vec::new();
    }
    // Collection index views carry their element in the key.
    if view
        .view
        .as_ref()
        .is_some_and(|v| v.computed.iter().any(|(n, _)| n == "coll_value"))
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    for c in &base.columns {
        if c.kind != ColumnKind::Regular || view.column(&c.name).is_some() {
            continue;
        }
        let ty = if c.ty.is_multicell() {
            match &c.ty {
                CqlType::List { .. } => "list<empty>".to_string(),
                CqlType::Set { elem, .. } => format!("set<{}>", nosql_types::cql_name(elem)),
                CqlType::Map { key, .. } => format!("map<{}, empty>", nosql_types::cql_name(key)),
                // A non-frozen UDT keeps its type.
                other => crate::schema::persist::type_text(other, nosql_types::Dialect::Scylla),
            }
        } else {
            "empty".to_string()
        };
        out.push((c.name.clone(), ty));
    }
    out
}

/// Whether a ScyllaDB view is updated synchronously (`synchronous_updates = true`, stored as the
/// `system:synchronous_view_updates` tag; local index views have it).
pub fn synchronous(view: &Table) -> bool {
    view.params
        .extensions
        .get("scylla_tags")
        .and_then(|b| crate::schema::params::parse_scylla_map_blob(b))
        .is_some_and(|tags| {
            tags.get("system:synchronous_view_updates")
                .is_some_and(|v| v == "true")
        })
}

/// The view tables (materialized views and ScyllaDB index views) of `base`.
pub fn views_of<'a>(
    ks: &'a crate::schema::Keyspace,
    base: &'a Table,
) -> impl Iterator<Item = &'a Arc<Table>> {
    ks.tables
        .values()
        .filter(move |t| t.view.as_ref().is_some_and(|v| v.base_table_id == base.id))
}

/// After `ALTER TABLE base ADD`: the `SELECT *` views of `base` get its new regular columns
/// (`AlterTableStatement.AddColumns`); returns the rebuilt views.
pub fn follow_base(ks: &mut crate::schema::Keyspace, base: &Table) -> Vec<Arc<Table>> {
    let mut out = Vec::new();
    let views: Vec<Arc<Table>> = views_of(ks, base)
        .filter(|v| v.view.as_ref().is_some_and(|m| m.include_all_columns))
        .cloned()
        .collect();
    for v in views {
        let mut cols = v.columns.clone();
        let mut next = v.next_column_id;
        for c in &base.columns {
            if c.kind == ColumnKind::Regular && v.column(&c.name).is_none() {
                cols.push(crate::schema::Column {
                    name: c.name.clone(),
                    ty: c.ty.clone(),
                    kind: ColumnKind::Regular,
                    position: -1,
                    reversed: false,
                    id: *v.column_ids.get(&c.name).unwrap_or(&next),
                });
                next += 1;
            }
        }
        if cols.len() == v.columns.len() {
            continue;
        }
        let t = Arc::new(v.rebuild(cols, v.params.clone()));
        ks.tables.insert(t.name.clone(), t.clone());
        out.push(t);
    }
    out
}

/// After `ALTER TABLE base RENAME` (Cassandra): the base's materialized views rename the same
/// key columns, in their columns and `WHERE` clause (`ViewMetadata.renamePrimaryKeyColumn`);
/// returns the rebuilt views.
pub fn follow_rename(
    ks: &mut crate::schema::Keyspace,
    base: &Table,
    renamed: &[(String, String)],
) -> Vec<Arc<Table>> {
    let mvs = ddl::materialized_views(ks);
    let views: Vec<Arc<Table>> = views_of(ks, base)
        .filter(|v| mvs.contains_key(&v.name))
        .cloned()
        .collect();
    let mut out = Vec::new();
    for v in views {
        let mut cols = v.columns.clone();
        let mut hit = false;
        for c in cols.iter_mut() {
            if let Some((_, to)) = renamed.iter().find(|(from, _)| *from == c.name) {
                c.name = to.clone();
                hit = true;
            }
        }
        if !hit {
            continue;
        }
        let mut t = v.rebuild(cols, v.params.clone());
        for (from, _) in renamed {
            t.column_ids.remove(from);
        }
        if let Some(meta) = t.view.as_mut() {
            meta.where_clause = rename_identifiers(&meta.where_clause, renamed);
        }
        let t = Arc::new(t);
        ks.tables.insert(t.name.clone(), t.clone());
        out.push(t);
    }
    out
}

/// `text` (a stored `WHERE` clause) with the column identifiers `renamed` replaced; string
/// literals are left alone.
fn rename_identifiers(text: &str, renamed: &[(String, String)]) -> String {
    let d = nosql_types::Dialect::Cassandra;
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let replace = |name: &str| {
        renamed
            .iter()
            .find(|(from, _)| from == name)
            .map(|(_, to)| nosql_types::quote_identifier(to, d))
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            // A string literal ('' escapes a quote).
            let start = i;
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(chars.len());
            out.extend(&chars[start..i]);
        } else if c == '"' {
            let start = i;
            let mut name = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        name.push('"');
                        i += 2;
                        continue;
                    }
                    break;
                }
                name.push(chars[i]);
                i += 1;
            }
            i = (i + 1).min(chars.len());
            match replace(&name) {
                Some(r) => out.push_str(&r),
                None => out.extend(&chars[start..i]),
            }
        } else if c.is_alphanumeric() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            match replace(&word) {
                Some(r) if !c.is_ascii_digit() => out.push_str(&r),
                _ => out.push_str(&word),
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Rows written per batch while building a view.
const BUILD_BATCH: usize = 512;

/// Backfills a new view (or index table) from its base table: every base row is a new entry
/// (`ViewBuilderTask`), written with the base cells' timestamps so concurrent writes win.
pub async fn build(db: &Arc<Database>, spec: &ViewSpec, base: &Arc<Table>) -> Result<()> {
    let now = db.now_secs();
    let st = db.storage_table(base)?;
    let mut stream = st.scan(RangeRead::all(now));
    let mut pending: Vec<(TableId, PartitionUpdate)> = Vec::new();
    let mut rows = 0usize;
    let id = spec.view.table_id();
    let mut statics_seen: Option<nosql_storage::PartitionKey> = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Error::internal)?;
        let parts: Vec<Bytes> = crate::storage::key_components(base, &chunk.key.key)
            .into_iter()
            .map(Option::unwrap_or_default)
            .collect();
        let bp = update::BasePartition {
            table: base,
            key: &parts,
            token: chunk.key.token,
            now,
            dialect: db.dialect(),
        };
        let mut g = update::Generator::new(spec, &bp);
        if spec.static_target {
            if statics_seen.as_ref() != Some(&chunk.key)
                && let Some(s) = &chunk.static_row
            {
                g.add(None, s)?;
                rows += 1;
            }
            statics_seen = Some(chunk.key.clone());
        } else {
            for r in &chunk.rows {
                g.add(None, r)?;
                rows += 1;
            }
        }
        pending.extend(g.finish().into_iter().map(|u| (id, u)));
        if rows >= BUILD_BATCH {
            db.apply(std::mem::take(&mut pending)).await?;
            rows = 0;
        }
    }
    db.apply(pending).await
}
