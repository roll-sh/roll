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
    /// A conflict logged with its DETAIL: the row wasn't there (update_missing /
    /// delete_missing), or another origin changed it last (update_origin_differs /
    /// delete_origin_differs, applied all the same).
    Logged(&'static str, String),
}

impl LocalRel {
    /// Still bound to the catalog `ctx` reads.
    pub fn current(&self, ctx: &ExecCtx) -> bool {
        self.target.version == ctx.catalog().version
    }

    pub fn owner(&self) -> u32 {
        self.target.rules.table.owner
    }
}

fn role_name(oid: u32, ctx: &ExecCtx) -> String {
    ctx.catalog().roles.get(&oid).map(|r| r.name.clone()).unwrap_or_default()
}

/// SwitchToUntrustedUser (run_as_owner = false): a change runs as its table's owner, which
/// the subscription owner must be able to SET ROLE to; when the owner can SET ROLE back no
/// switch is needed, else it runs as the owner in a security-restricted operation. True:
/// switched (`switch_back` after the change).
pub fn switch_to_owner(owner: u32, ctx: &ExecCtx) -> Result<bool> {
    let me = ctx.current_user();
    let cat = ctx.catalog();
    if !cat.can_set_role(me, owner) {
        drop(cat);
        return Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("role \"{}\" cannot SET ROLE to \"{}\"", role_name(me, ctx), role_name(owner, ctx)),
        ));
    }
    if cat.can_set_role(owner, me) {
        return Ok(false);
    }
    ctx.sess.definer.borrow_mut().push(owner);
    ctx.sess.sro.set(ctx.sess.sro.get() + 1);
    Ok(true)
}

pub fn switch_back(ctx: &ExecCtx) {
    ctx.sess.definer.borrow_mut().pop();
    ctx.sess.sro.set(ctx.sess.sro.get().saturating_sub(1));
}

/// copy_table's checks before the initial copy into table `relid`: as its owner (unless
/// run_as_owner), INSERT on it, and no row-level security for that user. True: switched
/// (`switch_back` once the copy is done).
pub fn sync_check(relid: u32, run_as_owner: bool, ctx: &ExecCtx) -> Result<bool> {
    let Some(t) = ctx.catalog().tables.get(&relid).cloned() else { return Ok(false) };
    let switched = !run_as_owner && switch_to_owner(t.owner, ctx)?;
    match target_check(&t, crate::catalog::acl::INSERT, ctx) {
        Err(mut e) => {
            if switched {
                switch_back(ctx);
            }
            if e.message.starts_with("user ") {
                e.code = code::FEATURE_NOT_SUPPORTED;
            }
            Err(e)
        }
        Ok(()) => Ok(switched),
    }
}

/// TargetPrivilegesCheck: the current user holds `mode` on table `t`, and row-level
/// security doesn't apply to it there (there is no way to honour policies in apply).
pub fn target_check(t: &Table, mode: u32, ctx: &ExecCtx) -> Result<()> {
    let user = ctx.current_user();
    let cat = ctx.catalog();
    let held = cat.rel_mask(t.oid, user, ctx.db_owner()).map_or(0, |m| m.0);
    if held & mode == 0 {
        return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, format!("permission denied for table {}", t.name)));
    }
    let rls = t.rls
        && !cat.is_superuser(user)
        && !cat.roles.get(&user).is_some_and(|r| r.bypass_rls)
        && (t.rls_force || !cat.has_privs_of_role(user, t.owner, ctx.db_owner()));
    if rls {
        drop(cat);
        return Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!(
                "user \"{}\" cannot replicate into relation with row-level security enabled: \"{}\"",
                role_name(user, ctx),
                t.name
            ),
        ));
    }
    Ok(())
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

/// A sent value through the column's input function (text) or receive function (binary =
/// true), as slot_store_data runs them, with its error context.
fn input(lr: &LocalRel, rel: &RemoteRel, i: usize, c: Col, ctx: &ExecCtx) -> Result<Value> {
    let t = &lr.target.rules.table;
    let col = &t.columns[lr.attrmap[i]];
    let r = match c {
        Col::Text(b) => match std::str::from_utf8(b) {
            Ok(s) => crate::types::text::parse_value(s, &col.ty, ctx.fmt()),
            Err(_) => crate::types::text::parse_value(&String::from_utf8_lossy(b), &col.ty, ctx.fmt()),
        },
        Col::Binary(b) => crate::types::binary::decode(b, &col.ty, ctx.fmt()).map_err(|e| {
            if e.message.starts_with("incorrect binary data format") {
                Error::new(
                    code::INVALID_BINARY_REPRESENTATION,
                    format!("incorrect binary data format in logical replication column {}", i + 1),
                )
            } else {
                e
            }
        }),
        Col::Null | Col::Unchanged => return Ok(Value::Null),
    };
    r.map_err(|e| {
        e.context(format!(
            "processing remote data for replication target relation \"{}.{}\" column \"{}\"",
            rel.nsp, rel.name, col.name
        ))
    })
}

/// slot_store_data: a tuple over the local columns (NULL where nothing or an unchanged value
/// is sent).
fn store(lr: &LocalRel, rel: &RemoteRel, tuple: &[Col], ctx: &ExecCtx) -> Result<Row> {
    let mut row = vec![Value::Null; lr.target.rules.table.columns.len()];
    for (i, &c) in tuple.iter().enumerate().take(lr.attrmap.len()) {
        if matches!(c, Col::Text(_) | Col::Binary(_)) {
            row[lr.attrmap[i]] = input(lr, rel, i, c, ctx)?;
        }
    }
    Ok(row)
}

/// apply_handle_insert: the new row, its columns the publisher doesn't send defaulted
/// (slot_fill_defaults).
pub fn insert(lr: &LocalRel, rel: &RemoteRel, new: &[Col], ctx: &ExecCtx) -> Result<()> {
    let mut src: Row = Vec::with_capacity(lr.attrmap.len());
    for i in 0..lr.attrmap.len() {
        src.push(input(lr, rel, i, new.get(i).copied().unwrap_or(Col::Null), ctx)?);
    }
    let row = crate::exec::dml::new_row(&lr.target.rules, &lr.attrmap, src, ctx)?;
    target_check(&lr.target.rules.table, crate::catalog::acl::INSERT, ctx)?;
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

/// update_origin_differs / delete_origin_differs (track_commit_timestamp on): the row found
/// was last changed by another origin than the subscription's (or locally); the conflict and
/// its DETAIL, logged before the change applies.
fn origin_differs(
    kind: &'static str,
    what: &str,
    p: &Place,
    xmin: u64,
    local: &[Value],
    remote: Option<&[Value]>,
    origin: u16,
    ctx: &ExecCtx,
) -> Option<(&'static str, String)> {
    let s = xr::row_commit(xmin, ctx)?;
    if s.origin == origin {
        return None;
    }
    let mut d = xr::modified_by(what, "a different origin ", xmin, s, ctx);
    if let Some(v) = xr::value_details(p.rules, None, Some(local), remote, Some(&p.search), ctx) {
        d.push('\n');
        d.push_str(&v);
    }
    Some((kind, d))
}

/// apply_handle_update: the row the old key (or old row) finds takes the sent values.
pub fn update(lr: &LocalRel, rel: &RemoteRel, old: Option<&[Col]>, new: &[Col], origin: u16, ctx: &ExecCtx) -> Result<Outcome> {
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
    let leaf_row = |remote: Row| match (&lr.target.route, p.leaf) {
        (Some(r), Some(k)) => to_leaf(&r.leaves[k].map, p.rules.table.columns.len(), &remote),
        _ => remote,
    };
    let Some((dkey, old_row, xmin)) = xr::find(p.rules, p.how, &p.search, false, ctx)? else {
        let remote = leaf_row(remote);
        return Ok(Outcome::Logged("update_missing", missing_detail(p.rules, "updated", Some(&remote), &p.search, ctx)));
    };
    let differs =
        origin_differs("update_origin_differs", "Updating the row that was", &p, xmin, &old_row, Some(&leaf_row(remote)), origin, ctx);
    let outcome = differs.map_or(Outcome::Applied, |(k, d)| Outcome::Logged(k, d));
    target_check(&p.rules.table, crate::catalog::acl::UPDATE, ctx)?;
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
        return Ok(outcome);
    }
    xr::update(p.rules, &dkey, old_row, new_row, &set_cols, &p.search, ctx)?;
    Ok(outcome)
}

/// apply_handle_delete: the row the old key (or old row) finds.
pub fn delete(lr: &LocalRel, rel: &RemoteRel, old: &[Col], origin: u16, ctx: &ExecCtx) -> Result<Outcome> {
    if !lr.updatable {
        return Err(not_updatable(lr, rel, ctx));
    }
    let search = store(lr, rel, old, ctx)?;
    let p = place(lr, search, Vec::new(), ctx)?;
    match xr::find(p.rules, p.how, &p.search, true, ctx)? {
        Some((dkey, old_row, xmin)) => {
            let differs = origin_differs("delete_origin_differs", "Deleting the row that was", &p, xmin, &old_row, None, origin, ctx);
            target_check(&p.rules.table, crate::catalog::acl::DELETE, ctx)?;
            xr::delete(p.rules, &dkey, old_row, ctx)?;
            Ok(differs.map_or(Outcome::Applied, |(k, d)| Outcome::Logged(k, d)))
        }
        None => Ok(Outcome::Logged("delete_missing", missing_detail(p.rules, "deleted", None, &p.search, ctx))),
    }
}
