//! A pgoutput stream's rows applied to local tables (worker.c's logicalrep_rel_open,
//! slot_store_data / slot_modify_data and apply_handle_insert / _update / _delete): values
//! through their types' input functions, then the executor's row paths
//! (`exec::replication`). No SQL statement is parsed or planned per change.

use super::apply::{Col, RemoteRel};
use crate::catalog::{Relation, Table};
use crate::error::{Error, Result, code};
use crate::exec::replication::{self as xr, Search, Target};
use crate::exec::{ExecCtx, Row};
use crate::sql::plan::TableRules;
use crate::types::Value;
use std::sync::Arc;

/// A remote relation's local table, bound for applying rows (LogicalRepRelMapEntry).
pub struct LocalRel {
    pub relid: u32,
    target: Target,
    /// The local column of each remote column.
    attrmap: Vec<usize>,
    /// The local columns the publisher sends.
    sent: Vec<bool>,
    /// The publisher sends whole old rows (REPLICA IDENTITY FULL).
    full: bool,
    /// logicalrep_rel_mark_updatable: the publisher's key covers the local identity.
    updatable: bool,
}

/// What an UPDATE or DELETE found to do.
pub enum Outcome {
    Applied,
    /// The row wasn't there: the conflict (update_missing / delete_missing) and its
    /// DETAIL, logged.
    Missing(&'static str, String),
}

impl LocalRel {
    /// Still bound to the catalog `ctx` reads.
    pub fn current(&self, ctx: &ExecCtx) -> bool {
        self.target.version == ctx.catalog().version
    }
}

/// logicalrep_rel_open: the local table of `rel` (by schema and name, its columns by name).
pub fn open(rel: &RemoteRel, ctx: &ExecCtx) -> Result<LocalRel> {
    let cat = ctx.catalog();
    let prereq = |m: String| Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, m);
    let found = cat.schema_by_name(&rel.nsp).and_then(|s| cat.relation_in(s.oid, &rel.name));
    let table: Arc<Table> = match found {
        Some(Relation::Table(t)) if !t.is_foreign() => t.clone(),
        Some(_) => {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("cannot use relation \"{}.{}\" as logical replication target", rel.nsp, rel.name),
            ));
        }
        None => return Err(prereq(format!("logical replication target relation \"{}.{}\" does not exist", rel.nsp, rel.name))),
    };
    let mut attrmap = Vec::with_capacity(rel.cols.len());
    let mut missing = Vec::new();
    let mut generated = Vec::new();
    for (name, _) in &rel.cols {
        match table.columns.iter().position(|c| !c.dropped && c.name == *name) {
            Some(i) if table.columns[i].generated.is_some() => generated.push(format!("\"{name}\"")),
            Some(i) => attrmap.push(i),
            None => missing.push(format!("\"{name}\"")),
        }
    }
    let plural = |v: &[String]| if v.len() == 1 { "" } else { "s" };
    if !missing.is_empty() {
        return Err(prereq(format!(
            "logical replication target relation \"{}.{}\" is missing replicated column{}: {}",
            rel.nsp,
            rel.name,
            plural(&missing),
            missing.join(", ")
        )));
    }
    if !generated.is_empty() {
        return Err(prereq(format!(
            "logical replication target relation \"{}.{}\" has incompatible generated column{}: {}",
            rel.nsp,
            rel.name,
            plural(&generated),
            generated.join(", ")
        )));
    }
    let mut sent = vec![false; table.columns.len()];
    let mut keyed = vec![false; table.columns.len()];
    let full = rel.ident == b'f';
    for ((_, key), &i) in rel.cols.iter().zip(&attrmap) {
        sent[i] = true;
        keyed[i] = *key || full;
    }
    let updatable = match xr::identity_or_pk(&cat, &table) {
        Some(cols) => cols.iter().all(|&c| keyed[c]),
        None => full,
    };
    drop(cat);
    let target = xr::open(&table, &sent, full, ctx)?;
    Ok(LocalRel { relid: table.oid, target, attrmap, sent, full, updatable })
}

/// check_relation_updatable's errors.
fn not_updatable(lr: &LocalRel, rel: &RemoteRel, ctx: &ExecCtx) -> Error {
    let t = &lr.target.rules.table;
    let m = if xr::identity_or_pk(&ctx.catalog(), t).is_some() {
        format!(
            "publisher did not send replica identity column expected by the logical replication target relation \"{}.{}\"",
            rel.nsp, rel.name
        )
    } else {
        format!(
            "logical replication target relation \"{}.{}\" has neither REPLICA IDENTITY index nor PRIMARY KEY and published relation does not have REPLICA IDENTITY FULL",
            rel.nsp, rel.name
        )
    };
    Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, m)
}

/// The text of a sent value through the column's input function.
fn input(lr: &LocalRel, rel: &RemoteRel, i: usize, s: &str, ctx: &ExecCtx) -> Result<Value> {
    let t = &lr.target.rules.table;
    let c = &t.columns[lr.attrmap[i]];
    crate::types::text::parse_value(s, &c.ty, ctx.fmt()).map_err(|e| {
        e.context(format!(
            "processing remote data for replication target relation \"{}.{}\" column \"{}\"",
            rel.nsp, rel.name, c.name
        ))
    })
}

/// slot_store_data: a tuple over the local columns (NULL where nothing or an unchanged value
/// is sent).
fn store(lr: &LocalRel, rel: &RemoteRel, tuple: &[Col], ctx: &ExecCtx) -> Result<Row> {
    let mut row = vec![Value::Null; lr.target.rules.table.columns.len()];
    for (i, c) in tuple.iter().enumerate().take(lr.attrmap.len()) {
        if let Col::Text(s) = c {
            row[lr.attrmap[i]] = input(lr, rel, i, s, ctx)?;
        }
    }
    Ok(row)
}

/// apply_handle_insert: the new row, its columns the publisher doesn't send defaulted
/// (slot_fill_defaults).
pub fn insert(lr: &LocalRel, rel: &RemoteRel, new: &[Col], ctx: &ExecCtx) -> Result<()> {
    let vals = store(lr, rel, new, ctx)?;
    let src: Row = lr.attrmap.iter().map(|&i| vals[i].clone()).collect();
    let row = crate::exec::dml::new_row(&lr.target.rules, &lr.attrmap, src, ctx)?;
    xr::insert(&lr.target, row, ctx)
}

/// A row of the partitioned table's layout as `leaf`'s.
fn to_leaf(map: &[Option<usize>], width: usize, row: &[Value]) -> Row {
    let mut out = vec![Value::Null; width];
    for (v, m) in row.iter().zip(map) {
        if let Some(j) = m {
            out[*j] = v.clone();
        }
    }
    out
}

/// Where the row searched for is: the table's rules, how to find it there, and the search
/// row and assigned columns in its layout (a partitioned table's: the leaf the row routes
/// to, apply_handle_tuple_routing's).
struct Place<'a> {
    rules: &'a TableRules,
    how: Search,
    search: Row,
    sets: Vec<(usize, Value)>,
    leaf: Option<usize>,
}

fn place<'a>(lr: &'a LocalRel, search: Row, sets: Vec<(usize, Value)>, ctx: &ExecCtx) -> Result<Place<'a>> {
    let Some(routing) = &lr.target.route else {
        return Ok(Place { rules: &lr.target.rules, how: lr.target.search, search, sets, leaf: None });
    };
    let k = crate::exec::dml::route_row(routing, &search, ctx)?;
    let leaf = &routing.leaves[k];
    let width = leaf.rules.table.columns.len();
    let mut sent = vec![false; width];
    for (i, m) in leaf.map.iter().enumerate() {
        if let Some(j) = m {
            sent[*j] = lr.sent.get(i).copied().unwrap_or(false);
        }
    }
    let how = xr::search_of(&leaf.rules, &sent, lr.full, ctx);
    let sets = sets.into_iter().filter_map(|(c, v)| leaf.map.get(c).copied().flatten().map(|j| (j, v))).collect();
    Ok(Place { rules: &leaf.rules, how, search: to_leaf(&leaf.map, width, &search), sets, leaf: Some(k) })
}

/// The DETAIL of an update_missing / delete_missing conflict.
fn missing_detail(rules: &TableRules, what: &str, remote: Option<&[Value]>, search: &[Value], ctx: &ExecCtx) -> String {
    let mut d = format!("Could not find the row to be {what}.");
    if let Some(v) = xr::value_details(rules, None, None, remote, Some(search), ctx) {
        d.push('\n');
        d.push_str(&v);
    }
    d
}

/// apply_handle_update: the row the old key (or old row) finds takes the sent values.
pub fn update(lr: &LocalRel, rel: &RemoteRel, old: Option<&[Col]>, new: &[Col], ctx: &ExecCtx) -> Result<Outcome> {
    if !lr.updatable {
        return Err(not_updatable(lr, rel, ctx));
    }
    let search = store(lr, rel, old.unwrap_or(new), ctx)?;
    let remote = store(lr, rel, new, ctx)?;
    let sets: Vec<(usize, Value)> = new
        .iter()
        .zip(&lr.attrmap)
        .filter(|(c, _)| !matches!(c, Col::Unchanged))
        .map(|(_, &i)| (i, remote[i].clone()))
        .collect();
    let p = place(lr, search, sets, ctx)?;
    let Some((dkey, old_row)) = xr::find(p.rules, p.how, &p.search, false, ctx)? else {
        let remote = match (&lr.target.route, p.leaf) {
            (Some(r), Some(k)) => to_leaf(&r.leaves[k].map, p.rules.table.columns.len(), &remote),
            _ => remote,
        };
        return Ok(Outcome::Missing("update_missing", missing_detail(p.rules, "updated", Some(&remote), &p.search, ctx)));
    };
    let mut new_row = old_row.clone();
    for (c, v) in &p.sets {
        new_row[*c] = v.clone();
    }
    let set_cols: Vec<usize> = p.sets.iter().map(|(c, _)| *c).collect();
    // A row leaving its partition: deleted there, inserted through the partitioned table.
    if let (Some(routing), Some(k)) = (&lr.target.route, p.leaf)
        && let Some(levels) = &routing.leaves[k].check
        && !crate::sql::binder::partition::check_levels(levels, &mut |key| {
            key.iter().map(|e| crate::sql::expr::eval(e, &new_row, ctx)).collect()
        })?
    {
        let leaf = &routing.leaves[k];
        xr::delete(p.rules, &dkey, old_row, ctx)?;
        let root: Row = leaf.map.iter().map(|m| m.map_or(Value::Null, |j| new_row[j].clone())).collect();
        xr::insert(&lr.target, root, ctx)?;
        return Ok(Outcome::Applied);
    }
    xr::update(p.rules, &dkey, old_row, new_row, &set_cols, &p.search, ctx)?;
    Ok(Outcome::Applied)
}

/// apply_handle_delete: the row the old key (or old row) finds.
pub fn delete(lr: &LocalRel, rel: &RemoteRel, old: &[Col], ctx: &ExecCtx) -> Result<Outcome> {
    if !lr.updatable {
        return Err(not_updatable(lr, rel, ctx));
    }
    let search = store(lr, rel, old, ctx)?;
    let p = place(lr, search, Vec::new(), ctx)?;
    match xr::find(p.rules, p.how, &p.search, true, ctx)? {
        Some((dkey, old_row)) => {
            xr::delete(p.rules, &dkey, old_row, ctx)?;
            Ok(Outcome::Applied)
        }
        None => Ok(Outcome::Missing("delete_missing", missing_detail(p.rules, "deleted", None, &p.search, ctx))),
    }
}
