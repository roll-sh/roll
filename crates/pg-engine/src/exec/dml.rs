//! INSERT / UPDATE / DELETE: defaults, generated columns, NOT NULL / CHECK / domain checks,
//! unique and primary keys, index maintenance, foreign keys (both sides, with referential
//! actions), ON CONFLICT, row locking with READ COMMITTED re-checks, and RETURNING.

use super::scan::{
    Locked, NUM_SYS_COLS, data_key, decode_table_row, index_entry_key,
    index_value_prefix, lock_row, lock_row_mode, sys_values, Xltw,
};
use super::trigger::{self, AfterRow};
use super::{BoxNode, ExecCtx, Node, Row, eval_all};
use crate::catalog::trigger_type as tt;
use crate::catalog::{ConstraintKind, FkAction, IndexKey, Table};
use crate::error::{Error, Frame, Result, code};
use crate::sql::expr::{Expr, eval, eval_bool};
use crate::sql::plan::{
    BoundIndex, ConflictAction, CopyLines, DeletePlan, InsertPlan, OldNew, PartLeaf, PartRouting,
    RowMark, TableRules, UpdatePlan, Wait,
};
use crate::storage::keys::{encode_value, index_prefix, prefix_end};
use crate::txn::DeferredFk;
use crate::txn::locks::{LockMode, RowLockMode, WaitPolicy};
use crate::types::text::format_value;
use crate::types::{DataType, Value};
use std::collections::HashSet;
use std::sync::Arc;

thread_local! {
    /// The target of each running INSERT / UPDATE / MERGE (innermost last) and the columns
    /// it assigns: what a failing row's description may show (ExecBuildSlotValueDescription).
    static DML_TARGETS: std::cell::RefCell<Vec<(Arc<Table>, Vec<String>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// While alive, `table` is the running statement's target, assigning columns `cols` (of
/// `layout`, which may be a child of `table`).
pub struct DmlTarget;

impl DmlTarget {
    pub fn push(table: Arc<Table>, layout: &Table, cols: &[usize]) -> DmlTarget {
        let names = cols
            .iter()
            .filter_map(|&i| layout.columns.get(i).map(|c| c.name.clone()))
            .collect();
        DML_TARGETS.with(|s| s.borrow_mut().push((table, names)));
        DmlTarget
    }
}

impl Drop for DmlTarget {
    fn drop(&mut self) {
        DML_TARGETS.with(|s| {
            s.borrow_mut().pop();
        });
    }
}

/// ExecBuildSlotValueDescription: "(1, null, abc)", or "(a, c) = (1, abc)" when the user may
/// see only some columns (those with SELECT, or assigned); None when nothing may be shown.
/// A row written to a partition through its partitioned table shows as the root's row.
pub fn failing_row(t: &Table, row: &[Value], ctx: &ExecCtx) -> Option<String> {
    let cat = ctx.catalog();
    let target = DML_TARGETS.with(|s| s.borrow().last().cloned());
    let mut rel: Arc<Table> = Arc::new(t.clone());
    let mut vals: Vec<Value> = row.to_vec();
    let mut modified: Vec<String> = Vec::new();
    if let Some((root, cols)) = target {
        if root.oid == t.oid {
            modified = cols;
        } else if cat.inheritors(root.oid).contains(&t.oid) {
            // (A partition written through its partitioned table, or a member of an
            // inheritance tree updated through its parent: ri_RootResultRelInfo's row.)
            let mut v = vec![Value::Null; root.columns.len()];
            for (i, c) in root.live_columns() {
                if let Some(j) = t.column_index(&c.name) {
                    v[i] = row.get(j).cloned().unwrap_or(Value::Null);
                }
            }
            vals = v;
            rel = root;
            modified = cols;
        }
    }
    if rel.rls
        && crate::sql::binder::Binder::new(ctx, Vec::new())
            .rls_enabled(&rel)
            .unwrap_or(true)
    {
        return None;
    }
    let me = ctx.current_user();
    let db_owner = ctx.db_owner();
    let table_perm = cat.is_superuser(me)
        || cat
            .rel_mask(rel.oid, me, db_owner)
            .is_some_and(|m| m.0 & crate::catalog::acl::SELECT != 0);
    let mut names = Vec::new();
    let mut parts = Vec::new();
    for (i, c) in rel.live_columns() {
        if !table_perm {
            let seen = modified.contains(&c.name)
                || cat.col_mask(rel.oid, i as i32 + 1, me, db_owner).0 & crate::catalog::acl::SELECT
                    != 0;
            if !seen {
                continue;
            }
            names.push(c.name.clone());
        }
        let v = match &vals[i] {
            _ if c.virtual_gen => "virtual".to_string(),
            Value::Null => "null".to_string(),
            v => format_value(v, &c.ty, ctx.fmt()),
        };
        // (At most 64 bytes of each value.)
        parts.push(if v.len() <= 64 {
            v
        } else {
            let mut end = 64;
            while !v.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &v[..end])
        });
    }
    if !table_perm && names.is_empty() {
        return None;
    }
    Some(if table_perm {
        format!("Failing row contains ({}).", parts.join(", "))
    } else {
        format!(
            "Failing row contains ({}) = ({}).",
            names.join(", "),
            parts.join(", ")
        )
    })
}

/// `e` with the failing row's description as its detail, if any may be shown.
pub fn with_failing_row(e: Error, t: &Table, row: &[Value], ctx: &ExecCtx) -> Error {
    match failing_row(t, row, ctx) {
        Some(d) => e.detail(d),
        None => e,
    }
}

/// BuildIndexValueDescription's check: whether the user may see the key (not under row-level
/// security; SELECT on the table or on every key column, which must be plain columns).
pub fn key_visible(t: &Table, ix: &BoundIndex, ctx: &ExecCtx) -> bool {
    let cat = ctx.catalog();
    if t.rls
        && crate::sql::binder::Binder::new(ctx, Vec::new())
            .rls_enabled(t)
            .unwrap_or(true)
    {
        return false;
    }
    let me = ctx.current_user();
    let db_owner = ctx.db_owner();
    if cat.is_superuser(me)
        || cat
            .rel_mask(t.oid, me, db_owner)
            .is_some_and(|m| m.0 & crate::catalog::acl::SELECT != 0)
    {
        return true;
    }
    let nkeys = ix.keys.len().min(ix.index.columns.len());
    ix.index.columns[..nkeys].iter().all(|c| match c.key {
        IndexKey::Column(i) => {
            cat.col_mask(t.oid, i as i32 + 1, me, db_owner).0 & crate::catalog::acl::SELECT != 0
        }
        IndexKey::Expr(_) => false,
    })
}

pub fn key_detail(
    t: &Table,
    ix: &BoundIndex,
    vals: &[Value],
    ctx: &ExecCtx,
    suffix: &str,
) -> String {
    let names: Vec<String> = ix
        .index
        .columns
        .iter()
        .map(|c| match &c.key {
            // (pg_get_indexdef_columns: quote_identifier's names.)
            IndexKey::Column(i) => crate::catalog::quote_ident(&t.columns[*i].name),
            IndexKey::Expr(e) => crate::sql::func::system::index_key_text(ctx, t, e),
        })
        .collect();
    let values: Vec<String> = vals
        .iter()
        .zip(&ix.keys)
        .map(|(v, e)| match v {
            Value::Null => "null".into(),
            v => format_value(v, &e.ty(), ctx.fmt()),
        })
        .collect();
    format!(
        "Key ({})=({}) {suffix}",
        names.join(", "),
        values.join(", ")
    )
}

/// Validate a new row: NOT NULL, domains, CHECK constraints.
pub fn check_row(rules: &TableRules, row: &[Value], ctx: &ExecCtx) -> Result<()> {
    check_row_rls(rules, &rules.rls_checks, row, ctx)
}

/// Row-level security WITH CHECK options (ExecWithCheckOptions): NULL fails too.
pub fn check_rls(
    checks: &[crate::sql::plan::RlsCheck],
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<()> {
    // A policy reading the new row's system columns (Postgres's slot for it has no TID yet:
    // ctid is (InvalidBlockNumber,0)).
    let mut wide = false;
    for c in checks {
        c.expr.walk(&mut |x| wide |= matches!(x, crate::sql::expr::Expr::Col(i, _) if *i >= row.len()));
    }
    let ext: Vec<Value>;
    let row = if wide {
        ext = row
            .iter()
            .cloned()
            .chain([
                Value::Tid(u32::MAX, 0),
                Value::Oid(0),
                Value::Oid(0),
                Value::Oid(0),
                Value::Oid(0),
                Value::Oid(0),
            ])
            .collect();
        &ext[..]
    } else {
        row
    };
    for c in checks {
        if eval(&c.expr, row, ctx)? != Value::Bool(true) {
            let using = if c.conflict {
                " (USING expression)"
            } else {
                ""
            };
            let msg = match &c.policy {
                Some(p) => format!(
                    "new row violates row-level security policy \"{p}\"{using} for table \"{}\"",
                    c.table
                ),
                None => format!(
                    "new row violates row-level security policy{using} for table \"{}\"",
                    c.table
                ),
            };
            return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, msg));
        }
    }
    Ok(())
}

/// check_row with these row-level security checks instead of the rules' own.
pub fn check_row_rls(
    rules: &TableRules,
    rls: &[crate::sql::plan::RlsCheck],
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<()> {
    check_rls(rls, row, ctx)?;
    let t = &rules.table;
    // The values' domains are checked as the row is formed, before its constraints.
    for (i, d) in rules.domain_checks.iter().enumerate() {
        if let Some(d) = d {
            d.check(&row[i], ctx)?;
        }
    }
    // (A foreign table's constraints are the remote server's to enforce: ExecConstraints
    // runs for local tables only.)
    if t.foreign.is_some() {
        return Ok(());
    }
    for (i, c) in t.live_columns() {
        if c.not_null && row[i].is_null() {
            return Err(with_failing_row(
            Error::new(
                code::NOT_NULL_VIOLATION,
                format!(
                    "null value in column \"{}\" of relation \"{}\" violates not-null constraint",
                    c.name, t.name
                ),
            ),
            t,
            row,
            ctx,
        )
            .table(ctx.catalog().schema_name(t.schema), &t.name)
            .column(&c.name));
        }
    }
    for (name, expr) in &rules.checks {
        // CHECK passes on NULL (unknown).
        if eval(expr, row, ctx)? == Value::Bool(false) {
            return Err(with_failing_row(
            Error::new(
                code::CHECK_VIOLATION,
                format!(
                    "new row for relation \"{}\" violates check constraint \"{name}\"",
                    t.name
                ),
            ),
            t,
            row,
            ctx,
        )
            .table(ctx.catalog().schema_name(t.schema), &t.name)
            .constraint(name));
        }
    }
    // (The views' check options come after the row is written and its unique keys checked,
    // ExecInsert / ExecUpdate's WCO_VIEW_CHECK: `store_inserted`, `update_row_rls`. A
    // foreign table's row is checked here, before the server writes it.)
    if t.foreign.is_some() {
        return check_view_options(rules, row, ctx);
    }
    Ok(())
}

/// WITH CHECK OPTION of the views written through (on a row an INSTEAD OF trigger of the
/// view under them returned, too). They fail on NULL too.
pub fn check_view_options(rules: &TableRules, row: &[Value], ctx: &ExecCtx) -> Result<()> {
    for (view, expr) in &rules.view_checks {
        if eval(expr, row, ctx)? != Value::Bool(true) {
            return Err(with_failing_row(
            Error::new(
                code::WITH_CHECK_OPTION_VIOLATION,
                format!("new row violates check option for view \"{view}\""),
            ),
            &rules.table,
            row,
            ctx,
        ));
        }
    }
    Ok(())
}

/// Whether a row of a partition satisfies its partition constraint.
pub fn partition_ok(rules: &TableRules, row: &[Value], ctx: &ExecCtx) -> Result<bool> {
    let Some(levels) = &rules.part_check else {
        return Ok(true);
    };
    crate::sql::binder::partition::check_levels(levels, &mut |key| {
        key.iter().map(|e| eval(e, row, ctx)).collect()
    })
}

/// ExecPartitionCheck: the row must belong in the partition it is written to.
pub fn check_partition(rules: &TableRules, row: &[Value], ctx: &ExecCtx) -> Result<()> {
    if partition_ok(rules, row, ctx)? {
        return Ok(());
    }
    let t = &rules.table;
    Err(with_failing_row(
    Error::new(
        code::CHECK_VIOLATION,
        format!(
            "new row for relation \"{}\" violates partition constraint",
            t.name
        ),
    ),
    t,
    row,
    ctx,
)
    .table(ctx.catalog().schema_name(t.schema), &t.name))
}

/// CheckCmdReplicaIdentity for a result relation of an UPDATE / DELETE (or what updates).
pub fn check_replica_identity(
    t: &Table,
    cmd: crate::catalog::pubsub::RiCmd,
    ctx: &ExecCtx,
) -> Result<()> {
    let cat = ctx.catalog();
    if cat.publications.is_empty() {
        return Ok(());
    }
    cat.check_replica_identity(t, cmd)
}

/// ExecBuildSlotPartitionKeyDescription: whether the user may see the partition key's
/// values (not under row-level security; with SELECT on the table, or on every key column
/// when the key has no expressions).
fn partition_key_visible(t: &Table, ctx: &ExecCtx) -> bool {
    if t.rls
        && crate::sql::binder::Binder::new(ctx, Vec::new())
            .rls_enabled(&Arc::new(t.clone()))
            .unwrap_or(true)
    {
        return false;
    }
    let cat = ctx.catalog();
    let me = ctx.current_user();
    let db_owner = ctx.db_owner();
    if cat.is_superuser(me)
        || cat
            .rel_mask(t.oid, me, db_owner)
            .is_some_and(|m| m.0 & crate::catalog::acl::SELECT != 0)
    {
        return true;
    }
    t.partkey.as_ref().is_some_and(|k| {
        k.cols.iter().all(|c| {
            c.col.is_some_and(|i| {
                cat.col_mask(t.oid, i as i32 + 1, me, db_owner).0 & crate::catalog::acl::SELECT != 0
            })
        })
    })
}

/// The leaf partition a row (over the root's columns) goes to.
pub fn route_row(routing: &PartRouting, row: &[Value], ctx: &ExecCtx) -> Result<usize> {
    match crate::sql::binder::partition::route_row(&routing.router, &mut |key| {
        key.iter().map(|e| eval(e, row, ctx)).collect()
    })? {
        // A foreign partition's wrapper would take the row (ExecInitRoutingInfo).
        Ok(leaf) => match routing
            .leaves
            .get(leaf)
            .filter(|l| l.rules.table.is_foreign())
            .and_then(|l| ctx.catalog().fdw_modify_error(&l.rules.table, "insert into"))
        {
            Some(e) => Err(e),
            None => Ok(leaf),
        },
        Err((r, key)) => {
            let vals: Vec<String> = key
                .iter()
                .zip(&r.types)
                .map(|(v, ty)| match v {
                    Value::Null => "null".into(),
                    v => format_value(v, ty, ctx.fmt()),
                })
                .collect();
            let e = Error::new(
                code::CHECK_VIOLATION,
                format!(
                    "no partition of relation \"{}\" found for row",
                    r.table.name
                ),
            );
            let e = if partition_key_visible(&r.table, ctx) {
                e.detail(format!(
                    "Partition key of the failing row contains ({}) = ({}).",
                    r.key_names.join(", "),
                    vals.join(", ")
                ))
            } else {
                e
            };
            Err(e.table(ctx.catalog().schema_name(r.table.schema), &r.table.name))
        }
    }
}

/// A row over the root's columns as the leaf's (target columns and values).
fn to_leaf(leaf: &PartLeaf, row: Row) -> (Vec<usize>, Row) {
    let mut target = Vec::with_capacity(row.len());
    let mut vals = Vec::with_capacity(row.len());
    for (v, m) in row.into_iter().zip(&leaf.map) {
        if let Some(j) = m {
            target.push(*j);
            vals.push(v);
        }
    }
    (target, vals)
}

/// A leaf's row over the root's columns.
fn from_leaf(leaf: &PartLeaf, row: &[Value]) -> Row {
    leaf.map
        .iter()
        .map(|m| m.map_or(Value::Null, |j| row[j].clone()))
        .collect()
}

/// Insert a row (over the root's columns, defaults applied) through partition routing: the
/// root's own checks, then the leaf it belongs to. AFTER ROW events are queued per leaf.
pub fn insert_routed(
    routing: &PartRouting,
    rls: &[crate::sql::plan::RlsCheck],
    row: Row,
    after: &mut Vec<(usize, AfterRow)>,
    ctx: &ExecCtx,
) -> Result<Option<(usize, Row, Vec<u8>)>> {
    match insert_routed_oc(routing, rls, row, None, after, ctx)? {
        Routed::Stored(leaf, row, key) => Ok(Some((leaf, row, key))),
        _ => Ok(None),
    }
}

/// What became of a routed row.
pub enum Routed {
    Skipped,
    Stored(usize, Row, Vec<u8>),
    /// It collides with a row of the leaf on an arbiter index (the proposed row over the
    /// root's columns).
    Conflict(usize, Row, Conflict),
}

/// The leaf's indexes standing for the arbiter indexes of the partitioned table.
fn leaf_arbiters(leaf: &PartLeaf, arbiters: &[u32], ctx: &ExecCtx) -> Vec<u32> {
    if arbiters.is_empty() {
        return Vec::new();
    }
    let cat = ctx.catalog();
    leaf.rules
        .indexes
        .iter()
        .filter(|b| {
            let mut p = b.index.parent;
            while p != 0 {
                if arbiters.contains(&p) {
                    return true;
                }
                p = cat.indexes.get(&p).map_or(0, |i| i.parent);
            }
            false
        })
        .map(|b| b.index.oid)
        .collect()
}

pub fn insert_routed_oc(
    routing: &PartRouting,
    rls: &[crate::sql::plan::RlsCheck],
    row: Row,
    on_conflict: Option<&crate::sql::plan::OnConflict>,
    after: &mut Vec<(usize, AfterRow)>,
    ctx: &ExecCtx,
) -> Result<Routed> {
    check_rls(rls, &row, ctx)?;
    if let Some(levels) = &routing.check
        && !crate::sql::binder::partition::check_levels(levels, &mut |key| {
            key.iter().map(|e| eval(e, &row, ctx)).collect()
        })?
    {
        let t = &routing.router.table;
        return Err(with_failing_row(
        Error::new(
            code::CHECK_VIOLATION,
            format!(
                "new row for relation \"{}\" violates partition constraint",
                t.name
            ),
        ),
        t,
        &row,
        ctx,
    )
        .table(ctx.catalog().schema_name(t.schema), &t.name));
    }
    let leaf = route_row(routing, &row, ctx)?;
    let l = &routing.leaves[leaf];
    // ExecInitPartitionInfo opens the leaf a row goes to with RowExclusiveLock (an
    // ATTACH PARTITION changing a default partition holds it off).
    let before = ctx.catalog().version;
    {
        let name = l.rules.table.name.clone();
        ctx.txn.borrow_mut().lock_relation(
            l.rules.table.oid,
            crate::txn::locks::LockMode::RowExclusive,
            WaitPolicy::Block,
            &|| name.clone(),
        )?;
    }
    if ctx.catalog().version != before {
        recheck_default_partitions(routing, &row, ctx)?;
    }
    // ExecInitPartitionInfo's CheckValidResultRel: ON CONFLICT DO UPDATE updates the leaf.
    if on_conflict.is_some_and(|oc| matches!(oc.action, crate::sql::plan::ConflictAction::Update { .. })) {
        check_replica_identity(&l.rules.table, crate::catalog::pubsub::RiCmd::Update, ctx)?;
    }
    let (target, vals) = to_leaf(l, row);
    let mut queue = Vec::new();
    let r = insert_new(&l.rules, &target, vals, &[], leaf_on_conflict(l, on_conflict, ctx).as_ref(), &mut queue, ctx)?;
    after.extend(queue.into_iter().map(|a| (leaf, a)));
    Ok(routed(l, leaf, r))
}

/// ExecFindPartition after a wait for a partition's lock: a row routed to a default
/// partition is checked against that partition's constraint as other transactions left it
/// (a partition attached meanwhile may take its keys now): Postgres rechecks it rather than
/// routing again.
fn recheck_default_partitions(routing: &PartRouting, row: &[Value], ctx: &ExecCtx) -> Result<()> {
    use crate::catalog::partition::{PartBound, route};
    let cat = ctx.catalog();
    let mut r = &routing.router;
    loop {
        let key: Vec<Value> = r.key.iter().map(|e| eval(e, row, ctx)).collect::<Result<_>>()?;
        let _g = crate::catalog::partition::under(r.colls.clone());
        let Some(i) = route(r.parts.iter().map(|(b, _)| b), &key, &r.types) else {
            return Ok(());
        };
        let (bound, target) = &r.parts[i];
        let part = match target {
            crate::sql::plan::PartTarget::Leaf(l) => routing.leaves[*l].rules.table.clone(),
            crate::sql::plan::PartTarget::Sub(s) => s.table.clone(),
        };
        if matches!(bound, PartBound::Default) {
            let siblings: Vec<PartBound> = cat
                .tables
                .values()
                .filter(|t| t.oid != part.oid && t.inherits.first() == Some(&r.table.oid))
                .filter_map(|t| t.partbound.clone())
                .filter(|b| !matches!(b, PartBound::Default))
                .collect();
            if siblings.iter().any(|b| crate::catalog::partition::accepts(b, &key, &r.types)) {
                let now = cat.tables.get(&part.oid).cloned().unwrap_or_else(|| part.clone());
                let root = &routing.router.table;
                let as_part: Row = now
                    .columns
                    .iter()
                    .map(|c| {
                        root.columns
                            .iter()
                            .position(|rc| !rc.dropped && !c.dropped && rc.name == c.name)
                            .and_then(|j| row.get(j).cloned())
                            .unwrap_or(Value::Null)
                    })
                    .collect();
                return Err(with_failing_row(
                    Error::new(
                        code::CHECK_VIOLATION,
                        format!("new row for relation \"{}\" violates partition constraint", now.name),
                    ),
                    &now,
                    &as_part,
                    ctx,
                )
                .table(cat.schema_name(now.schema), &now.name));
            }
        }
        match target {
            crate::sql::plan::PartTarget::Leaf(_) => return Ok(()),
            crate::sql::plan::PartTarget::Sub(s) => r = s,
        }
    }
}

/// The ON CONFLICT clause as a leaf applies it (its own arbiter indexes).
fn leaf_on_conflict(
    l: &PartLeaf,
    on_conflict: Option<&crate::sql::plan::OnConflict>,
    ctx: &ExecCtx,
) -> Option<crate::sql::plan::OnConflict> {
    on_conflict.map(|oc| crate::sql::plan::OnConflict {
        arbiters: leaf_arbiters(l, &oc.arbiters, ctx),
        action: ConflictAction::Nothing,
    })
}

fn routed(l: &PartLeaf, leaf: usize, r: Inserted) -> Routed {
    match r {
        Inserted::Stored(r, key) => Routed::Stored(leaf, from_leaf(l, &r), key),
        Inserted::Conflict(r, c) => Routed::Conflict(leaf, from_leaf(l, &r), c),
        Inserted::Skipped => Routed::Skipped,
    }
}

/// An ON CONFLICT insertion into leaf `leaf` starting over from its prepared row (after
/// its conflicting row changed while it waited).
pub fn resume_routed(
    routing: &PartRouting,
    leaf: usize,
    p: Prepared,
    on_conflict: Option<&crate::sql::plan::OnConflict>,
    after: &mut Vec<(usize, AfterRow)>,
    ctx: &ExecCtx,
) -> Result<Routed> {
    let l = &routing.leaves[leaf];
    let mut queue = Vec::new();
    let r = insert_prepared(&l.rules, p, leaf_on_conflict(l, on_conflict, ctx).as_ref(), &mut queue, ctx)?;
    after.extend(queue.into_iter().map(|a| (leaf, a)));
    Ok(routed(l, leaf, r))
}

/// Fire the queued AFTER ROW events of routed rows, leaf by leaf in queue order.
pub fn finish_routed(
    routing: &PartRouting,
    after: Vec<(usize, AfterRow)>,
    ctx: &ExecCtx,
) -> Result<()> {
    let mut run: Vec<AfterRow> = Vec::new();
    let mut cur: Option<usize> = None;
    for (leaf, a) in after {
        if let Some(c) = cur
            && c != leaf
        {
            let l = &routing.leaves[c];
            trigger::finish(
                &l.rules.triggers,
                &l.rules.table,
                std::mem::take(&mut run),
                &[],
                &[],
                ctx,
            )?;
        }
        cur = Some(leaf);
        run.push(a);
    }
    if let Some(c) = cur {
        let l = &routing.leaves[c];
        trigger::finish(&l.rules.triggers, &l.rules.table, run, &[], &[], ctx)?;
    }
    Ok(())
}

/// Index key values for a row, or None if the row isn't in the (partial) index.
pub fn index_values(ix: &BoundIndex, row: &[Value], ctx: &ExecCtx) -> Result<Option<Vec<Value>>> {
    if let Some(p) = &ix.predicate
        && !eval_bool(p, row, ctx)?
    {
        return Ok(None);
    }
    let vals = eval_all(&ix.keys, row, ctx)?;
    // A made operator class's compress function checks the key it is given (intarray's
    // gist__int_ops refuses arrays too big for its ranges).
    if ix.index.columns.iter().any(|c| c.opclass >= crate::catalog::FIRST_USER_OID) {
        let cat = ctx.catalog();
        for (c, v) in ix.index.columns.iter().zip(&vals) {
            if c.opclass >= crate::catalog::FIRST_USER_OID {
                crate::ext::native::check_key_for(&cat, c.opclass, v, &c.opclass_options)?;
            }
        }
    }
    Ok(Some(vals))
}

/// A unique-key conflict: which index, and the existing row's data key.
pub struct Conflict {
    pub index: usize,
    pub existing: Vec<u8>,
    pub values: Vec<Value>,
    /// The proposed row as the table (the leaf, when routed) takes it, to start over from.
    pub prepared: Prepared,
}

/// How a unique check treats another transaction's in-progress index entry for the key, or
/// its update or delete of a row with the key (what Postgres's dirty snapshot sees).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dirty {
    /// Wait for the writer, then look again (_bt_check_unique; CEOUC_WAIT).
    Wait,
    /// A potential conflict, without waiting (UNIQUE_CHECK_PARTIAL: a speculative
    /// insertion's own entries, a deferrable constraint's).
    Partial,
}

/// A unique check made while adding an index entry for the key: the check takes the key's
/// lock, which stands for the entry until the transaction ends. `token`: the speculative
/// insertion adding it, while unfinished; `row`: the row (for others' index expressions).
pub struct Entering<'a> {
    pub token: Option<u32>,
    pub row: Option<&'a Arc<Vec<Value>>>,
    /// The row's storage key (its heap tuple, named by a wait's CONTEXT).
    pub key: Option<&'a [u8]>,
}

/// The lock of a unique key (`locks::UNIQUE_KEY`): per key value, or the whole index under a
/// nondeterministic collation (equal keys needn't be equal bytes).
fn unique_lock_key(t: &Table, bix: &BoundIndex, vals: &[Value], nondet: bool) -> Vec<u8> {
    let mut lock_key = vec![0, 0, 0, 0, 0, 0, 0, crate::txn::locks::UNIQUE_KEY];
    if nondet {
        lock_key.extend(index_prefix(t.oid, bix.index.id));
    } else {
        lock_key.extend(index_value_prefix(t, &bix.index, vals, &bix.colls));
    }
    lock_key
}

/// The nondeterministic collations of an index's keys whose entries aren't keyed under
/// them (a clustered primary key's data keys): a unique check looks through the rows. An
/// index's own entries are (`KeyColl`): equal keys share an entry prefix.
fn unkeyed_nondet(t: &Table, bix: &BoundIndex, ctx: &ExecCtx) -> Vec<Option<Arc<crate::types::collation::Collation>>> {
    if bix.index.id != 0 {
        return Vec::new();
    }
    crate::types::collation::index_nondet(&ctx.catalog(), t, &bix.index)
}

/// Whether evaluating the index's key or predicate can be seen (anything but columns).
fn index_has_exprs(bix: &BoundIndex) -> bool {
    bix.predicate.is_some() || bix.keys.iter().any(|k| !matches!(k, crate::sql::expr::Expr::Col(..)))
}

/// Look for a row other than `own` with the same unique key, as the index's uniqueness check
/// does over a dirty snapshot: another transaction's in-progress entry for the key (its
/// lock on the key) or its update or delete of the row found is waited for (or, `Partial`,
/// a conflict). `entering`: the check is for an entry being added (it takes the key's
/// lock). `form_existing`: check_exclusion_or_unique_constraint's FormIndexDatum over each
/// row it meets (the index expressions run on it). Some(empty key): an in-progress entry.
#[allow(clippy::too_many_arguments)]
fn find_conflict(
    t: &Table,
    bix: &BoundIndex,
    vals: &[Value],
    own: Option<&[u8]>,
    entering: Option<&Entering>,
    dirty: Dirty,
    form_existing: bool,
    ctx: &ExecCtx,
) -> Result<Option<Vec<u8>>> {
    if !bix.index.nulls_not_distinct && vals.iter().any(Value::is_null) {
        return Ok(None);
    }
    let nondet = unkeyed_nondet(t, bix, ctx);
    let lock_key = unique_lock_key(t, bix, vals, !nondet.is_empty());
    let exprs = form_existing && index_has_exprs(bix);
    let what = || format!("unique key in \"{}\"", t.name);
    loop {
        ctx.check()?;
        let mine = match entering {
            Some(e) => ctx.txn.borrow_mut().lock_unique_key(
                &lock_key,
                e.token,
                e.row.cloned(),
                WaitPolicy::SkipLocked,
                &what,
                e.key,
            )?,
            None => false,
        };
        if !mine {
            let holder = ctx.txn.borrow().unique_key_holder(&lock_key);
            if let Some(kh) = holder {
                if exprs && let Some(r) = &kh.row {
                    index_values(bix, r, ctx)?;
                }
                if dirty == Dirty::Partial {
                    return Ok(Some(Vec::new()));
                }
                let line = unique_wait_context(t, bix, entering, kh.row_key.as_deref(), ctx);
                ctx.txn.borrow_mut().wait_for_key_holder(&kh, line)?;
                continue;
            }
            if let Some(e) = entering {
                // Free, but others are queued for it: wait in line.
                ctx.txn.borrow_mut().lock_unique_key(
                    &lock_key,
                    e.token,
                    e.row.cloned(),
                    WaitPolicy::Block,
                    &what,
                    e.key,
                )?;
            }
        }
        let Some(dkey) = committed_conflict(t, bix, vals, own, &nondet, ctx)? else {
            return Ok(None);
        };
        let latest = if exprs { ctx.txn.borrow().get_latest(&dkey)? } else { None };
        if let Some((_, Some(bytes))) = latest {
            index_values(bix, &decode_table_row(t, &bytes)?, ctx)?;
        }
        // Being updated or deleted by a transaction in progress: what it does decides.
        let modifier = ctx.txn.borrow().row_modifier(&dkey);
        if let Some(h) = modifier {
            if dirty == Dirty::Partial {
                return Ok(Some(dkey));
            }
            let line = unique_wait_context(t, bix, entering, Some(&dkey), ctx);
            ctx.txn.borrow_mut().wait_for_xact(h).map_err(|e| e.inner_context(line))?;
            continue;
        }
        return Ok(Some(dkey));
    }
}

/// XactLockTableWait's CONTEXT for a unique check's wait (XLTW_InsertIndex): an entry being
/// added names its own heap tuple in the index (_bt_check_unique); ON CONFLICT's pre-check
/// names the tuple it found in the table (check_exclusion_or_unique_constraint; not when
/// that is another transaction's entry in progress, whose row key isn't kept).
fn unique_wait_context(
    t: &Table,
    bix: &BoundIndex,
    entering: Option<&Entering>,
    found: Option<&[u8]>,
    ctx: &ExecCtx,
) -> Option<String> {
    let (rel, key) = match entering {
        Some(e) => (bix.index.name.as_str(), e.key?),
        None => (t.name.as_str(), found?),
    };
    let (b, o) = row_tid(t, key, ctx);
    Some(format!("while inserting index tuple ({b},{o}) in relation \"{rel}\""))
}

/// A row version's heap TID as its ctid shows it: the one stored with it, else where the heap
/// model put it (another transaction's version in progress), else the key's stand-in (a row
/// not written yet: the unique checks run before the heap write).
fn row_tid(t: &Table, key: &[u8], ctx: &ExecCtx) -> (u32, u16) {
    let stored = ctx.txn.try_borrow().ok().and_then(|x| x.get(key).ok().flatten());
    if let Some(tid) = stored.as_deref().and_then(super::scan::stored_tid) {
        return tid;
    }
    if let Some(tid) = super::sizing::modelled_tid(t.oid, key, ctx) {
        return tid;
    }
    match super::scan::ctid_of(key) {
        Value::Tid(b, o) => (b, o),
        _ => (0, 1),
    }
}

/// A committed row (or one this transaction wrote) other than `own` with the unique key.
pub(crate) fn committed_conflict(
    t: &Table,
    bix: &BoundIndex,
    vals: &[Value],
    own: Option<&[u8]>,
    nondet: &[Option<Arc<crate::types::collation::Collation>>],
    ctx: &ExecCtx,
) -> Result<Option<Vec<u8>>> {
    // Under a nondeterministic collation, equal keys needn't be equal bytes: look through
    // the table's rows.
    if !nondet.is_empty() {
        let start = index_prefix(t.oid, 0);
        let rows: Vec<(Vec<u8>, bytes::Bytes)> = ctx
            .txn
            .borrow()
            .scan_latest(&start, &prefix_end(&start))
            .collect::<Result<_>>()?;
        for (dkey, bytes) in rows {
            ctx.check()?;
            if own == Some(dkey.as_slice()) {
                continue;
            }
            let Some(other) = index_values(bix, &decode_table_row(t, &bytes)?, ctx)? else {
                continue;
            };
            if crate::types::collation::keys_equal(
                vals,
                &other,
                nondet,
                bix.index.nulls_not_distinct,
            ) {
                return Ok(Some(dkey));
            }
        }
        return Ok(None);
    }
    if bix.index.id == 0 {
        // Primary key: the data key itself.
        let mut k = index_prefix(t.oid, 0);
        for v in vals {
            encode_value(&mut k, v, false);
        }
        if own == Some(k.as_slice()) {
            return Ok(None);
        }
        let latest = ctx.txn.borrow().get_latest(&k)?;
        return Ok(match latest {
            Some((_, Some(_))) => Some(k),
            _ => None,
        });
    }
    // record_image_ops columns (pg_opclass 10036) compare images: equal keys under the
    // encoding's record equality may still be distinct entries.
    let image: Vec<bool> = bix
        .index
        .columns
        .iter()
        .map(|c| c.opclass == RECORD_IMAGE_OPS)
        .collect();
    let prefix = index_value_prefix(t, &bix.index, vals, &bix.colls);
    let mut scan = ctx.txn.borrow().scan_latest(&prefix, &prefix_end(&prefix));
    for e in scan.by_ref() {
        let (_, dkey) = e?;
        if own == Some(dkey.as_ref()) {
            continue;
        }
        if image.contains(&true) {
            let latest = ctx.txn.borrow().get_latest(&dkey)?;
            if let Some((_, Some(bytes))) = latest
                && let Some(other) = index_values(bix, &decode_table_row(t, &bytes)?, ctx)?
                && !vals.iter().zip(&other).zip(&image).all(|((a, b), img)| {
                    if *img {
                        crate::sql::func::ops::record_image_equal(a, b)
                    } else {
                        true
                    }
                })
            {
                continue;
            }
        }
        return Ok(Some(dkey.to_vec()));
    }
    Ok(None)
}

/// An ON CONFLICT DO UPDATE starting its row over (never reaches a client).
const RETRY_INSERT: &str = "XXRTY";

/// The error COPY FROM's insert stops with when it has inserted every row read so far: the
/// caller reads the next batch into the feed and asks for the next row again
/// (`session::copy`; nothing of the insert's state changes on it).
pub const NEED_COPY_ROWS: &str = "XXCPY";

/// COPY FROM's next row (its values, line number and text); None: the data has all been
/// read and inserted.
fn next_copied(c: &CopyLines, ctx: &ExecCtx) -> Result<Option<(Row, (usize, String))>> {
    let next = c.feed.lock().unwrap().rows.pop_front();
    match next {
        Some((exprs, line, raw)) => Ok(Some((eval_all(&exprs, &[], ctx)?, (line, raw)))),
        None if c.feed.lock().unwrap().done => Ok(None),
        None => Err(Error::new(NEED_COPY_ROWS, "COPY FROM needs its next rows")),
    }
}

/// pg_opclass oid of record_image_ops.
const RECORD_IMAGE_OPS: u32 = 10036;

fn unique_violation(t: &Table, bix: &BoundIndex, vals: &[Value], ctx: &ExecCtx) -> Error {
    let name = bix
        .constraint
        .as_ref()
        .map_or(bix.index.name.as_str(), |c| c.name.as_str());
    let e = Error::new(
        code::UNIQUE_VIOLATION,
        format!("duplicate key value violates unique constraint \"{name}\""),
    );
    let e = if key_visible(t, bix, ctx) {
        e.detail(key_detail(t, bix, vals, ctx, "already exists."))
    } else {
        e
    };
    e.table(ctx.catalog().schema_name(t.schema), &t.name)
        .constraint(name)
}

/// How an exclusion check treats a conflicting entry, or a conflicting row's delete, that a
/// transaction in progress is writing (CEOUC_WAIT_MODE).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExclWait {
    /// Wait for the writer, then look again (CEOUC_WAIT).
    Wait,
    /// A potential conflict, without waiting (CEOUC_NOWAIT: a deferrable constraint's
    /// check as the row goes in).
    NoWait,
    /// A speculative insertion's arbiter (CEOUC_LIVELOCK_PREVENTING_WAIT): wait only for
    /// another speculative insertion of a newer transaction; anything else is a conflict.
    Livelock,
}

/// Look for a row other than `own` whose key values conflict with `vals` under an exclusion
/// constraint (every operator true). Returns the conflicting row's key values.
/// check_exclusion_or_unique_constraint over a dirty snapshot: `entering` (the row's storage
/// key) puts the row's entry in first (`Txn::hold_exclusion_entry`, as Postgres inserts its
/// index tuple before checking), so concurrent conflicting writers always see each other;
/// another writer's conflicting entry in progress, or a conflicting row a transaction in
/// progress is deleting, is waited for (XactLockTableWait, XLTW_RecheckExclusionConstr) and
/// the check starts over. Writers whose rows don't conflict don't wait for each other.
pub fn exclusion_conflict(
    t: &Table,
    bix: &BoundIndex,
    vals: &[Value],
    own: Option<&[u8]>,
    entering: Option<(&[u8], Option<u32>)>,
    mode: ExclWait,
    ctx: &ExecCtx,
) -> Result<Option<Vec<Value>>> {
    let Some(con) = &bix.constraint else {
        return Ok(None);
    };
    let Some(ops) = &con.exclusion else {
        return Ok(None);
    };
    if vals.iter().any(Value::is_null) {
        return Ok(None);
    }
    // Whether another entry's key values conflict with ours (every operator true).
    let conflicts = |other: &[Value]| -> Result<bool> {
        if other.iter().any(Value::is_null) {
            return Ok(false);
        }
        let mut pair = vals.to_vec();
        pair.extend(other.iter().cloned());
        for op in ops {
            if eval(op, &pair, ctx)? != Value::Bool(true) {
                return Ok(false);
            }
        }
        Ok(true)
    };
    let check = |dkey: &[u8], row: &[Value]| -> Result<Option<Vec<Value>>> {
        if own == Some(dkey) {
            return Ok(None);
        }
        let Some(other) = index_values(bix, row, ctx)? else {
            return Ok(None);
        };
        Ok(conflicts(&other)?.then_some(other))
    };
    if let Some((k, token)) = entering {
        ctx.txn.borrow_mut().hold_exclusion_entry(con.oid, k, Arc::new(vals.to_vec()), token)?;
    }
    let wait_context = |dkey: &[u8]| {
        let (b, o) = row_tid(t, dkey, ctx);
        Some(format!("while checking exclusion constraint on tuple ({b},{o}) in relation \"{}\"", t.name))
    };
    let by_value = con.leading_eq && bix.index.id != 0;
    'retry: loop {
        ctx.check()?;
        // Others' entries in progress (their rows aren't readable yet), taken before the
        // committed rows are read: a writer committing meanwhile is in one or the other.
        let pending = ctx.txn.borrow().exclusion_entries(con.oid);
        // Committed rows (and ours): only the leading `=` value's when it has one. A
        // conflicting one is a conflict whatever else is in progress (the scan's order
        // decides which Postgres meets first; it reports the same conflict, after waiting
        // for the others when they come first), unless a transaction in progress is
        // deleting or updating it: then what that one does decides.
        let rows: Vec<(Vec<u8>, bytes::Bytes)> = if by_value {
            let prefix = index_value_prefix(t, &bix.index, &vals[..1], &bix.colls);
            let dkeys: Vec<Vec<u8>> = ctx
                .txn
                .borrow()
                .scan_latest(&prefix, &prefix_end(&prefix))
                .map(|e| e.map(|(_, d)| d.to_vec()))
                .collect::<Result<_>>()?;
            let mut rows = Vec::with_capacity(dkeys.len());
            for dkey in dkeys {
                if let Some((_, Some(bytes))) = ctx.txn.borrow().get_latest(&dkey)? {
                    rows.push((dkey, bytes));
                }
            }
            rows
        } else {
            // The rows the index's search structure places near ours (`search`), else
            // every row.
            let conds: Vec<(usize, &str, Value)> = ops
                .iter()
                .enumerate()
                .filter_map(|(i, op)| match op {
                    crate::sql::expr::Expr::Call(def, args, _) if args.len() == 2 => Some((i, def.name, vals[i].clone())),
                    _ => None,
                })
                .collect();
            match super::search::candidates(t, bix, &conds, None, ctx)? {
                Some(super::search::Found::Keys(dkeys)) => {
                    let mut rows = Vec::with_capacity(dkeys.len());
                    for dkey in dkeys {
                        if let Some((_, Some(bytes))) = ctx.txn.borrow().get_latest(&dkey)? {
                            rows.push((dkey, bytes));
                        }
                    }
                    rows
                }
                _ => {
                    let start = index_prefix(t.oid, 0);
                    ctx.txn.borrow().scan_latest(&start, &prefix_end(&start)).collect::<Result<_>>()?
                }
            }
        };
        let mut modified = None;
        for (dkey, bytes) in rows {
            ctx.check()?;
            let Some(other) = check(&dkey, &decode_table_row(t, &bytes)?)? else { continue };
            let modifier = ctx.txn.borrow().row_modifier(&dkey);
            match modifier.filter(|_| mode == ExclWait::Wait) {
                Some(h) => modified = modified.or(Some((dkey, h))),
                None => return Ok(Some(other)),
            }
        }
        if let Some((dkey, h)) = modified {
            let line = wait_context(&dkey);
            ctx.txn.borrow_mut().wait_for_xact(h).map_err(|e| e.inner_context(line))?;
            continue 'retry;
        }
        for (dkey, kh) in pending {
            let Some(other) = kh.row.as_deref() else { continue };
            if own != Some(dkey.as_slice()) && conflicts(other)? {
                let wait = match mode {
                    ExclWait::Wait => true,
                    ExclWait::NoWait => false,
                    // Only an older speculative insertion waits for a newer one; the newer
                    // gives up its row and starts over, so the two never wait for each other.
                    ExclWait::Livelock => kh.token.is_some() && ctx.txn.borrow_mut().cur_xid() < kh.xid,
                };
                if !wait {
                    return Ok(Some(other.to_vec()));
                }
                let line = wait_context(&dkey);
                ctx.txn.borrow_mut().wait_for_key_holder(&kh, line)?;
                continue 'retry;
            }
        }
        return Ok(None);
    }
}

/// ExecWithoutOverlapsNotEmpty: the range part of a WITHOUT OVERLAPS key can't be empty
/// (checked before NULLs in the rest of the key let a row pass).
pub fn check_without_overlaps(t: &Table, bix: &BoundIndex, vals: &[Value]) -> Result<()> {
    if !bix.constraint.as_ref().is_some_and(|c| c.without_overlaps) {
        return Ok(());
    }
    let empty = match vals.last() {
        Some(Value::Range(r)) => r.empty,
        Some(Value::Multirange(m)) => m.is_empty(),
        _ => false,
    };
    if !empty {
        return Ok(());
    }
    let col = match bix.index.columns.last().map(|c| &c.key) {
        Some(IndexKey::Column(i)) => t.columns[*i].name.as_str(),
        _ => "",
    };
    Err(Error::new(
        code::CHECK_VIOLATION,
        format!(
            "empty WITHOUT OVERLAPS value found in column \"{col}\" in relation \"{}\"",
            t.name
        ),
    ))
}

fn exclusion_violation(
    t: &Table,
    bix: &BoundIndex,
    vals: &[Value],
    other: &[Value],
    ctx: &ExecCtx,
) -> Error {
    let name = bix
        .constraint
        .as_ref()
        .map_or(bix.index.name.as_str(), |c| c.name.as_str());
    let existing = key_detail(t, bix, other, ctx, "");
    let existing = existing.trim_end().trim_start_matches("Key ");
    Error::new(
        code::EXCLUSION_VIOLATION,
        format!("conflicting key value violates exclusion constraint \"{name}\""),
    )
    .detail(key_detail(
        t,
        bix,
        vals,
        ctx,
        &format!("conflicts with existing key {existing}."),
    ))
    .table(ctx.catalog().schema_name(t.schema), &t.name)
    .constraint(name)
}

/// What a unique or exclusion index finds for a new row's key values: a conflicting row's
/// key (and, for exclusion constraints, its values). A unique index's check is
/// `find_conflict`'s (`entering`, `dirty`, `form_existing` as there).
#[allow(clippy::too_many_arguments)]
fn index_conflict(
    t: &Table,
    bix: &BoundIndex,
    vals: &[Value],
    own: Option<&[u8]>,
    entering: Option<&Entering>,
    dirty: Dirty,
    form_existing: bool,
    ctx: &ExecCtx,
) -> Result<Option<(Vec<u8>, Option<Vec<Value>>)>> {
    if bix.index.unique {
        return Ok(find_conflict(t, bix, vals, own, entering, dirty, form_existing, ctx)?
            .map(|k| (k, None)));
    }
    let mode = match dirty {
        Dirty::Wait => ExclWait::Wait,
        Dirty::Partial => ExclWait::NoWait,
    };
    let entering = entering.and_then(|e| e.key).map(|k| (k, None));
    Ok(exclusion_conflict(t, bix, vals, own, entering, mode, ctx)?.map(|o| (Vec::new(), Some(o))))
}

fn index_violation(
    t: &Table,
    bix: &BoundIndex,
    vals: &[Value],
    other: Option<&[Value]>,
    ctx: &ExecCtx,
) -> Error {
    match other {
        Some(o) => exclusion_violation(t, bix, vals, o, ctx),
        None => unique_violation(t, bix, vals, ctx),
    }
}

/// Whether the index enforces a unique or exclusion constraint checked as rows are written.
fn checks_rows(bix: &BoundIndex) -> bool {
    bix.index.unique
        || bix
            .constraint
            .as_ref()
            .is_some_and(|c| c.exclusion.is_some())
}

/// A deferrable constraint: if the row with key values `vals` (stored at `dkey`) conflicts
/// now, queue a recheck of it (at statement end, or at commit while deferred) instead of
/// failing. `exclude`: the row's current entry, which isn't a conflict. False: not deferrable.
fn defer_recheck(
    t: &Table,
    bix: &BoundIndex,
    vals: &[Value],
    dkey: &[u8],
    exclude: Option<&[u8]>,
    ctx: &ExecCtx,
) -> Result<bool> {
    let Some(con) = bix.constraint.as_ref().filter(|c| c.deferral.0) else {
        return Ok(false);
    };
    if index_conflict(t, bix, vals, exclude, Some(&Entering { token: None, row: None, key: Some(dkey) }), Dirty::Partial, false, ctx)?.is_none() {
        return Ok(true);
    }
    let deferred = ctx.txn.borrow().is_deferred(con.oid, con.deferral);
    let item = DeferredFk {
        constraint: con.oid,
        table: t.oid,
        parent_side: false,
        values: Vec::new(),
        recheck: Some(dkey.to_vec()),
        stmt: !deferred,
    };
    ctx.txn.borrow_mut().deferred.push(item);
    Ok(true)
}

/// Recheck a row queued by `defer_recheck`, if it is still there.
fn recheck_row(t: &Table, bix: &BoundIndex, dkey: &[u8], ctx: &ExecCtx) -> Result<()> {
    let Some((_, Some(bytes))) = ctx.txn.borrow().get_latest(dkey)? else {
        return Ok(());
    };
    let row = decode_table_row(t, &bytes)?;
    let Some(vals) = index_values(bix, &row, ctx)? else {
        return Ok(());
    };
    if let Some((_, other)) = index_conflict(t, bix, &vals, Some(dkey), Some(&Entering { token: None, row: None, key: Some(dkey) }), Dirty::Wait, false, ctx)? {
        return Err(index_violation(t, bix, &vals, other.as_deref(), ctx));
    }
    Ok(())
}

/// Run the rechecks of deferrable constraints that are due when a statement ends.
pub fn check_statement_rechecks(ctx: &ExecCtx) -> Result<()> {
    if !ctx.txn.borrow().deferred.iter().any(|d| d.stmt) {
        return Ok(());
    }
    run_deferred(ctx, &|d| d.stmt)
}

/// Write a row's index entries.
pub fn put_index_entries(
    t: &Table,
    indexes: &[BoundIndex],
    row: &[Value],
    dkey: &[u8],
    ctx: &ExecCtx,
) -> Result<()> {
    let vals = indexes
        .iter()
        .map(|bix| if bix.index.id == 0 { Ok(None) } else { index_values(bix, row, ctx) })
        .collect::<Result<Vec<_>>>()?;
    put_index_entries_with(t, indexes, &vals, dkey, false, ctx)
}

/// Write a row's index entries from its index values (one per index). `fresh`: the row's
/// key was never used before, so neither were its entries' (`Txn::put_fresh`).
fn put_index_entries_with(
    t: &Table,
    indexes: &[BoundIndex],
    vals: &[Option<Vec<Value>>],
    dkey: &[u8],
    fresh: bool,
    ctx: &ExecCtx,
) -> Result<()> {
    for (bix, vals) in indexes.iter().zip(vals) {
        if bix.index.id == 0 {
            continue;
        }
        if am_insert(t, bix, vals.as_deref(), dkey, false, ctx)? {
            continue;
        }
        if let Some(vals) = vals {
            let k = index_entry_key(t, &bix.index, vals, dkey, &bix.colls);
            let tracked = ctx.txn.borrow().serializable_tracked();
            if tracked {
                let me = ctx.txn.borrow().id;
                ctx.sess.db.ssi.wrote_entry(me, bix.index.oid, &k);
            }
            if fresh {
                ctx.txn.borrow_mut().put_fresh(k, dkey.to_vec())?;
            } else {
                ctx.txn.borrow_mut().put(k, dkey.to_vec())?;
            }
        }
    }
    Ok(())
}

/// Points an SP-GiST index's root leaf page holds (8 KB pages: 36 bytes a point and its
/// line pointer): the next one splits it, and every insertion after goes through the inner
/// tuples.
pub const SPGIST_ROOT_POINTS: usize = 226;

/// Whether `bix` is an SP-GiST index over points with quad_point_ops (the default class).
pub fn quad_point_index(bix: &BoundIndex, ctx: &ExecCtx) -> bool {
    let [c] = &bix.index.columns[..] else { return false };
    bix.index.method == "spgist"
        && matches!(bix.keys.first().map(|k| k.ty()), Some(crate::types::DataType::Point))
        && (c.opclass == 0 || ctx.catalog().opclasses.values().any(|o| o.oid == c.opclass && o.name == "quad_point_ops"))
}

/// A point with a NaN coordinate: quad_point_ops' getQuadrant places it in no quadrant (nor
/// any point around a centroid computed with it).
pub fn nan_point(v: &Value) -> bool {
    matches!(v, Value::Geo(g) if matches!(&**g, crate::types::geo::Geo::Point(p) if p.x.is_nan() || p.y.is_nan()))
}

/// getQuadrant's error (spgquadtreeproc.c), raised where PostgreSQL meets it: splitting the
/// root leaf page with such a point on it, or choosing a quadrant for one once it is split.
pub fn quadrant_error() -> Error {
    crate::error::internal("getQuadrant: impossible case")
}

/// An insertion of `vals` into quad_point_ops index `bix`: a NaN point fails once the index
/// has more points than its root page holds, and the insertion that splits the root fails
/// with one on it.
fn quad_insert_check(t: &Table, bix: &BoundIndex, vals: Option<&[Value]>, ctx: &ExecCtx) -> Result<()> {
    let Some(v) = vals.and_then(|v| v.first()) else { return Ok(()) };
    if bix.index.method != "spgist" || v.is_null() || !quad_point_index(bix, ctx) {
        return Ok(());
    }
    let start = index_prefix(t.oid, bix.index.id);
    let nulls = index_value_prefix(t, &bix.index, &[Value::Null], &bix.colls);
    let mut dkeys = Vec::new();
    for r in ctx.txn.borrow().scan(&start, &prefix_end(&start)) {
        let (k, d) = r?;
        if !k.starts_with(&nulls) {
            dkeys.push(d);
            if dkeys.len() > SPGIST_ROOT_POINTS {
                break;
            }
        }
    }
    let split = match dkeys.len() {
        n if n < SPGIST_ROOT_POINTS => return Ok(()),
        n if n > SPGIST_ROOT_POINTS => false,
        _ => true,
    };
    if nan_point(v) {
        return Err(quadrant_error());
    }
    if split {
        // (The root's points, the new one's centroid with them.)
        for d in &dkeys {
            let Some(bytes) = ctx.txn.borrow().get(d)? else { continue };
            let row = decode_table_row(t, &bytes)?;
            if index_values(bix, &row, ctx)?.as_ref().and_then(|v| v.first()).is_some_and(nan_point) {
                return Err(quadrant_error());
            }
        }
    }
    Ok(())
}

/// aminsert for an index of a native access method (`ext::index_am`): whether `bix` is one.
/// Its entries are the method's own; a deleted row's stay until VACUUM, as in PostgreSQL.
/// (`update`: an UPDATE gave the row's key a new value: its older entries no longer hold it.)
fn am_insert(t: &Table, bix: &BoundIndex, vals: Option<&[Value]>, dkey: &[u8], update: bool, ctx: &ExecCtx) -> Result<bool> {
    quad_insert_check(t, bix, vals, ctx)?;
    if crate::ext::index_am::builtin_method(&bix.index.method) {
        return Ok(false);
    }
    let cat = ctx.catalog();
    let Some(r) = crate::ext::index_am::routine_by_name(&cat, &bix.index.method) else {
        return Ok(false);
    };
    let am = crate::ext::index_am::AmIndex::of(&cat, t, &bix.index, &bix.keys, ctx);
    if update {
        am.store.mark_updated(dkey)?;
    }
    if let Some(vals) = vals {
        let xid = ctx.txn.borrow_mut().cur_xid();
        crate::ext::index_am::note_insert(&am.store, dkey, xid, ctx.sess.db.xids().horizon());
        r.am.insert(&am, dkey, vals, ctx)?;
    }
    Ok(true)
}

fn delete_index_entries(
    t: &Table,
    indexes: &[BoundIndex],
    row: &[Value],
    dkey: &[u8],
    defer: bool,
    ctx: &ExecCtx,
) -> Result<()> {
    for bix in indexes {
        if bix.index.id == 0 {
            continue;
        }
        if !crate::ext::index_am::builtin_method(&bix.index.method)
            && crate::ext::index_am::routine_by_name(&ctx.catalog(), &bix.index.method).is_some()
        {
            continue;
        }
        if let Some(vals) = index_values(bix, row, ctx)? {
            let k = index_entry_key(t, &bix.index, &vals, dkey, &bix.colls);
            if defer {
                ctx.txn.borrow_mut().defer_entry_delete(k)?;
            } else {
                ctx.txn.borrow_mut().delete(k)?;
            }
        }
    }
    Ok(())
}

// ── foreign keys ──

/// A foreign key, as one of its internal triggers (`trigger::ri_triggers`) sees it.
pub struct RiCon {
    /// The trigger function (`trigger::ri_fn`).
    func: u32,
    /// The referencing table, and the constraint.
    child: Arc<Table>,
    con: crate::catalog::Constraint,
}

impl RiCon {
    pub fn of(trig: &crate::catalog::Trigger, ri: (u32, u32), ctx: &ExecCtx) -> Option<RiCon> {
        let child = ctx.catalog().tables.get(&ri.1)?.clone();
        let con = child.constraints.iter().find(|c| c.oid == ri.0)?.clone();
        Some(RiCon {
            func: trig.function,
            child,
            con,
        })
    }

    /// Whether an event on `t` needs the trigger, when it is queued: a referenced key that
    /// is NULL or unchanged needs no action, a referencing one that is NULL (MATCH SIMPLE) or
    /// unchanged no check (RI_FKey_pk_upd_check_required / RI_FKey_fk_upd_check_required).
    pub fn required(&self, event: u16, ev: &AfterRow, ctx: &ExecCtx) -> bool {
        let ConstraintKind::ForeignKey {
            columns,
            ref_columns,
            match_full,
            ..
        } = &self.con.kind
        else {
            return false;
        };
        let same = |cols: &[usize], a: &Row, b: &Row| {
            cols.iter()
                .all(|c| a[*c].cmp_total(&b[*c]) == std::cmp::Ordering::Equal)
        };
        if trigger::ri_fn::is_check(self.func) {
            let (true, Some(old), Some(new)) = (event == tt::UPDATE, &ev.old, &ev.new) else {
                return true;
            };
            let nulls = columns.iter().filter(|c| new[**c].is_null()).count();
            if nulls > 0 {
                return *match_full && nulls < columns.len();
            }
            // With PERIOD, a period that stayed or shrank needs no check (`new <@ old`).
            if self.con.is_period()
                && let Some((p, scalar)) = columns.split_last()
            {
                if old[*p].is_null() {
                    return true;
                }
                return !same(scalar, old, new)
                    || !period_within(&new[*p], &period_ranges(&old[*p]));
            }
            if !same(columns, old, new) {
                return true;
            }
            // A row this transaction made is checked all the same: its own check, still
            // deferred, will find it gone (RI_FKey_fk_upd_check_required's xmin test).
            let all = |a: &Row, b: &Row| {
                a.len() == b.len()
                    && a.iter().zip(b).all(|(x, y)| x.cmp_total(y) == std::cmp::Ordering::Equal)
            };
            return ctx.txn.borrow().deferred_triggers.iter().any(|d| {
                d.constraint == self.con.oid && d.new.as_ref().is_some_and(|n| all(n, old))
            });
        }
        let Some(old) = &ev.old else {
            return false;
        };
        if ref_columns.iter().any(|c| old[*c].is_null()) {
            return false;
        }
        // (ri_KeysEqual on the referenced side compares images: -0 to 0 is a change.)
        let image = |cols: &[usize], a: &Row, b: &Row| {
            cols.iter().all(|c| format!("{:?}", a[*c]) == format!("{:?}", b[*c]))
        };
        ev.new
            .as_ref()
            .is_none_or(|new| !image(ref_columns, old, new))
    }

    /// Run the trigger for an event on `t`: check a referencing row, or act on the rows
    /// referencing a deleted or updated one.
    pub fn fire(&self, t: &Table, event: u16, ev: &AfterRow, ctx: &ExecCtx) -> Result<()> {
        let ConstraintKind::ForeignKey { columns, .. } = &self.con.kind else {
            return Ok(());
        };
        if trigger::ri_fn::is_check(self.func) {
            let Some(new) = &ev.new else {
                return Ok(());
            };
            // A row deleted or changed since needs no check (of this version).
            if !ev.key.is_empty() {
                let live = match ctx.txn.borrow().get_latest(&ev.key)? {
                    Some((_, Some(bytes))) => {
                        let row = decode_table_row(t, &bytes)?;
                        columns
                            .iter()
                            .all(|c| row[*c].cmp_total(&new[*c]) == std::cmp::Ordering::Equal)
                    }
                    _ => false,
                };
                if !live {
                    return Ok(());
                }
            }
            return check_fk(t, &self.con, new, ctx);
        }
        let Some(old) = &ev.old else {
            return Ok(());
        };
        let new = if event == tt::UPDATE {
            ev.new.as_deref()
        } else {
            None
        };
        let action = match self.func {
            trigger::ri_fn::RESTRICT_DEL | trigger::ri_fn::RESTRICT_UPD => FkAction::Restrict,
            trigger::ri_fn::CASCADE_DEL | trigger::ri_fn::CASCADE_UPD => FkAction::Cascade,
            trigger::ri_fn::SETNULL_DEL | trigger::ri_fn::SETNULL_UPD => FkAction::SetNull,
            trigger::ri_fn::SETDEFAULT_DEL | trigger::ri_fn::SETDEFAULT_UPD => FkAction::SetDefault,
            _ => FkAction::NoAction,
        };
        fk_parent_action(t, &self.child, &self.con, action, old, new, ctx)
    }
}

/// RI_FKey_check: a referencing row's key must have its referenced row (MATCH SIMPLE: a
/// NULL in it passes; MATCH FULL: only all NULL does).
fn check_fk(
    t: &Table,
    con: &crate::catalog::Constraint,
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<()> {
    let ConstraintKind::ForeignKey {
        columns,
        ref_table,
        ref_columns,
        match_full,
        ..
    } = &con.kind
    else {
        return Ok(());
    };
    if con.is_period() {
        let cat = ctx.catalog();
        let Some(parent) = cat.tables.get(ref_table).cloned() else {
            return Ok(());
        };
        return check_fk_period(t, &parent, columns, ref_columns, &con.name, *match_full, row, ctx);
    }
    let vals: Vec<Value> = columns.iter().map(|c| row[*c].clone()).collect();
    let nulls = vals.iter().filter(|v| v.is_null()).count();
    if nulls > 0 {
        if *match_full && nulls != vals.len() {
            return Err(Error::new(
                code::FOREIGN_KEY_VIOLATION,
                format!(
                    "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                    t.name, con.name
                ),
            )
            .detail("MATCH FULL does not allow mixing of null and nonnull key values.")
            .table(ctx.catalog().schema_name(t.schema), &t.name)
            .constraint(&con.name));
        }
        return Ok(());
    }
    let cat = ctx.catalog();
    let Some(parent) = cat.tables.get(ref_table).cloned() else {
        return Ok(());
    };
    // RI_FKey_check: over the transaction snapshot in REPEATABLE READ unless the referenced
    // table is partitioned (detectNewRows).
    let xact_snapshot = parent.partkey.is_none() && ctx.txn.borrow().isolation.txn_snapshot();
    // (Its query, made only for an error while it looks, such as its lock wait's: the error
    // names it.)
    let found = match find_parent_tree(&parent, ref_columns, &vals, xact_snapshot, ctx) {
        Ok(found) => found,
        Err(e) => {
            let frame = ri_check_query(&cat, &parent, t, columns, ref_columns);
            return ctx.sess.framed(frame, |_| Err(e));
        }
    };
    if !found {
        return Err(child_violation(t, con, &parent, &vals, ctx));
    }
    Ok(())
}

fn child_violation(
    t: &Table,
    con: &crate::catalog::Constraint,
    parent: &Table,
    vals: &[Value],
    ctx: &ExecCtx,
) -> Error {
    let ConstraintKind::ForeignKey { columns, .. } = &con.kind else {
        return crate::error::internal("not a foreign key");
    };
    child_violation_of(t, columns, &con.name, parent, vals, ctx)
}

fn child_violation_of(
    t: &Table,
    columns: &[usize],
    conname: &str,
    parent: &Table,
    vals: &[Value],
    ctx: &ExecCtx,
) -> Error {
    let con_name = conname;
    let names: Vec<&str> = columns
        .iter()
        .map(|c| t.columns[*c].name.as_str())
        .collect();
    let shown: Vec<String> = vals
        .iter()
        .zip(columns)
        .map(|(v, c)| format_value(v, &t.columns[*c].ty, ctx.fmt()))
        .collect();
    Error::new(
        code::FOREIGN_KEY_VIOLATION,
        format!(
            "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
            t.name, con_name
        ),
    )
    .detail(if ri_key_visible(t, columns, ctx) {
        format!(
            "Key ({})=({}) is not present in table \"{}\".",
            names.join(", "),
            shown.join(", "),
            parent.name
        )
    } else {
        format!("Key is not present in table \"{}\".", parent.name)
    })
    .table(ctx.catalog().schema_name(t.schema), &t.name)
    .constraint(con_name)
}

/// ri_ReportViolation's check: the key's values are shown only to a user who may read them
/// (not under row-level security; SELECT on the table or on each key column).
fn ri_key_visible(t: &Table, columns: &[usize], ctx: &ExecCtx) -> bool {
    let cat = ctx.catalog();
    if t.rls
        && crate::sql::binder::Binder::new(ctx, Vec::new())
            .rls_enabled(t)
            .unwrap_or(true)
    {
        return false;
    }
    let me = ctx.current_user();
    let db_owner = ctx.db_owner();
    cat.is_superuser(me)
        || cat
            .rel_mask(t.oid, me, db_owner)
            .is_some_and(|m| m.0 & crate::catalog::acl::SELECT != 0)
        || columns.iter().all(|c| {
            cat.col_mask(t.oid, *c as i32 + 1, me, db_owner).0 & crate::catalog::acl::SELECT != 0
        })
}

// ── foreign keys using PERIOD ──

/// A period value's ranges: a range (none when empty) or a multirange's.
fn period_ranges(v: &Value) -> Vec<crate::types::range::Range> {
    match v {
        Value::Range(r) if r.empty => Vec::new(),
        Value::Range(r) => vec![(**r).clone()],
        Value::Multirange(m) => (**m).clone(),
        _ => Vec::new(),
    }
}

/// The range type of a period column.
fn period_type(ty: &DataType) -> Option<Arc<crate::types::range::RangeType>> {
    match ty {
        DataType::Range(r) | DataType::Multirange(r) => Some(r.clone()),
        _ => None,
    }
}

/// `a && b` for ranges or multiranges.
fn period_overlaps(a: &Value, b: &Value) -> bool {
    let (a, b) = (period_ranges(a), period_ranges(b));
    a.iter()
        .any(|x| b.iter().any(|y| crate::types::range::overlaps(x, y)))
}

/// `v <@ agg`, `agg` a normalised multirange (range_agg).
fn period_within(v: &Value, agg: &[crate::types::range::Range]) -> bool {
    period_ranges(v)
        .iter()
        .all(|x| agg.iter().any(|y| crate::types::range::contains(y, x)))
}

/// `a * b` (both ranges, or both multiranges).
fn period_intersect(
    rt: &crate::types::range::RangeType,
    a: &Value,
    b: &Value,
) -> Result<Value> {
    use crate::types::range;
    Ok(match (a, b) {
        (Value::Range(x), Value::Range(y)) => range::value(range::intersect(rt, x, y)?),
        _ => range::multi_value(range::multi_intersect(
            rt,
            &period_ranges(a),
            &period_ranges(b),
        )?),
    })
}

/// range_agg of the periods of `parent`'s rows (and its partitions') whose scalar key
/// (`cols` but the last) equals `vals` and whose period (the last of `cols`) overlaps
/// `period`, share-locking them (FOR KEY SHARE). None: no such row (range_agg is NULL).
fn period_agg(
    parent: &Arc<Table>,
    cols: &[usize],
    vals: &[Value],
    period: &Value,
    ctx: &ExecCtx,
) -> Result<Option<Vec<crate::types::range::Range>>> {
    let n = cols.len();
    let Some(rt) = cols.last().and_then(|c| period_type(&parent.columns[*c].ty)) else {
        return Ok(None);
    };
    let _coll = crate::types::range::under(rt.coll());
    let mut all = Vec::new();
    let mut any = false;
    for (leaf, lcols, rows) in scan_tree(parent, cols, &vals[..n - 1], ctx, false)? {
        for (dkey, _) in rows {
            let xact_ctx = super::scan::xltw_context(Xltw::Lock, &leaf.name, &dkey, ctx);
            let got = ctx
                .txn
                .borrow_mut()
                .lock_row(leaf.oid, &dkey, RowLockMode::KeyShare, WaitPolicy::Block, &|| {
                    leaf.name.clone()
                }, &xact_ctx)?;
            if !got {
                continue;
            }
            let Some((_, Some(bytes))) = ctx.txn.borrow().get_latest(&dkey)? else {
                continue;
            };
            let row = decode_table_row(&leaf, &bytes)?;
            let p = &row[lcols[n - 1]];
            if p.is_null() || !period_overlaps(p, period) {
                continue;
            }
            any = true;
            all.extend(period_ranges(p));
        }
    }
    if !any {
        return Ok(None);
    }
    Ok(Some(crate::types::range::normalize(&rt, all)?))
}

/// RI_FKey_check for a foreign key using PERIOD: the referenced rows with the key must
/// together cover the referencing row's period
/// (`HAVING $n <@ range_agg(pkperiod)` over those overlapping it).
#[allow(clippy::too_many_arguments)]
pub fn check_fk_period(
    t: &Table,
    parent: &Arc<Table>,
    columns: &[usize],
    ref_columns: &[usize],
    conname: &str,
    match_full: bool,
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<()> {
    let vals: Vec<Value> = columns.iter().map(|c| row[*c].clone()).collect();
    let nulls = vals.iter().filter(|v| v.is_null()).count();
    if nulls > 0 {
        if match_full && nulls != vals.len() {
            return Err(Error::new(
                code::FOREIGN_KEY_VIOLATION,
                format!(
                    "insert or update on table \"{}\" violates foreign key constraint \"{conname}\"",
                    t.name
                ),
            )
            .detail("MATCH FULL does not allow mixing of null and nonnull key values.")
            .table(ctx.catalog().schema_name(t.schema), &t.name)
            .constraint(conname));
        }
        return Ok(());
    }
    let period = &vals[vals.len() - 1];
    let covered = period_agg(parent, ref_columns, &vals, period, ctx)?
        .is_some_and(|agg| period_within(period, &agg));
    if !covered {
        return Err(child_violation_of(t, columns, conname, parent, &vals, ctx));
    }
    Ok(())
}

/// ri_restrict (NO ACTION) for a foreign key using PERIOD, after a referenced row with key
/// `oldvals` was deleted or updated: the referencing rows overlapping its old period must
/// still have that part covered by the referenced rows now there.
fn fk_parent_period(
    t: &Table,
    child: &Arc<Table>,
    con: &crate::catalog::Constraint,
    oldvals: &[Value],
    ctx: &ExecCtx,
) -> Result<()> {
    let ConstraintKind::ForeignKey {
        columns,
        ref_table,
        ref_columns,
        ..
    } = &con.kind
    else {
        return Ok(());
    };
    let n = oldvals.len();
    let cat = ctx.catalog();
    let Some(parent) = cat.tables.get(ref_table).cloned() else {
        return Ok(());
    };
    let Some(rt) = period_type(&t.columns[ref_columns[n - 1]].ty) else {
        return Ok(());
    };
    let _coll = crate::types::range::under(rt.coll());
    let old_period = &oldvals[n - 1];
    let mut replacement = None;
    for (_, lcols, rows) in scan_tree(child, columns, &oldvals[..n - 1], ctx, false)? {
        for (_, crow) in rows {
            let p = &crow[lcols[n - 1]];
            if p.is_null() || !period_overlaps(old_period, p) {
                continue;
            }
            if replacement.is_none() {
                replacement = Some(period_agg(&parent, ref_columns, oldvals, old_period, ctx)?);
            }
            let part = period_intersect(&rt, p, old_period)?;
            let ok = replacement
                .as_ref()
                .and_then(|r| r.as_ref())
                .is_some_and(|agg| period_within(&part, agg));
            if !ok {
                return Err(parent_violation(t, child, con, oldvals, false, ctx));
            }
        }
    }
    Ok(())
}

fn parent_violation(
    t: &Table,
    child: &Table,
    con: &crate::catalog::Constraint,
    oldvals: &[Value],
    restrict: bool,
    ctx: &ExecCtx,
) -> Error {
    let ConstraintKind::ForeignKey { ref_columns, .. } = &con.kind else {
        return crate::error::internal("not a foreign key");
    };
    let names: Vec<&str> = ref_columns
        .iter()
        .map(|c| t.columns[*c].name.as_str())
        .collect();
    let shown: Vec<String> = oldvals
        .iter()
        .zip(ref_columns)
        .map(|(v, c)| format_value(v, &t.columns[*c].ty, ctx.fmt()))
        .collect();
    let key = if ri_key_visible(t, ref_columns, ctx) {
        format!("Key ({})=({})", names.join(", "), shown.join(", "))
    } else {
        "Key".to_string()
    };
    if restrict {
        return Error::new(
            code::RESTRICT_VIOLATION,
            format!(
                "update or delete on table \"{}\" violates RESTRICT setting of foreign key constraint \"{}\" on table \"{}\"",
                t.name, con.name, child.name
            ),
        )
        .detail(format!("{key} is referenced from table \"{}\".", child.name))
        .table(ctx.catalog().schema_name(child.schema), &child.name)
        .constraint(&con.name);
    }
    Error::new(
        code::FOREIGN_KEY_VIOLATION,
        format!("update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"", t.name, con.name, child.name),
    )
    .detail(format!("{key} is still referenced from table \"{}\".", child.name))
    .table(ctx.catalog().schema_name(child.schema), &child.name)
    .constraint(&con.name)
}

/// Run the deferred constraint rechecks (all, or those of `only`), removing them from the
/// queue.
pub fn check_deferred(ctx: &ExecCtx, only: Option<&HashSet<u32>>) -> Result<()> {
    run_deferred(ctx, &|d| only.is_none_or(|o| o.contains(&d.constraint)))
}

fn run_deferred(ctx: &ExecCtx, due: &dyn Fn(&DeferredFk) -> bool) -> Result<()> {
    let pending: Vec<DeferredFk> = {
        let mut txn = ctx.txn.borrow_mut();
        let (now, keep) = std::mem::take(&mut txn.deferred)
            .into_iter()
            .partition(|d| due(d));
        txn.deferred = keep;
        now
    };
    let cat = ctx.catalog();
    let mut seen = HashSet::new();
    let mut bound: std::collections::HashMap<u32, Option<BoundIndex>> =
        std::collections::HashMap::new();
    for d in pending {
        // (Foreign keys are checked by their triggers, deferred as trigger events.)
        if let Some(dkey) = &d.recheck {
            if !seen.insert((d.constraint, false, format!("{dkey:?}"))) {
                continue;
            }
            let Some(t) = cat.tables.get(&d.table).cloned() else {
                continue;
            };
            if let std::collections::hash_map::Entry::Vacant(e) = bound.entry(d.constraint) {
                let bix = crate::sql::binder::bound_indexes(&t, ctx)?
                    .into_iter()
                    .find(|b| b.constraint.as_ref().is_some_and(|c| c.oid == d.constraint));
                e.insert(bix);
            }
            if let Some(Some(bix)) = bound.get(&d.constraint) {
                recheck_row(&t, bix, dkey, ctx)?;
            }
        }
    }
    Ok(())
}

/// A table's leaf partitions (itself when not partitioned), with `cols` as theirs.
fn fk_leaves(t: &Arc<Table>, cols: &[usize], ctx: &ExecCtx) -> Vec<(Arc<Table>, Vec<usize>)> {
    if t.partkey.is_none() {
        return vec![(t.clone(), cols.to_vec())];
    }
    // (ri_PerformCheck plans with a fresh snapshot when the referenced table is
    // partitioned: a partition whose detach is pending is out even for a snapshot
    // transaction that began before.)
    let cat = ctx.txn.borrow().catalog_latest();
    cat.leaves(t.oid)
        .into_iter()
        .filter_map(|l| {
            let m: Option<Vec<usize>> = cols
                .iter()
                .map(|c| l.column_index(&t.columns[*c].name))
                .collect();
            m.map(|m| (l, m))
        })
        .collect()
}

/// find_parent over a partitioned table's partitions.
fn find_parent_tree(
    parent: &Arc<Table>,
    cols: &[usize],
    vals: &[Value],
    xact_snapshot: bool,
    ctx: &ExecCtx,
) -> Result<bool> {
    for (leaf, lcols) in fk_leaves(parent, cols, ctx) {
        if find_parent(&leaf, &lcols, vals, xact_snapshot, ctx)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// scan_matching over a partitioned table's partitions: (partition, its columns, rows).
#[allow(clippy::type_complexity)]
fn scan_tree(
    t: &Arc<Table>,
    cols: &[usize],
    vals: &[Value],
    ctx: &ExecCtx,
    first_only: bool,
) -> Result<Vec<(Arc<Table>, Vec<usize>, Vec<(Vec<u8>, Row)>)>> {
    let mut out = Vec::new();
    for (leaf, lcols) in fk_leaves(t, cols, ctx) {
        let rows = scan_matching(&leaf, &lcols, vals, ctx, first_only)?;
        if !rows.is_empty() {
            out.push((leaf, lcols, rows));
            if first_only {
                break;
            }
        }
    }
    Ok(out)
}

/// Find (and share-lock) the parent row with `cols = vals`, as RI_FKey_check's
/// `SELECT 1 FROM ONLY pk WHERE ... FOR KEY SHARE` does. `xact_snapshot`: it runs over the
/// transaction snapshot (REPEATABLE READ / SERIALIZABLE, a referenced table that isn't
/// partitioned: ri_PerformCheck's detectNewRows is false), else over the latest committed
/// rows.
fn find_parent(
    parent: &Table,
    cols: &[usize],
    vals: &[Value],
    xact_snapshot: bool,
    ctx: &ExecCtx,
) -> Result<bool> {
    let cat = ctx.catalog();
    // Cast values to the parent's column types so keys encode identically.
    let vals: Vec<Value> = vals
        .iter()
        .zip(cols)
        .map(|(v, c)| coerce_key(v, &parent.columns[*c].ty, ctx))
        .collect::<Result<_>>()?;
    let nondet = ri_colls(parent, cols, ctx).is_some();
    let lookup = |dkey: &[u8]| -> Result<bool> {
        if xact_snapshot {
            // A row the snapshot doesn't see isn't there; one changed since is
            // ExecLockRows's serialization failure (either way, "concurrent update").
            if ctx.txn.borrow().get(dkey)?.is_none() {
                return Ok(false);
            }
            return match super::scan::lock_row_mode(parent, dkey, RowLockMode::KeyShare, Wait::Block, Xltw::Lock, ctx) {
                Ok(Locked::Same) => Ok(true),
                Ok(_) => Ok(false),
                Err(e) if e.code == code::SERIALIZATION_FAILURE => Err(Error::new(
                    code::SERIALIZATION_FAILURE,
                    "could not serialize access due to concurrent update",
                )),
                Err(e) => Err(e),
            };
        }
        // (The version the latest snapshot sees, before any wait.)
        let seen = ctx.txn.borrow().get_latest(dkey)?.map(|(ts, _)| ts);
        let xact_ctx = super::scan::xltw_context(Xltw::Lock, &parent.name, dkey, ctx);
        let got = ctx
            .txn
            .borrow_mut()
            .lock_row(parent.oid, dkey, RowLockMode::KeyShare, WaitPolicy::Block, &|| {
                parent.name.clone()
            }, &xact_ctx)?;
        if !got {
            return Ok(false);
        }
        let txn = ctx.txn.borrow();
        let latest = txn.get_latest(dkey)?;
        let enc = |v: &Value| {
            let mut out = Vec::new();
            encode_value(&mut out, v, false);
            out
        };
        let key_matches = |bytes: &[u8]| -> Result<bool> {
            if nondet {
                return Ok(true);
            }
            let row = decode_table_row(parent, bytes)?;
            Ok(cols
                .iter()
                .zip(&vals)
                .all(|(c, v)| row.get(*c).is_some_and(|r| enc(r) == enc(v))))
        };
        // REPEATABLE READ (a partitioned referenced table, read over the latest snapshot):
        // a row deleted, or its key changed, by the transaction the lock waited for is
        // ExecLockRows's serialization failure, not a missing row.
        if txn.isolation.txn_snapshot() && latest.as_ref().map(|(ts, _)| *ts) != seen {
            let gone = match &latest {
                Some((_, Some(b))) => !key_matches(b)?,
                _ => true,
            };
            if gone {
                return Err(Error::new(
                    code::SERIALIZATION_FAILURE,
                    "could not serialize access due to concurrent update",
                ));
            }
        }
        match latest {
            // Moved to another partition by the update it waited for: heap_lock_tuple's
            // error (EvalPlanQual can't follow it).
            Some((ts, None)) if txn.moved_at(dkey, ts) => Err(Error::new(
                code::SERIALIZATION_FAILURE,
                super::scan::MOVED_TO_PARTITION,
            )),
            // The newest version, rechecked against the key (EvalPlanQual).
            Some((_, Some(bytes))) => key_matches(&bytes),
            _ => Ok(false),
        }
    };
    // Keys under a nondeterministic collation match more than their bytes: scan.
    if nondet {
        for (dkey, _) in scan_matching(parent, cols, &vals, ctx, false)? {
            if lookup(&dkey)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if parent.storage_pkey().map(|p| p.as_slice()) == Some(cols) {
        let mut k = index_prefix(parent.oid, 0);
        for v in &vals {
            encode_value(&mut k, v, false);
        }
        return lookup(&k);
    }
    // (The primary key's index too: `pkey_index` isn't among `indexes`.)
    let pkey = parent.pkey_index.and_then(|p| cat.indexes.get(&p).cloned()).filter(|ix| ix.id != 0);
    for ix in pkey.into_iter().chain(cat.table_indexes(parent)) {
        let ixcols: Vec<Option<usize>> = ix
            .columns
            .iter()
            .map(|c| {
                if let IndexKey::Column(i) = c.key {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        if ix.unique
            && ix.predicate.is_none()
            && ixcols.len() == cols.len()
            && ixcols.iter().zip(cols).all(|(a, b)| *a == Some(*b))
        {
            let colls = crate::sql::binder::column_key_colls(&cat, parent, &ix);
            let prefix = index_value_prefix(parent, &ix, &vals, &colls);
            let entries: Vec<Vec<u8>> = {
                let txn = ctx.txn.borrow();
                let scan = if xact_snapshot {
                    txn.scan(&prefix, &prefix_end(&prefix))
                } else {
                    txn.scan_latest(&prefix, &prefix_end(&prefix))
                };
                scan.map(|e| e.map(|(_, v)| v.to_vec())).collect::<Result<_>>()?
            };
            for dkey in entries {
                if lookup(&dkey)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
    }
    // No usable unique index: scan (the row found is locked FOR KEY SHARE as well).
    for (dkey, _) in scan_matching(parent, cols, &vals, ctx, false)? {
        if lookup(&dkey)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn coerce_key(v: &Value, ty: &DataType, ctx: &ExecCtx) -> Result<Value> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    // Values of other types (ranges, arrays, ...) are stored the same whatever the
    // column's exact type: only the scalars below need a cast.
    let Some(from) = value_type(v) else {
        return Ok(v.clone());
    };
    if from.same_base(ty) {
        return Ok(v.clone());
    }
    crate::types::cast::cast_value(v.clone(), &from, ty, ctx.fmt(), false)
}

fn value_type(v: &Value) -> Option<DataType> {
    Some(match v {
        Value::Bool(_) => DataType::Bool,
        Value::Int2(_) => DataType::Int2,
        Value::Int4(_) => DataType::Int4,
        Value::Int8(_) => DataType::Int8,
        Value::Float4(_) => DataType::Float4,
        Value::Float8(_) => DataType::Float8,
        Value::Numeric(_) => DataType::Numeric(-1),
        Value::Date(_) => DataType::Date,
        Value::Timestamp(_) => DataType::Timestamp(-1),
        Value::Timestamptz(_) => DataType::Timestamptz(-1),
        Value::Uuid(_) => DataType::Uuid,
        Value::Text(_) => DataType::Text,
        _ => return None,
    })
}

/// Per column of `cols`, the nondeterministic collation its values compare under (the RI
/// queries compare keys under the key columns' collation); None when none has one.
fn ri_colls(
    t: &Table,
    cols: &[usize],
    ctx: &ExecCtx,
) -> Option<Vec<Option<Arc<crate::types::collation::Collation>>>> {
    let cat = ctx.catalog();
    if cat.collations.values().all(|d| d.deterministic) {
        return None;
    }
    let v: Vec<_> = cols
        .iter()
        .map(|c| {
            let name = t.columns.get(*c)?.collation.clone()?;
            crate::types::collation::by_name(&cat, &name).filter(|k| !k.deterministic)
        })
        .collect();
    v.iter().any(Option::is_some).then_some(v)
}

/// Rows of `t` whose `cols` equal `vals` (latest committed state plus our writes).
fn scan_matching(
    t: &Table,
    cols: &[usize],
    vals: &[Value],
    ctx: &ExecCtx,
    first_only: bool,
) -> Result<Vec<(Vec<u8>, Row)>> {
    let cat = ctx.catalog();
    let mut out = Vec::new();
    let nondet = ri_colls(t, cols, ctx);
    // An index whose leading columns are exactly `cols` (not for keys equal beyond their
    // bytes).
    // (The primary key's index is `pkey_index`, not among `indexes`.)
    let pkey = t.pkey_index.and_then(|p| cat.indexes.get(&p).cloned()).filter(|ix| ix.id != 0);
    for ix in pkey.into_iter().chain(cat.table_indexes(t)).filter(|_| nondet.is_none()) {
        if ix.predicate.is_some() || ix.columns.len() < cols.len() {
            continue;
        }
        let lead: Vec<Option<usize>> = ix.columns[..cols.len()]
            .iter()
            .map(|c| {
                if let IndexKey::Column(i) = c.key {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        if !lead.iter().zip(cols).all(|(a, b)| *a == Some(*b)) {
            continue;
        }
        let mut prefix = index_prefix(t.oid, ix.id);
        for (v, c) in vals.iter().zip(&ix.columns) {
            encode_value(
                &mut prefix,
                &coerce_key(
                    v,
                    &t.columns[match c.key {
                        IndexKey::Column(i) => i,
                        _ => 0,
                    }]
                    .ty,
                    ctx,
                )?,
                c.desc,
            );
        }
        let entries: Vec<(Vec<u8>, bytes::Bytes)> = ctx
            .txn
            .borrow()
            .scan_latest(&prefix, &prefix_end(&prefix))
            .collect::<Result<_>>()?;
        for (k, v) in entries {
            let (dkey, bytes) = if ix.id == 0 {
                (k, v)
            } else {
                let dkey = v.to_vec();
                match ctx.txn.borrow().get_latest(&dkey)? {
                    Some((_, Some(b))) => (dkey, b),
                    _ => continue,
                }
            };
            out.push((dkey, decode_table_row(t, &bytes)?));
            if first_only {
                return Ok(out);
            }
        }
        return Ok(out);
    }
    let prefix = index_prefix(t.oid, 0);
    let entries: Vec<(Vec<u8>, bytes::Bytes)> = ctx
        .txn
        .borrow()
        .scan_latest(&prefix, &prefix_end(&prefix))
        .collect::<Result<_>>()?;
    for (k, v) in entries {
        ctx.check()?;
        let row = decode_table_row(t, &v)?;
        let matches = cols.iter().zip(vals).enumerate().all(|(i, (c, v))| {
            let rv = &row[*c];
            !rv.is_null()
                && coerce_key(v, &t.columns[*c].ty, ctx).is_ok_and(|cv| {
                    match nondet.as_ref().and_then(|n| n[i].as_ref()) {
                        Some(coll) => crate::types::collation::eq_values(coll, rv, &cv),
                        None => rv.cmp_total(&cv) == std::cmp::Ordering::Equal,
                    }
                })
        });
        if matches {
            out.push((k, row));
            if first_only {
                break;
            }
        }
    }
    Ok(out)
}

/// The referential action of foreign key `con` (of `child`) for a row of `t` deleted
/// (new = None) or whose key was updated, as its trigger runs it once the statement is done.
fn fk_parent_action(
    t: &Table,
    child: &Arc<Table>,
    con: &crate::catalog::Constraint,
    action: FkAction,
    old: &[Value],
    new: Option<&[Value]>,
    ctx: &ExecCtx,
) -> Result<()> {
    let ConstraintKind::ForeignKey {
        columns,
        ref_table,
        ref_columns,
        ..
    } = &con.kind
    else {
        return Ok(());
    };
    let cat = ctx.catalog();
    let oldvals: Vec<Value> = ref_columns.iter().map(|c| old[*c].clone()).collect();
    if con.is_period() {
        return fk_parent_period(t, child, con, &oldvals, ctx);
    }
    if action == FkAction::NoAction
        && let Some(parent) = cat.tables.get(ref_table)
        && find_parent_tree(parent, ref_columns, &oldvals, false, ctx)?
    {
        // Another row has the key now (ri_Check_Pk_Match).
        return Ok(());
    }
    let found = scan_tree(
        child,
        columns,
        &oldvals,
        ctx,
        matches!(action, FkAction::NoAction | FkAction::Restrict),
    )?;
    if matches!(action, FkAction::NoAction | FkAction::Restrict) {
        if found.is_empty() {
            return Ok(());
        }
        return Err(parent_violation(
            t,
            child,
            con,
            &oldvals,
            action == FkAction::Restrict,
            ctx,
        ));
    }
    // ON DELETE SET NULL / DEFAULT (columns) sets only those (positions in the key).
    let set_pos: Vec<usize> = if new.is_none()
        && matches!(action, FkAction::SetNull | FkAction::SetDefault)
        && !con.del_set_cols.is_empty()
    {
        (0..columns.len())
            .filter(|p| con.del_set_cols.contains(&columns[*p]))
            .collect()
    } else {
        (0..columns.len()).collect()
    };
    // The query runs (its statement triggers fire) even when no row references the key.
    let child = child.clone();
    ctx.sess.framed(
        ri_query(&cat, t, &child, columns, ref_columns, action, new.is_none(), &set_pos),
        |_| {
            // The referential action runs like its own DELETE / UPDATE on the child, so the
            // child's triggers fire around it: its BEFORE STATEMENT ones once per statement, its
            // AFTER ones queued after the events queued so far (the query runs without firing
            // them: `SPI_execute_snapshot(..., false, ...)`).
            let root_rules = crate::sql::binder::table_rules(&child, ctx)?;
            let ev = if new.is_none() && action == FkAction::Cascade {
                tt::DELETE
            } else {
                tt::UPDATE
            };
            let set_cols: Vec<usize> = if ev == tt::UPDATE {
                set_pos.iter().map(|p| columns[*p]).collect()
            } else {
                Vec::new()
            };
            trigger::statement(&root_rules.triggers, &child, true, ev, &set_cols, ctx)?;
            // Rows of a partitioned table may move to another partition.
            let routing = if ev == tt::UPDATE {
                crate::sql::binder::partition_routing(&child, ctx)?
            } else {
                None
            };
            let mut routed_after = Vec::new();
            let mut root_done = false;
            // (The action's UPDATE is on the referencing table itself: its defaults.)
            let root_cols = columns.clone();
            for (leaf, columns, children) in found {
                let columns = &columns;
                let rules = if leaf.oid == root_rules.table.oid {
                    root_rules.clone()
                } else {
                    crate::sql::binder::table_rules(&leaf, ctx)?
                };
                let set_cols: Vec<usize> = if ev == tt::UPDATE {
                    set_pos.iter().map(|p| columns[*p]).collect()
                } else {
                    Vec::new()
                };
                let mut queue = Vec::new();
                let oper = if ev == tt::UPDATE { Xltw::Update } else { Xltw::Delete };
                for (dkey, crow) in children {
                    if let Locked::Skip = lock_row(&leaf, &dkey, RowMark::Update, Wait::Block, oper, ctx)?
                    {
                        continue;
                    }
                    if ev == tt::DELETE {
                        if trigger::any(&rules.triggers, true, true, tt::DELETE, ctx)
                            && !trigger::before_delete(&rules.triggers, &leaf, &crow, ctx)?
                        {
                            continue;
                        }
                        delete_row(&leaf, &rules.indexes, &dkey, &crow, ctx)?;
                        if trigger::wants_after(&rules.triggers, tt::DELETE, ctx) {
                            queue.push(AfterRow {
                                event: tt::DELETE,
                                old: Some(crow),
                                ..Default::default()
                            });
                        }
                        continue;
                    }
                    let mut nrow = crow.clone();
                    for (p, (c, r)) in columns.iter().zip(ref_columns).enumerate() {
                        if !set_pos.contains(&p) {
                            continue;
                        }
                        nrow[*c] = match (action, new) {
                            (FkAction::Cascade, Some(new)) => new[*r].clone(),
                            (FkAction::SetDefault, _) => match &root_rules.defaults[root_cols[p]] {
                                Some(e) => eval(e, &[], ctx)?,
                                None => Value::Null,
                            },
                            _ => Value::Null,
                        };
                    }
                    if trigger::any(&rules.triggers, true, true, tt::UPDATE, ctx) {
                        match trigger::before_row(
                            &rules.triggers,
                            &leaf,
                            tt::UPDATE,
                            Some(&crow),
                            nrow,
                            &set_cols,
                            ctx,
                        )? {
                            Some(r) => nrow = r,
                            None => continue,
                        }
                    }
                    if let Some(routing) = &routing {
                        for (i, g) in rules.generated.iter().enumerate() {
                            if let Some(e) = g {
                                nrow[i] = eval(e, &nrow, ctx)?;
                            }
                        }
                        if !partition_ok(&rules, &nrow, ctx)? {
                            move_row(&rules, routing, &dkey, &crow, nrow, &mut routed_after, ctx)?;
                            continue;
                        }
                    }
                    let stored = update_row(&rules, &dkey, &crow, nrow, ctx)?;
                    if trigger::wants_after(&rules.triggers, tt::UPDATE, ctx) {
                        queue.push(AfterRow {
                            event: tt::UPDATE,
                            key: stored_key(&leaf, &stored, &dkey),
                            old: Some(crow),
                            new: Some(stored),
                        });
                    }
                }
                let events: Vec<u16> = if leaf.oid == root_rules.table.oid {
                    root_done = true;
                    vec![ev]
                } else {
                    Vec::new()
                };
                trigger::finish(&rules.triggers, &leaf, queue, &events, &set_cols, ctx)?;
            }
            if let Some(routing) = routing {
                finish_routed(&routing, routed_after, ctx)?;
            }
            if !root_done {
                trigger::finish(
                    &root_rules.triggers,
                    &child,
                    Vec::new(),
                    &[ev],
                    &set_cols,
                    ctx,
                )?;
            }
            Ok(())
        },
    )?;
    // SET DEFAULT may have set a row back to the very key that went: ri_set then runs the
    // NO ACTION check (ri_restrict).
    if action == FkAction::SetDefault {
        if let Some(parent) = cat.tables.get(ref_table)
            && find_parent_tree(parent, ref_columns, &oldvals, false, ctx)?
        {
            return Ok(());
        }
        if !scan_tree(&child, columns, &oldvals, ctx, true)?.is_empty() {
            return Err(parent_violation(t, &child, con, &oldvals, false, ctx));
        }
    }
    Ok(())
}

/// The data key a row is stored under after an update (from its primary key, or the row's
/// unchanged key `dkey`).
pub(crate) fn stored_key(t: &Table, row: &[Value], dkey: &[u8]) -> Vec<u8> {
    if t.storage_pkey().is_some() {
        data_key(t, row, None)
    } else {
        dkey.to_vec()
    }
}

/// The query PostgreSQL runs for a referential action on `child` (ri_triggers.c), which
/// errors and notices in it name.
#[allow(clippy::too_many_arguments)]
fn ri_query(
    cat: &crate::catalog::Catalog,
    parent: &Table,
    child: &Table,
    columns: &[usize],
    ref_columns: &[usize],
    action: FkAction,
    delete: bool,
    set_pos: &[usize],
) -> Frame {
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let only = if child.partkey.is_some() { "" } else { "ONLY " };
    let rel = format!(
        "{}.{}",
        quote(cat.schema_name(child.schema)),
        quote(&child.name)
    );
    let n = columns.len();
    // ri_GenerateQual: `$i OPERATOR(pg_catalog.=) col`, the column cast to the key's type
    // when there is no operator between the two.
    let qual = |first: usize| {
        let parts: Vec<String> = columns
            .iter()
            .zip(ref_columns)
            .enumerate()
            .map(|(i, (c, r))| {
                let (pk, fk) = (&parent.columns[*r].ty, &child.columns[*c].ty);
                let same_family = |t: &DataType| match t {
                    DataType::Int2 | DataType::Int4 | DataType::Int8 => 1,
                    DataType::Float4 | DataType::Float8 => 2,
                    t => t.oid(),
                };
                let cast = if same_family(pk) == same_family(fk) {
                    String::new()
                } else {
                    format!("::pg_catalog.{}", pk.typname())
                };
                format!(
                    "${} OPERATOR(pg_catalog.=) {}{cast}",
                    first + i,
                    quote(&child.columns[*c].name)
                )
            })
            .collect();
        format!(" WHERE {}", parts.join(" AND "))
    };
    let text = match (action, delete) {
        (FkAction::Cascade, true) => format!("DELETE FROM {only}{rel}{}", qual(1)),
        (FkAction::Cascade, false) => {
            let sets: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, c)| format!(" {} = ${}", quote(&child.columns[*c].name), i + 1))
                .collect();
            format!("UPDATE {only}{rel} SET{}{}", sets.join(","), qual(n + 1))
        }
        _ => {
            let value = if action == FkAction::SetNull {
                "NULL"
            } else {
                "DEFAULT"
            };
            let sets: Vec<String> = set_pos
                .iter()
                .map(|p| format!(" {} = {value}", quote(&child.columns[columns[*p]].name)))
                .collect();
            format!("UPDATE {only}{rel} SET{}{}", sets.join(","), qual(1))
        }
    };
    Frame::Query {
        kind: "SQL statement",
        text,
        shift: 0,
    }
}

/// The query RI_FKey_check runs for a referencing row (`SELECT 1 FROM [ONLY] pk x WHERE
/// pkcol OPERATOR(pg_catalog.=) $i ... FOR KEY SHARE OF x`), which errors raised while it
/// looks for the referenced row (its lock wait's) name.
fn ri_check_query(
    cat: &crate::catalog::Catalog,
    parent: &Table,
    child: &Table,
    columns: &[usize],
    ref_columns: &[usize],
) -> Frame {
    let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let only = if parent.partkey.is_some() { "" } else { "ONLY " };
    let rel = format!("{}.{}", quote(cat.schema_name(parent.schema)), quote(&parent.name));
    let same_family = |t: &DataType| match t {
        DataType::Int2 | DataType::Int4 | DataType::Int8 => 1,
        DataType::Float4 | DataType::Float8 => 2,
        t => t.oid(),
    };
    let quals: Vec<String> = ref_columns
        .iter()
        .zip(columns)
        .enumerate()
        .map(|(i, (r, c))| {
            let (pk, fk) = (&parent.columns[*r].ty, &child.columns[*c].ty);
            // ri_GenerateQual: the parameter cast to the operator's input type.
            let cast = if same_family(pk) == same_family(fk) {
                String::new()
            } else {
                format!("::pg_catalog.{}", pk.typname())
            };
            format!("{} OPERATOR(pg_catalog.=) ${}{cast}", quote(&parent.columns[*r].name), i + 1)
        })
        .collect();
    Frame::Query {
        kind: "SQL statement",
        text: format!("SELECT 1 FROM {only}{rel} x WHERE {} FOR KEY SHARE OF x", quals.join(" AND ")),
        shift: 0,
    }
}

/// Delete one row (already locked): data and index entries. (Referential actions are AFTER
/// triggers.)
pub fn delete_row(
    t: &Table,
    indexes: &[BoundIndex],
    dkey: &[u8],
    old: &[Value],
    ctx: &ExecCtx,
) -> Result<()> {
    delete_row_with(t, indexes, dkey, old, false, ctx)
}

/// `delete_row`; its index entries deferred to the statement's end when `defer`
/// (`Txn::defer_entry_delete`, `DeleteNode::defers_entries`).
fn delete_row_with(
    t: &Table,
    indexes: &[BoundIndex],
    dkey: &[u8],
    old: &[Value],
    defer: bool,
    ctx: &ExecCtx,
) -> Result<()> {
    if t.foreign.is_some() && crate::ext::postgres_fdw::scan::is_postgres_fdw(&ctx.catalog(), t) {
        return crate::ext::postgres_fdw::modify::delete(t, dkey, ctx).map(|_| ());
    }
    ssi_change(t, indexes, dkey, old, None, ctx)?;
    // heap_delete sets its xmax: writers of its unique keys wait for us now.
    ctx.txn.borrow_mut().row_modified(dkey);
    super::sizing::deleted(t, indexes, dkey, old, ctx)?;
    super::pgstat::count_delete(ctx, t.oid);
    super::toast::delete_row(dkey.to_vec(), Some(old), &mut ctx.txn.borrow_mut())?;
    crate::repl::log::delete(t, old, ctx)?;
    delete_index_entries(t, indexes, old, dkey, defer, ctx)
}

/// A partition with the root's column layout: a routed row needs no conversion.
fn same_layout(l: &PartLeaf) -> bool {
    l.map.len() == l.rules.table.columns.len()
        && l.map.iter().enumerate().all(|(i, m)| *m == Some(i))
}

/// Whether RETURNING reads xmin, cmin, xmax or cmax (the system columns after `base`, the
/// ctid's position).
fn reads_xact_columns(returning: &[crate::sql::expr::Expr], base: usize) -> bool {
    returning
        .iter()
        .any(|e| e.columns().iter().any(|&c| (base + 1..=base + 4).contains(&c)))
}

fn xact_columns_unavailable() -> Error {
    Error::new(
        code::FEATURE_NOT_SUPPORTED,
        "cannot retrieve a system column in this context",
    )
}

/// A generated column's value for a new row. A virtual one is computed when it is read: an
/// error computing it here is the statement's only when a CHECK or NOT NULL constraint reads
/// it (ExecConstraints expands it there; RETURNING reads its expression, `bind_returning`).
fn generated_value(rules: &TableRules, i: usize, e: &crate::sql::expr::Expr, row: &[Value], ctx: &ExecCtx) -> Result<Value> {
    match eval(e, row, ctx) {
        Ok(v) => Ok(v),
        Err(err) => match rules.table.columns.get(i) {
            Some(c) if c.virtual_gen && !c.not_null && !rules.checks.iter().any(|(_, x)| x.columns().contains(&i)) => {
                Ok(Value::Null)
            }
            _ => Err(err),
        },
    }
}

/// CheckForSerializableConflictIn for a new row: the relation, and its entries' index pages.
fn ssi_insert(t: &Table, indexes: &[BoundIndex], dkey: &[u8], row: &[Value], ctx: &ExecCtx) -> Result<()> {
    if t.temporary || !ctx.txn.borrow().serializable_tracked() {
        return Ok(());
    }
    let pages = super::sizing::entry_pages(t, indexes, row, ctx)?;
    ctx.txn.borrow_mut().ssi_write(t.oid, dkey, None, &pages)
}

/// CheckForSerializableConflictIn for a row about to be updated or deleted: its tuple,
/// page and relation (and, for an update, the index pages its new entries go to).
fn ssi_change(
    t: &Table,
    indexes: &[BoundIndex],
    dkey: &[u8],
    old: &[Value],
    new: Option<&[Value]>,
    ctx: &ExecCtx,
) -> Result<()> {
    if t.temporary || !ctx.txn.borrow().serializable_tracked() {
        return Ok(());
    }
    let bytes = ctx.txn.borrow().get(dkey)?;
    let tid = bytes.as_deref().and_then(super::scan::stored_tid);
    // (Without tuple ids, the version it replaces: the latest committed one.)
    let version = match tid {
        Some(_) => None,
        None => ctx.txn.borrow().get_latest(dkey)?.map(|(ts, _)| ts),
    };
    // A HOT update (no indexed column changed) adds no index entries.
    let pages = match new {
        Some(r) if !hot_update(indexes, old, r, ctx)? => {
            super::sizing::entry_pages(t, indexes, r, ctx)?
        }
        _ => Vec::new(),
    };
    ctx.txn.borrow_mut().ssi_write_version(t.oid, dkey, tid, &pages, version)
}

/// Whether an update keeps every index's values (heap_update's HOT test).
fn hot_update(indexes: &[BoundIndex], old: &[Value], new: &[Value], ctx: &ExecCtx) -> Result<bool> {
    for bix in indexes {
        if bix.index.id == 0 {
            continue;
        }
        let (a, b) = (index_values(bix, old, ctx)?, index_values(bix, new, ctx)?);
        let same = match (&a, &b) {
            (Some(a), Some(b)) => super::agg::cmp_rows(a, b) == std::cmp::Ordering::Equal,
            (None, None) => true,
            _ => false,
        };
        if !same {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether two values are the same datum (datumIsEqual: their stored images, so `1.0` and
/// `1.00` differ).
fn same_image(a: Option<&Value>, b: Option<&Value>) -> bool {
    let img = |v: Option<&Value>| {
        let mut out = Vec::new();
        crate::storage::row::encode_value(&mut out, v.unwrap_or(&Value::Null));
        out
    };
    img(a) == img(b)
}

/// Compute generated columns, validate, and write a new version of a row (already locked).
/// Returns the row as stored.
pub fn update_row(
    rules: &TableRules,
    dkey: &[u8],
    old: &[Value],
    new: Row,
    ctx: &ExecCtx,
) -> Result<Row> {
    update_row_rls(rules, &rules.rls_checks, dkey, old, new, ctx)
}

/// update_row, checking these row-level security options (ON CONFLICT DO UPDATE's).
pub fn update_row_rls(
    rules: &TableRules,
    rls: &[crate::sql::plan::RlsCheck],
    dkey: &[u8],
    old: &[Value],
    mut new: Row,
    ctx: &ExecCtx,
) -> Result<Row> {
    let t = &rules.table;
    for (i, g) in rules.generated.iter().enumerate() {
        if let Some(e) = g {
            new[i] = generated_value(rules, i, e, &new, ctx)?;
        }
    }
    // (Not a foreign partition's row: the remote server has the say, ExecForeignUpdate.)
    if t.foreign.is_none() {
        check_partition(rules, &new, ctx)?;
    }
    check_row_rls(rules, rls, &new, ctx)?;
    if t.foreign.is_some() && crate::ext::postgres_fdw::scan::is_postgres_fdw(&ctx.catalog(), t) {
        return crate::ext::postgres_fdw::modify::update(t, dkey, old, new, ctx);
    }
    ssi_change(t, &rules.indexes, dkey, old, Some(&new), ctx)?;
    // heap_update sets the old version's xmax: writers of its unique keys wait for us now.
    ctx.txn.borrow_mut().row_modified(dkey);
    let new_key = if t.storage_pkey().is_some() {
        data_key(t, &new, None)
    } else {
        dkey.to_vec()
    };
    // An index none of whose columns changed keeps its entry, its expressions not run again
    // (heap_update's HOT test is by column: index expressions are immutable).
    let changed: Vec<bool> = rules
        .indexes
        .iter()
        .map(|bix| {
            if new_key != dkey {
                return true;
            }
            let mut cols = HashSet::new();
            super::sizing::index_columns(bix, &mut cols);
            cols.is_empty() || cols.iter().any(|&c| !same_image(old.get(c), new.get(c)))
        })
        .collect();
    let mut new_vals: Vec<Option<Option<Vec<Value>>>> = vec![None; rules.indexes.len()];
    let mut old_vals: Vec<Option<Option<Vec<Value>>>> = vec![None; rules.indexes.len()];
    // Unique and exclusion indexes whose key changed.
    for (i, bix) in rules.indexes.iter().enumerate() {
        if !checks_rows(bix) || !changed[i] {
            continue;
        }
        let nv = index_values(bix, &new, ctx)?;
        new_vals[i] = Some(nv.clone());
        let ov = index_values(bix, old, ctx)?;
        old_vals[i] = Some(ov.clone());
        let Some(nv) = nv else {
            continue;
        };
        check_without_overlaps(t, bix, &nv)?;
        if ov
            .as_ref()
            .is_some_and(|o| super::agg::cmp_rows(o, &nv) == std::cmp::Ordering::Equal)
        {
            continue;
        }
        if defer_recheck(t, bix, &nv, &new_key, Some(dkey), ctx)? {
            continue;
        }
        let entering = Entering { token: None, row: None, key: Some(new_key.as_slice()) };
        if let Some((_, other)) =
            index_conflict(t, bix, &nv, Some(dkey), Some(&entering), Dirty::Wait, false, ctx)?
        {
            ssi_change(t, &rules.indexes, dkey, old, Some(&new), ctx)?;
            // The new version went into the heap before its index entries found the
            // clash: it stays, dead, where it was put.
            if new_key == dkey {
                let _ = super::sizing::updated(t, &rules.indexes, dkey, old, &new_key, &new, ctx);
            }
            return Err(index_violation(t, bix, &nv, other.as_deref(), ctx));
        }
    }
    let upd = super::sizing::updated(t, &rules.indexes, dkey, old, &new_key, &new, ctx)?;
    let tid = upd.map(|u| u.0);
    super::pgstat::count_update(ctx, t.oid, upd.is_some_and(|u| u.1), false);
    if new_key != dkey {
        super::toast::delete_row(dkey.to_vec(), Some(old), &mut ctx.txn.borrow_mut())?;
    }
    for (i, bix) in rules.indexes.iter().enumerate() {
        if bix.index.id == 0 || !changed[i] {
            continue;
        }
        let ov = match old_vals[i].take() {
            Some(v) => v,
            None => index_values(bix, old, ctx)?,
        };
        let nv = match new_vals[i].take() {
            Some(v) => v,
            None => index_values(bix, &new, ctx)?,
        };
        let same = new_key == dkey
            && matches!((&ov, &nv), (Some(a), Some(b)) if super::agg::cmp_rows(a, b) == std::cmp::Ordering::Equal);
        if same {
            continue;
        }
        // (A native method's index: the new version's entry; the old one's stays.)
        if am_insert(t, bix, nv.as_deref(), &new_key, new_key == dkey, ctx)? {
            continue;
        }
        if let Some(ov) = ov {
            ctx.txn
                .borrow_mut()
                .delete(index_entry_key(t, &bix.index, &ov, dkey, &bix.colls))?;
        }
        if let Some(nv) = nv {
            ctx.txn.borrow_mut().put(
                index_entry_key(t, &bix.index, &nv, &new_key, &bix.colls),
                new_key.clone(),
            )?;
        }
    }
    let xid = ctx.txn.borrow_mut().cur_xid();
    {
        // (A row kept under its key keeps its unchanged out-of-line values.)
        let txn = &mut ctx.txn.borrow_mut();
        if new_key != dkey {
            super::toast::put_row(t, new_key, &new, xid, tid, txn)?;
        } else {
            super::toast::replace_row(t, new_key, Some(old), &new, xid, tid, txn)?;
        }
    }
    crate::repl::log::update(t, old, &new, ctx)?;
    check_view_options(rules, &new, ctx)?;
    Ok(new)
}

/// What became of a row offered for insertion.
pub enum Inserted {
    /// A BEFORE (or INSTEAD OF) trigger skipped it.
    Skipped,
    /// The row as stored (or as an INSTEAD OF trigger returned it, with an empty key).
    Stored(Row, Vec<u8>),
    /// It collides with an existing row on an ON CONFLICT arbiter index.
    Conflict(Row, Conflict),
}

/// A new row from values for the `target` columns, the others defaulted.
pub fn new_row(rules: &TableRules, target: &[usize], src: Row, ctx: &ExecCtx) -> Result<Row> {
    let t = &rules.table;
    // (Every column given, in order: the row as it is.)
    if src.len() == t.columns.len() && target.len() == src.len() && target.iter().enumerate().all(|(i, c)| i == *c) {
        return Ok(src);
    }
    let mut row: Row = vec![Value::Null; t.columns.len()];
    let mut provided = vec![false; t.columns.len()];
    for (v, c) in src.into_iter().zip(target) {
        row[*c] = v;
        provided[*c] = true;
    }
    for (i, c) in t.columns.iter().enumerate() {
        if c.dropped || provided[i] || c.generated.is_some() {
            continue;
        }
        if let Some(d) = &rules.defaults[i] {
            row[i] = eval(d, &[], ctx)?;
        }
    }
    Ok(row)
}

/// A row ready for its unique checks and its write (ExecInsert at its `vlock:` label):
/// defaults, BEFORE ROW triggers, generated columns and checks done, its key chosen. An ON
/// CONFLICT DO UPDATE whose conflicting row changed while it waited starts over from here.
#[derive(Clone)]
pub struct Prepared {
    pub row: Row,
    pub dkey: Vec<u8>,
}

/// Defaults, BEFORE ROW (or INSTEAD OF) triggers, generated columns and checks of a row
/// offered for insertion: Err(what became of it) when a trigger took it over.
fn prepare_insert(
    rules: &TableRules,
    target: &[usize],
    src: Row,
    rls: &[crate::sql::plan::RlsCheck],
    ctx: &ExecCtx,
) -> Result<std::result::Result<Prepared, Inserted>> {
    let t = &rules.table;
    let mut row = new_row(rules, target, src, ctx)?;
    if rules.view {
        return Ok(Err(
            match trigger::instead(&rules.triggers, t, tt::INSERT, None, Some(row), ctx)? {
                Some(r) => {
                    check_view_options(rules, &r, ctx)?;
                    Inserted::Stored(r, Vec::new())
                }
                None => Inserted::Skipped,
            },
        ));
    }
    if trigger::any(&rules.triggers, true, true, tt::INSERT, ctx) {
        let fits = |r: &Row| partition_ok(rules, r, ctx);
        let fits: Option<&dyn Fn(&Row) -> Result<bool>> = if rules.part_check.is_some() {
            Some(&fits)
        } else {
            None
        };
        match trigger::before_row_in(&rules.triggers, t, tt::INSERT, None, row, &[], fits, ctx)? {
            Some(r) => row = r,
            None => return Ok(Err(Inserted::Skipped)),
        }
    }
    for (i, g) in rules.generated.iter().enumerate() {
        if let Some(e) = g {
            row[i] = generated_value(rules, i, e, &row, ctx)?;
        }
    }
    check_row_rls(rules, rls, &row, ctx)?;
    // (ExecInsert checks a foreign partition's row by its server only.)
    if t.foreign.is_none() {
        check_partition(rules, &row, ctx)?;
    }
    let rowid = if t.has_rowid() {
        Some(
            ctx.sess
                .db
                .nextval_by_one(t.oid, || crate::session::ddl::rowid_sequence(t))?,
        )
    } else {
        None
    };
    let dkey = data_key(t, &row, rowid);
    Ok(Ok(Prepared { row, dkey }))
}

/// Insert one row given values for the `target` columns: defaults, BEFORE ROW triggers,
/// generated columns, checks, unique keys (a collision is an error unless `on_conflict`
/// covers it), foreign keys, the write and its index entries. AFTER ROW events are queued
/// on `after`.
pub fn insert_new(
    rules: &TableRules,
    target: &[usize],
    src: Row,
    rls: &[crate::sql::plan::RlsCheck],
    on_conflict: Option<&crate::sql::plan::OnConflict>,
    after: &mut Vec<AfterRow>,
    ctx: &ExecCtx,
) -> Result<Inserted> {
    match prepare_insert(rules, target, src, rls, ctx)? {
        Ok(p) => insert_prepared(rules, p, on_conflict, after, ctx),
        Err(done) => Ok(done),
    }
}

/// The unique checks and the write of a prepared row. With ON CONFLICT, Postgres's
/// speculative insertion: the arbiters are checked first (waiting for transactions writing
/// the key), then the row goes in under a speculative token, its arbiter entries checked
/// again without waiting; a conflict found then kills the row and starts over.
pub fn insert_prepared(
    rules: &TableRules,
    p: Prepared,
    on_conflict: Option<&crate::sql::plan::OnConflict>,
    after: &mut Vec<AfterRow>,
    ctx: &ExecCtx,
) -> Result<Inserted> {
    let t = &rules.table;
    // A postgres_fdw foreign table: the row goes to the remote server.
    if t.foreign.is_some() && crate::ext::postgres_fdw::scan::is_postgres_fdw(&ctx.catalog(), t) {
        crate::ext::postgres_fdw::modify::check_routed_capture()?;
        // (A row routed here from a partitioned table: what this leaf's AFTER ROW triggers
        // and the views' check options read comes back.)
        use crate::ext::postgres_fdw::modify;
        modify::check_update_target(t)?;
        let _back = (!modify::has_back()).then(|| {
            let exprs: Vec<&Expr> = rules.view_checks.iter().map(|(_, e)| e).collect();
            let tg = modify::tgtypes(&rules.triggers);
            modify::with_back(modify::with_routed(t, modify::Back::of(t, &tg, tt::INSERT, &exprs, None)))
        });
        // (ON CONFLICT DO NOTHING goes to the server; other ON CONFLICT specifications
        // found no arbiter index while planning.)
        let do_nothing = on_conflict.is_some_and(|c| matches!(c.action, crate::sql::plan::ConflictAction::Nothing));
        let Some(row) = modify::insert(t, p.row, do_nothing, ctx)? else {
            return Ok(Inserted::Skipped);
        };
        if trigger::wants_after(&rules.triggers, tt::INSERT, ctx) {
            after.push(AfterRow {
                event: tt::INSERT,
                old: None,
                new: Some(row.clone()),
                key: Vec::new(),
            });
        }
        // (The views' check options, on the row as stored there: ExecWithCheckOptions.)
        check_view_options(rules, &row, ctx)?;
        return Ok(Inserted::Stored(row, Vec::new()));
    }
    // Others' index expressions over our in-progress row (check_exclusion_or_unique_
    // constraint's FormIndexDatum) need it.
    let shared = rules
        .indexes
        .iter()
        .any(|b| checks_rows(b) && index_has_exprs(b))
        .then(|| Arc::new(p.row.clone()));
    let Some(oc) = on_conflict else {
        // SSI: heap_insert's check at the relation, then each btree entry's at its leaf
        // page (before a unique check fails: _bt_check_unique).
        ssi_insert(t, &rules.indexes, &p.dkey, &p.row, ctx)?;
        let vals = index_entries_checked(rules, &p, None, shared.as_ref(), ctx)?;
        let (row, dkey) = store_inserted(rules, p, &vals, after, ctx)?;
        return Ok(Inserted::Stored(row, dkey));
    };
    let arbiter_of =
        |bix: &BoundIndex| oc.arbiters.is_empty() || oc.arbiters.contains(&bix.index.oid);
    loop {
        // ExecCheckIndexConstraints: the arbiters, waiting for writers of the key.
        for (n, bix) in rules.indexes.iter().enumerate() {
            if !checks_rows(bix) || !arbiter_of(bix) {
                continue;
            }
            if let Some(c) = bix.constraint.as_ref().filter(|c| c.deferral.0) {
                return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, "ON CONFLICT does not support deferrable unique constraints/exclusion constraints as arbiters")
                    .table(ctx.catalog().schema_name(t.schema), &t.name)
                    .constraint(&c.name));
            }
            let Some(vals) = index_values(bix, &p.row, ctx)? else {
                continue;
            };
            check_without_overlaps(t, bix, &vals)?;
            if let Some((existing, _)) =
                index_conflict(t, bix, &vals, None, None, Dirty::Wait, true, ctx)?
            {
                return Ok(Inserted::Conflict(
                    p.row.clone(),
                    Conflict {
                        index: n,
                        existing,
                        values: vals,
                        prepared: p,
                    },
                ));
            }
        }
        ssi_insert(t, &rules.indexes, &p.dkey, &p.row, ctx)?;
        let token = ctx.txn.borrow_mut().begin_speculative()?;
        let r = index_entries_checked(rules, &p, Some((token, &arbiter_of)), shared.as_ref(), ctx);
        let r = match r {
            Ok(vals) => match vals.conflict {
                // specConflict: the tuple (and its entries) went in and is killed; start
                // over at the pre-check.
                true => {
                    let _ = super::sizing::inserted_with(
                        t,
                        &rules.indexes,
                        &p.dkey,
                        &p.row,
                        Some(&vals.vals),
                        ctx,
                    );
                    super::pgstat::count_insert(ctx, t.oid, 1);
                    super::pgstat::count_delete(ctx, t.oid);
                    for k in &vals.taken {
                        ctx.txn.borrow_mut().unlock(k, LockMode::Exclusive);
                    }
                    for k in &vals.entered {
                        if !vals.taken.contains(k) {
                            ctx.txn.borrow_mut().hold_unique_key(k, None, shared.clone(), Some(&p.dkey));
                        }
                    }
                    None
                }
                false => {
                    // The speculative insertion is complete: its entries stand for the
                    // transaction from now on.
                    for k in &vals.entered {
                        ctx.txn.borrow_mut().hold_unique_key(k, None, shared.clone(), Some(&p.dkey));
                    }
                    for &con in &vals.excl {
                        ctx.txn.borrow().settle_exclusion_entry(con, &p.dkey);
                    }
                    Some(store_inserted(rules, p.clone(), &vals, after, ctx))
                }
            },
            Err(e) => Some(Err(e)),
        };
        ctx.txn.borrow_mut().end_speculative(token);
        match r {
            None => continue,
            Some(r) => {
                let (row, dkey) = r?;
                return Ok(Inserted::Stored(row, dkey));
            }
        }
    }
}

/// A row's index values, one per index, with what its unique checks did.
struct CheckedEntries {
    vals: Vec<Option<Vec<Value>>>,
    /// A speculative insertion's arbiter found a (possible) conflict.
    conflict: bool,
    /// Unique key locks this attempt took, and every one its arbiter entries hold.
    taken: Vec<Vec<u8>>,
    entered: Vec<Vec<u8>>,
    /// The exclusion constraints a speculative insertion's arbiter entries went into.
    excl: Vec<u32>,
}

/// ExecInsertIndexTuples' part before the writes: each index's values (FormIndexDatum, in
/// index order, once) and the unique and exclusion checks, erroring on a violation (the
/// dead tuple left with the entries made before it). `spec`: a speculative insertion (its
/// token, and which indexes are arbiters), whose arbiters' checks don't wait and report a
/// conflict instead.
#[allow(clippy::type_complexity)]
fn index_entries_checked(
    rules: &TableRules,
    p: &Prepared,
    spec: Option<(u32, &dyn Fn(&BoundIndex) -> bool)>,
    shared: Option<&Arc<Vec<Value>>>,
    ctx: &ExecCtx,
) -> Result<CheckedEntries> {
    let t = &rules.table;
    let mut out = CheckedEntries {
        vals: Vec::with_capacity(rules.indexes.len()),
        conflict: false,
        taken: Vec::new(),
        entered: Vec::new(),
        excl: Vec::new(),
    };
    for bix in &rules.indexes {
        let vals = index_values(bix, &p.row, ctx)?;
        if let Some(v) = &vals
            && checks_rows(bix)
        {
            match spec {
                Some((token, arbiter_of)) if arbiter_of(bix) => {
                    if bix.index.unique {
                        let nondet = unkeyed_nondet(t, bix, ctx);
                        let lk = unique_lock_key(t, bix, v, !nondet.is_empty());
                        let had = ctx.txn.borrow().holds_lock_mode(&lk, LockMode::Exclusive);
                        let entering = Entering { key: Some(p.dkey.as_slice()), token: Some(token), row: shared };
                        let found = find_conflict(t, bix, v, None, Some(&entering), Dirty::Partial, false, ctx)?;
                        if ctx.txn.borrow().holds_lock_mode(&lk, LockMode::Exclusive) {
                            if !had {
                                out.taken.push(lk.clone());
                            }
                            out.entered.push(lk);
                        }
                        out.conflict |= found.is_some();
                    } else {
                        // The speculative row's exclusion entry goes in first, as any
                        // writer's; given up with the row when it conflicts (`taken`).
                        let con = bix.constraint.as_ref().map_or(0, |c| c.oid);
                        let lk = crate::txn::locks::excl_entry_key(con, &p.dkey);
                        let had = ctx.txn.borrow().holds_lock_mode(&lk, LockMode::Exclusive);
                        let entering = Some((p.dkey.as_slice(), Some(token)));
                        let found = exclusion_conflict(t, bix, v, None, entering, ExclWait::Livelock, ctx)?;
                        if !had && ctx.txn.borrow().holds_lock_mode(&lk, LockMode::Exclusive) {
                            out.taken.push(lk);
                        }
                        out.excl.push(con);
                        out.conflict |= found.is_some();
                    }
                }
                _ => {
                    check_without_overlaps(t, bix, v)?;
                    if !defer_recheck(t, bix, v, &p.dkey, None, ctx)? {
                        let entering = Entering { key: Some(p.dkey.as_slice()), token: None, row: shared };
                        if let Some((_, other)) =
                            index_conflict(t, bix, v, None, Some(&entering), Dirty::Wait, false, ctx)?
                        {
                            // _bt_check_unique checks for SSI conflicts before it reports
                            // the clash.
                            ssi_insert(t, &rules.indexes, &p.dkey, &p.row, ctx)?;
                            // The heap tuple went in before its index entries found the
                            // clash (ExecInsertIndexTuples): it stays, dead, where it was
                            // put, with the entries made so far.
                            if t.has_rowid() {
                                let _ = super::sizing::inserted_with(
                                    t,
                                    &rules.indexes,
                                    &p.dkey,
                                    &p.row,
                                    Some(&out.vals),
                                    ctx,
                                );
                            }
                            return Err(index_violation(t, bix, v, other.as_deref(), ctx));
                        }
                    }
                }
            }
        }
        out.vals.push(vals);
    }
    Ok(out)
}

/// Write an inserted row and its index entries; its AFTER ROW event queued.
fn store_inserted(
    rules: &TableRules,
    p: Prepared,
    vals: &CheckedEntries,
    after: &mut Vec<AfterRow>,
    ctx: &ExecCtx,
) -> Result<(Row, Vec<u8>)> {
    let t = &rules.table;
    let Prepared { row, dkey } = p;
    let tid = super::sizing::inserted_with(t, &rules.indexes, &dkey, &row, Some(&vals.vals), ctx)?;
    super::pgstat::count_insert(ctx, t.oid, 1);
    super::pgstat::io_insert(ctx, t, 1, "normal");
    let xid = ctx.txn.borrow_mut().cur_xid();
    // (A row of a table keyed by row ids has a key never used before.)
    let fresh = t.has_rowid();
    if fresh {
        super::toast::put_new_row(t, dkey.clone(), &row, xid, tid, &mut ctx.txn.borrow_mut())?;
    } else {
        super::toast::put_row(t, dkey.clone(), &row, xid, tid, &mut ctx.txn.borrow_mut())?;
    }
    crate::repl::log::insert(t, &row, ctx)?;
    put_index_entries_with(t, &rules.indexes, &vals.vals, &dkey, fresh, ctx)?;
    if trigger::wants_after(&rules.triggers, tt::INSERT, ctx) {
        after.push(AfterRow {
            event: tt::INSERT,
            old: None,
            new: Some(row.clone()),
            key: dkey.clone(),
        });
    }
    check_view_options(rules, &row, ctx)?;
    Ok((row, dkey))
}

/// Whether a plan returns each row of the tables it scans at most once: scans of distinct
/// tables under filters, projections, sorts and appends (no join, set operation or
/// function). An UPDATE or DELETE over one needn't remember the rows it has done.
fn rows_once(p: &crate::sql::plan::Plan) -> bool {
    fn scans(p: &crate::sql::plan::Plan, tables: &mut Vec<u32>) -> bool {
        use crate::sql::plan::Plan as P;
        match p {
            P::Scan(s) => {
                let fresh = !tables.contains(&s.table.oid);
                tables.push(s.table.oid);
                fresh
            }
            P::Filter { .. }
            | P::Project { .. }
            | P::Barrier { .. }
            | P::Context { .. }
            | P::LockRows { .. }
            | P::Limit { .. }
            | P::Sort { .. }
            | P::Append { .. } => crate::sql::explain::plan_inputs(p).iter().all(|c| scans(c, tables)),
            _ => false,
        }
    }
    scans(p, &mut Vec::new())
}

// ── INSERT ──

/// Whether an INSERT's rows past its first few thousand can wait for its end to go in the
/// heap model (`sizing::CopyLoad::deferred_inserts`): nothing the statement runs before it
/// ends can read the table's model or the rows' TIDs. That is COPY's multi-insert rule (no
/// BEFORE ROW trigger, no volatile default) plus no RETURNING or ON CONFLICT, no RLS or
/// volatile check, and a source that runs nothing volatile and doesn't read the table
/// (whose scan could prune the pages the rows go to).
fn model_deferrable(plan: &InsertPlan, ctx: &ExecCtx) -> bool {
    let rules = &plan.rules;
    let t = &rules.table;
    if rules.view
        || t.partkey.is_some()
        || t.foreign.is_some()
        || plan.route.is_some()
        || plan.on_conflict.is_some()
        || !plan.returning.is_empty()
        || !rules.rls_checks.is_empty()
        || trigger::any(&rules.triggers, true, true, tt::INSERT, ctx)
        || rules.checks.iter().any(|(_, e)| e.is_volatile())
    {
        return false;
    }
    let volatile_default = rules.defaults.iter().flatten().any(|d| {
        let mut v = false;
        d.walk(&mut |e| match e {
            crate::sql::expr::Expr::Call(f, ..) if f.volatility == 'v' && f.name != "nextval" => v = true,
            crate::sql::expr::Expr::Subquery(_) => v = true,
            _ => {}
        });
        v
    });
    !volatile_default
        && !crate::sql::paths::plan_volatile_deep(&plan.source)
        && reads_only_others(&plan.source, t)
}

/// Whether a plan (its sub-selects and CTEs too) is made only of plain reading nodes, none
/// scanning table `t` or one it inherits from.
fn reads_only_others(p: &crate::sql::plan::Plan, t: &crate::catalog::Table) -> bool {
    use crate::sql::plan::Plan as P;
    let ok = match p {
        P::Scan(s) => s.table.oid != t.oid && !t.inherits.contains(&s.table.oid),
        P::With { ctes, .. } => ctes.iter().all(|(_, c)| reads_only_others(c, t)),
        P::One
        | P::Empty { .. }
        | P::Values { .. }
        | P::FunctionScan { .. }
        | P::Barrier { .. }
        | P::Context { .. }
        | P::Filter { .. }
        | P::Project { .. }
        | P::ProjectSet { .. }
        | P::Agg { .. }
        | P::Window { .. }
        | P::Sort { .. }
        | P::Limit { .. }
        | P::Distinct { .. }
        | P::SetOp { .. }
        | P::Append { .. }
        | P::Join { .. }
        | P::CteScan { .. }
        | P::Recursive { .. }
        | P::WorkScan { .. } => true,
        _ => false,
    };
    if !ok {
        return false;
    }
    let mut es = Vec::new();
    crate::sql::explain::node_exprs(p, &mut es);
    let mut fine = true;
    for e in es {
        e.walk(&mut |x| {
            if let crate::sql::expr::Expr::Subquery(s) = x {
                fine &= reads_only_others(&s.plan, t);
            }
        });
    }
    fine && crate::sql::explain::plan_inputs(p).iter().all(|c| reads_only_others(c, t))
}

pub struct InsertNode {
    plan: super::planref::PlanRef<InsertPlan>,
    source: BoxNode,
    done: bool,
    /// Rows changed by ON CONFLICT DO UPDATE in this command.
    touched: HashSet<Vec<u8>>,
    /// AFTER ROW trigger events, fired when the statement ends.
    after: Vec<AfterRow>,
    /// Those of rows routed to partitions, with their leaf.
    routed_after: Vec<(usize, AfterRow)>,
    /// The table each RETURNING row went to (tableoid).
    last_table: u32,
    /// COPY: the line number and text of the row being inserted.
    copy_line: (usize, String),
    /// COPY FROM's bulk insert into the table's modelled heap.
    copy_load: Option<super::sizing::CopyLoad>,
    /// The last row went to a partition laid out as the root: RETURNING reads it from the
    /// routing's own (virtual) slot, which has no transaction system columns.
    virtual_slot: bool,
    /// The statement's AFTER queue length when this command started (`hoist_row_events`).
    queue_mark: Option<usize>,
    /// ON CONFLICT DO UPDATE starting a row over: (its leaf, when routed; the row as
    /// prepared).
    retry: Option<(Option<usize>, Prepared)>,
}

impl InsertNode {
    pub fn new(plan: super::planref::PlanRef<InsertPlan>, ctx: &ExecCtx) -> Result<Self> {
        // (ExecutorStart: es_output_cid, which the next command moves past.)
        ctx.txn.borrow_mut().output_cid();
        let rules = &plan.rules;
        if let Some(e) = ctx.catalog().fdw_modify_error(&rules.table, "insert into") {
            return Err(e);
        }
        if !rules.view
            && plan.on_conflict.as_ref().is_some_and(|oc| {
                matches!(oc.action, crate::sql::plan::ConflictAction::Update { .. })
            })
        {
            check_replica_identity(&rules.table, crate::catalog::pubsub::RiCmd::Update, ctx)?;
        }
        for e in Self::events(&plan) {
            trigger::statement(
                &rules.triggers,
                &rules.table,
                true,
                e,
                &Self::conflict_cols(&plan),
                ctx,
            )?;
        }
        let source = super::nodes::open(&plan.source, ctx)?;
        let last_table = plan.rules.table.oid;
        let copy_load = match &plan.copy {
            Some(c) if !rules.view && rules.table.partkey.is_none() => {
                Some(super::sizing::CopyLoad::start(&rules.table, c.multi, c.freeze, ctx))
            }
            None if model_deferrable(&plan, ctx) => {
                Some(super::sizing::CopyLoad::deferred_inserts(&rules.table, ctx))
            }
            _ => None,
        };
        Ok(InsertNode {
            plan,
            source,
            done: false,
            touched: HashSet::new(),
            after: Vec::new(),
            routed_after: Vec::new(),
            last_table,
            copy_line: (0, String::new()),
            copy_load,
            virtual_slot: false,
            queue_mark: None,
            retry: None,
        })
    }

    /// Statement-level events: INSERT, plus UPDATE for ON CONFLICT DO UPDATE.
    fn events(plan: &InsertPlan) -> Vec<u16> {
        let mut ev = vec![tt::INSERT];
        if matches!(
            plan.on_conflict.as_ref().map(|o| &o.action),
            Some(ConflictAction::Update { .. })
        ) {
            ev.push(tt::UPDATE);
        }
        ev
    }

    fn conflict_cols(plan: &InsertPlan) -> Vec<usize> {
        match plan.on_conflict.as_ref().map(|o| &o.action) {
            Some(ConflictAction::Update { sets, .. }) => sets.iter().map(|(c, _)| *c).collect(),
            _ => Vec::new(),
        }
    }

    /// Insert one source row. Returns the final row (for RETURNING) with its key and the
    /// existing row ON CONFLICT updated, or None if skipped.
    fn insert(&mut self, src: Row, ctx: &ExecCtx) -> Result<Option<(Row, Vec<u8>, Option<Row>)>> {
        // (Only ON CONFLICT DO UPDATE retries, needing the row again.)
        if self.plan.on_conflict.is_none() {
            return self.insert_once(src, None, ctx);
        }
        // ON CONFLICT DO UPDATE found its row changed while it waited for it: start over at
        // the unique checks (its BEFORE ROW triggers don't fire again).
        let mut resume = None;
        loop {
            match self.insert_once(src.clone(), resume.take(), ctx) {
                Err(e) if e.code == RETRY_INSERT => resume = self.retry.take(),
                r => return r,
            }
        }
    }

    fn insert_once(
        &mut self,
        src: Row,
        resume: Option<(Option<usize>, Prepared)>,
        ctx: &ExecCtx,
    ) -> Result<Option<(Row, Vec<u8>, Option<Row>)>> {
        let rules = &self.plan.rules;
        if let Some(routing) = self.plan.route.clone() {
            // (A foreign partition can't give its rows to the transition tables.)
            let _capture = crate::ext::postgres_fdw::modify::routed_capture(trigger::has_transition(&self.plan.rules.triggers, tt::INSERT));
            // (What foreign partitions return for the RETURNING list and the check options.)
            let _routed_back = if self.plan.returning.is_empty() && rules.view_checks.is_empty() {
                None
            } else {
                let exprs: Vec<&Expr> = self.plan.returning.iter().chain(rules.view_checks.iter().map(|(_, e)| e)).collect();
                let b = crate::ext::postgres_fdw::modify::Back::of(&rules.table, &[], tt::INSERT, &exprs, None);
                crate::ext::postgres_fdw::modify::routed_back(&rules.table, b)
            };
            let r = match resume {
                Some((Some(leaf), p)) => resume_routed(
                    &routing,
                    leaf,
                    p,
                    self.plan.on_conflict.as_ref(),
                    &mut self.routed_after,
                    ctx,
                )?,
                _ => {
                    let row = new_row(rules, &self.plan.target, src, ctx)?;
                    insert_routed_oc(
                        &routing,
                        &rules.rls_checks,
                        row,
                        self.plan.on_conflict.as_ref(),
                        &mut self.routed_after,
                        ctx,
                    )?
                }
            };
            return match r {
                Routed::Skipped => Ok(None),
                Routed::Stored(leaf, row, key) => {
                    self.last_table = routing.leaves[leaf].rules.table.oid;
                    self.virtual_slot = same_layout(&routing.leaves[leaf]);
                    // The views written through check the row as the root's.
                    check_view_options(&self.plan.rules, &row, ctx)?;
                    if trigger::has_transition(&self.plan.rules.triggers, tt::INSERT) {
                        self.after.push(AfterRow {
                            event: tt::INSERT | trigger::CAPTURE,
                            new: Some(row.clone()),
                            ..Default::default()
                        });
                    }
                    Ok(Some((row, key, None)))
                }
                Routed::Conflict(leaf, row, c) => {
                    self.last_table = routing.leaves[leaf].rules.table.oid;
                    Ok(self
                        .on_conflict(c, row, Some((&routing, leaf)), ctx)?
                        .map(|(r, k, o)| (r, k, Some(o))))
                }
            };
        }
        // (A postgres_fdw table's rows come back as the remote server stored them, for
        // RETURNING, the views' check options and AFTER ROW triggers.)
        let _back = rules.table.foreign.is_some().then(|| {
            let n = rules.table.columns.len();
            crate::ext::postgres_fdw::modify::with_back(crate::ext::postgres_fdw::modify::Back::of(
                &rules.table,
                &crate::ext::postgres_fdw::modify::tgtypes(&rules.triggers),
                tt::INSERT,
                &self.plan.returning.iter().chain(rules.view_checks.iter().map(|(_, e)| e)).collect::<Vec<_>>(),
                self.plan.sys.then_some(n),
            ))
        });
        let r = match resume {
            Some((None, p)) => insert_prepared(
                rules,
                p,
                self.plan.on_conflict.as_ref(),
                &mut self.after,
                ctx,
            )?,
            _ => insert_new(
                rules,
                &self.plan.target,
                src,
                &rules.rls_checks,
                self.plan.on_conflict.as_ref(),
                &mut self.after,
                ctx,
            )?,
        };
        match r {
            Inserted::Skipped => Ok(None),
            Inserted::Stored(row, key) => Ok(Some((row, key, None))),
            Inserted::Conflict(row, c) => Ok(self
                .on_conflict(c, row, None, ctx)?
                .map(|(r, k, o)| (r, k, Some(o)))),
        }
    }

    /// Insert a COPY FROM row under its line's context. Buffered rows reach their indexes
    /// after the line is gone, so a unique violation names only the line number.
    fn insert_copied(
        &mut self,
        c: &CopyLines,
        src: Row,
        ctx: &ExecCtx,
    ) -> Result<Option<(Row, Vec<u8>, Option<Row>)>> {
        let (line, raw) = std::mem::take(&mut self.copy_line);
        let raw_len = raw.len();
        let frames = &ctx.sess.frames;
        let n = {
            let mut f = frames.borrow_mut();
            f.push(Frame::Copy {
                table: c.table.clone(),
                line,
                raw: Some(raw),
            });
            f.len() - 1
        };
        let r = self.insert(src, ctx).map_err(|e| {
            if c.multi
                && !e.stacked
                && (e.code == code::UNIQUE_VIOLATION || e.code == code::EXCLUSION_VIOLATION)
            {
                frames.borrow_mut()[n] = Frame::Copy {
                    table: c.table.clone(),
                    line,
                    raw: None,
                };
            }
            e.in_frames(&frames.borrow())
        });
        frames.borrow_mut().truncate(n);
        if let Some(l) = &self.copy_load {
            l.row_done(raw_len, ctx);
        }
        r
    }

    /// ON CONFLICT for a row colliding in the target, or in a leaf partition (`leaf`) when
    /// the target is partitioned: the action's expressions are over the target's columns.
    fn on_conflict(
        &mut self,
        c: Conflict,
        excluded: Row,
        leaf: Option<(&PartRouting, usize)>,
        ctx: &ExecCtx,
    ) -> Result<Option<(Row, Vec<u8>, Row)>> {
        let l = leaf.map(|(r, i)| &r.leaves[i]);
        let rules = l.map_or(&self.plan.rules, |l| &l.rules);
        let t = &rules.table;
        let oc = self.plan.on_conflict.as_ref().unwrap();
        match &oc.action {
            // ExecCheckTIDVisible: a snapshot transaction can't skip a row its snapshot
            // doesn't see.
            ConflictAction::Nothing => {
                let txn = ctx.txn.borrow();
                if txn.isolation.txn_snapshot()
                    && !self.touched.contains(&c.existing)
                    && txn.changed_since_snapshot(&c.existing)?
                {
                    return Err(Error::new(
                        code::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent update",
                    ));
                }
                Ok(None)
            }
            ConflictAction::Update {
                sets,
                filter,
                rls_existing,
                rls_new,
            } => {
                // (A row this command inserted or updated is invisible to its snapshot.)
                if self.touched.contains(&c.existing)
                    || self_modified(&c.existing, ctx) != SelfModified::No
                {
                    return Err(Error::new(code::CARDINALITY_VIOLATION, "ON CONFLICT DO UPDATE command cannot affect row a second time")
                        .hint("Ensure that no rows proposed for insertion within the same command have duplicate constrained values."));
                }
                let name = t.name.clone();
                // ExecUpdateLockMode: FOR UPDATE when the SET names a key column.
                // (`sets` are the named table's columns.)
                let keys = crate::exec::scan::key_columns(&self.plan.rules.table, &ctx.catalog());
                let mode = if sets.iter().any(|(c, _)| keys.contains(c)) {
                    RowLockMode::Update
                } else {
                    RowLockMode::NoKeyUpdate
                };
                // The version the conflict was found with: changed or gone once the lock is
                // ours (TM_Updated / TM_Deleted), the insertion starts over in READ
                // COMMITTED; a snapshot transaction fails.
                let found = ctx.txn.borrow().get_latest(&c.existing)?.map(|(ts, _)| ts);
                let xact_ctx = super::scan::xltw_context(Xltw::Lock, &name, &c.existing, ctx);
                ctx.txn.borrow_mut().lock_row(
                    t.oid,
                    &c.existing,
                    mode,
                    WaitPolicy::Block,
                    &|| name.clone(),
                    &xact_ctx,
                )?;
                let latest = ctx.txn.borrow().get_latest(&c.existing)?;
                let snapshot = ctx.txn.borrow().isolation.txn_snapshot();
                let Some((ts, Some(bytes))) = latest else {
                    if snapshot {
                        return Err(Error::new(
                            code::SERIALIZATION_FAILURE,
                            "could not serialize access due to concurrent delete",
                        ));
                    }
                    self.retry = Some((leaf.map(|(_, i)| i), c.prepared.clone()));
                    return Err(Error::new(RETRY_INSERT, "retry"));
                };
                if Some(ts) != found {
                    if snapshot {
                        return Err(Error::new(
                            code::SERIALIZATION_FAILURE,
                            "could not serialize access due to concurrent update",
                        ));
                    }
                    self.retry = Some((leaf.map(|(_, i)| i), c.prepared.clone()));
                    return Err(Error::new(RETRY_INSERT, "retry"));
                }
                // ExecCheckTupleVisible.
                if snapshot && ctx.txn.borrow().changed_since_snapshot(&c.existing)? {
                    return Err(Error::new(
                        code::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent update",
                    ));
                }
                let existing = decode_table_row(t, &bytes)?;
                // Over the target's columns.
                let existing_t = l.map_or_else(|| existing.clone(), |l| from_leaf(l, &existing));
                let mut combined = existing_t.clone();
                combined.extend(excluded);
                if let Some(f) = filter
                    && !eval_bool(f, &combined, ctx)?
                {
                    return Ok(None);
                }
                check_rls(rls_existing, &existing_t, ctx)?;
                let mut new_t = existing_t.clone();
                let mut old_t = existing_t;
                // RETURNING old's system columns: the version this replaces.
                if self.plan.sys {
                    let xid = ctx.txn.borrow_mut().cur_xid();
                    let mut sys = super::scan::sys_values(
                        &c.existing,
                        super::scan::row_xmin(&bytes)?,
                        xid,
                        t.oid,
                    );
                    if let Some((b, o)) = super::scan::stored_tid(&bytes) {
                        sys[0] = Value::Tid(b, o);
                    }
                    old_t.extend(sys);
                }
                assign(&mut new_t, sets, &combined, ctx)?;
                let mut new = match l {
                    Some(l) => {
                        let mut r = existing.clone();
                        for (v, m) in new_t.into_iter().zip(&l.map) {
                            if let Some(j) = m {
                                r[*j] = v;
                            }
                        }
                        r
                    }
                    None => new_t,
                };
                if trigger::any(&rules.triggers, true, true, tt::UPDATE, ctx) {
                    let cols: Vec<usize> = sets.iter().map(|(c, _)| *c).collect();
                    match trigger::before_row(
                        &rules.triggers,
                        t,
                        tt::UPDATE,
                        Some(&existing),
                        new,
                        &cols,
                        ctx,
                    )? {
                        Some(r) => new = r,
                        None => return Ok(None),
                    }
                }
                // Changed while the new values were computed (by a data-modifying CTE they
                // read, or a trigger): the statement's own change stands.
                match self_modified(&c.existing, ctx) {
                    SelfModified::No => {}
                    SelfModified::ByThis => return Ok(None),
                    SelfModified::ByLater => return Err(triggered_change("updated")),
                }
                let stored = match l {
                    Some(l) => {
                        // The row must stay in its partition.
                        let mut g = new.clone();
                        for (i, e) in rules.generated.iter().enumerate() {
                            if let Some(e) = e {
                                g[i] = eval(e, &g, ctx)?;
                            }
                        }
                        let fits = match &l.check {
                            Some(levels) => {
                                crate::sql::binder::partition::check_levels(levels, &mut |key| {
                                    key.iter().map(|e| eval(e, &g, ctx)).collect()
                                })?
                            }
                            None => true,
                        };
                        if !fits {
                            return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "invalid ON UPDATE specification")
                                .detail("The result tuple would appear in a different partition than the original tuple."));
                        }
                        check_rls(rls_new, &from_leaf(l, &g), ctx)?;
                        update_row_rls(rules, &[], &c.existing, &existing, new, ctx)?
                    }
                    None => update_row_rls(rules, rls_new, &c.existing, &existing, new, ctx)?,
                };
                if trigger::wants_after(&rules.triggers, tt::UPDATE, ctx) {
                    let ev = AfterRow {
                        event: tt::UPDATE,
                        old: Some(existing.clone()),
                        new: Some(stored.clone()),
                        key: stored_key(t, &stored, &c.existing),
                    };
                    match leaf {
                        Some((_, i)) => self.routed_after.push((i, ev)),
                        None => self.after.push(ev),
                    }
                }
                // A partition's update goes into the partitioned table's transition tables.
                if let Some(l) = l
                    && trigger::has_transition(&self.plan.rules.triggers, tt::UPDATE)
                {
                    self.after.push(AfterRow {
                        event: tt::UPDATE | trigger::CAPTURE,
                        old: Some(from_leaf(l, &existing)),
                        new: Some(from_leaf(l, &stored)),
                        ..Default::default()
                    });
                }
                let new_key = if t.storage_pkey().is_some() {
                    data_key(t, &stored, None)
                } else {
                    c.existing.clone()
                };
                self.touched.insert(new_key.clone());
                let _ = c.index;
                Ok(Some((
                    l.map_or_else(|| stored.clone(), |l| from_leaf(l, &stored)),
                    new_key,
                    old_t,
                )))
            }
        }
    }
}

impl Node for InsertNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let _target = DmlTarget::push(
            self.plan.rules.table.clone(),
            &self.plan.rules.table,
            &self.plan.target,
        );
        if self.done {
            return Ok(None);
        }
        if self.queue_mark.is_none() {
            self.queue_mark = Some(ctx.after_queue.borrow().len());
        }
        loop {
            ctx.check()?;
            let next = match &self.plan.copy {
                Some(c) => match next_copied(c, ctx)? {
                    Some((row, line)) => {
                        self.copy_line = line;
                        Some(row)
                    }
                    None => None,
                },
                None => self.source.next(ctx)?,
            };
            let Some(src) = next else {
                self.done = true;
                // (ExecPendingInserts: a batch of remote INSERTs not full yet. COPY's
                // CopyMultiInsertBufferFlush names only the relation: relname_only.)
                crate::ext::postgres_fdw::modify::flush_pending(ctx).map_err(|e| match &self.plan.copy {
                    Some(c) => {
                        let line = format!("COPY {}", c.table);
                        let ctx_lines = e.context.as_deref().map_or(line.clone(), |x| format!("{x}\n{line}"));
                        e.context(ctx_lines)
                    }
                    None => e,
                })?;
                if let Some(l) = self.copy_load.take() {
                    l.finish(ctx);
                }
                check_statement_rechecks(ctx)?;
                let rules = &self.plan.rules;
                // AFTER STATEMENT: UPDATE (ON CONFLICT) before INSERT, as ExecEndPlan fires them.
                let mut events = Self::events(&self.plan);
                events.reverse();
                if let Some(c) = &self.plan.copy {
                    if let Some(p) = ctx.sess.copy_progress.borrow_mut().as_mut() {
                        p.tuples_processed = ctx.processed.get() as i64;
                    }
                    let notice = c.feed.lock().unwrap().skipped_notice.clone();
                    if let Some(msg) = notice {
                        ctx.notice(crate::session::notice(msg));
                    }
                }
                let mark = self.queue_mark.unwrap_or(0);
                trigger::batch(ctx, || {
                    let before = ctx.after_queue.borrow().len();
                    if let Some(routing) = &self.plan.route {
                        finish_routed(routing, std::mem::take(&mut self.routed_after), ctx)?;
                    }
                    trigger::finish(
                        &rules.triggers,
                        &rules.table,
                        std::mem::take(&mut self.after),
                        &events,
                        &Self::conflict_cols(&self.plan),
                        ctx,
                    )?;
                    trigger::hoist_row_events(ctx, mark, before);
                    Ok(())
                })?;
                return Ok(None);
            };
            let inserted = match self.plan.copy.clone() {
                Some(c) => self.insert_copied(&c, src, ctx)?,
                None => self.insert(src, ctx)?,
            };
            let Some((mut row, key, old)) = inserted else {
                continue;
            };
            ctx.processed.set(ctx.processed.get() + 1);
            if self.plan.returning.is_empty() {
                continue;
            }
            if self.plan.sys && self.virtual_slot && reads_xact_columns(&self.plan.returning, row.len()) {
                return Err(xact_columns_unavailable());
            }
            if self.plan.sys {
                // ON CONFLICT DO UPDATE locks the row first, so its new version has xmax set.
                let xid = ctx.txn.borrow_mut().cur_xid();
                row.extend(sys_values(
                    &key,
                    xid,
                    if old.is_some() { xid } else { 0 },
                    self.last_table,
                ));
                written_tid(&key, &mut row, ctx)?;
                // (A foreign table's row: the ctid its server returned.)
                if self.plan.rules.table.foreign.is_some() {
                    let n = row.len() - super::scan::NUM_SYS_COLS;
                    row[n] = crate::ext::postgres_fdw::modify::last_ctid().unwrap_or(Value::Tid(u32::MAX, 0));
                }
            }
            let row = with_old_new(row.clone(), self.plan.old_new, old.as_deref(), Some(&row));
            return Ok(Some(eval_all(&self.plan.returning, &row, ctx)?));
        }
    }
}

/// Write the assignments' values over `row` into `new` (UPDATE's SET, MERGE's UPDATE SET, ON
/// CONFLICT DO UPDATE SET): a multiple-column assignment's subquery (`SET (a, b) = (SELECT
/// …)`, each column a field of the same subquery: a MULTIEXPR SubPlan) runs once for the row.
pub fn assign(
    new: &mut [Value],
    sets: &[(usize, crate::sql::expr::Expr)],
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<()> {
    use crate::sql::expr::Expr;
    let mut ran: Vec<(usize, Value)> = Vec::new();
    for (col, e) in sets {
        let mut multi = false;
        e.walk(&mut |x| multi |= crate::sql::expr::multi_assign_field(x).is_some());
        new[*col] = if multi {
            // (Its result, a row, in place of the subquery.)
            let mut err = None;
            let e = e.clone().transform(&mut |x| {
                Ok(match crate::sql::expr::multi_assign_field(&x) {
                    Some((sq, k)) => {
                        let v = match ran.iter().find(|(id, _)| *id == sq.id) {
                            Some((_, v)) => v.clone(),
                            None => match ctx.eval_subquery(sq, row) {
                                Ok(v) => {
                                    ran.push((sq.id, v.clone()));
                                    v
                                }
                                Err(e) => {
                                    err.get_or_insert(e);
                                    Value::Null
                                }
                            },
                        };
                        Expr::Field(Box::new(Expr::Const(v, DataType::Record)), k, x.ty())
                    }
                    None => x,
                })
            })?;
            if let Some(e) = err {
                return Err(e);
            }
            eval(&e, row, ctx)?
        } else {
            eval(e, row, ctx)?
        };
    }
    Ok(())
}

/// The heap TID the version stored under `key` was written at (when modelled).
fn tid_at(key: &[u8], ctx: &ExecCtx) -> Result<Option<Value>> {
    let bytes = ctx.txn.borrow().get(key)?;
    Ok(bytes
        .and_then(|b| super::scan::stored_tid(&b))
        .map(|(b, o)| Value::Tid(b, o)))
}

/// Put the written version's heap TID in the ctid of a RETURNING row (its system columns
/// at the end).
fn written_tid(key: &[u8], row: &mut [Value], ctx: &ExecCtx) -> Result<()> {
    let n = row.len();
    if n >= super::scan::NUM_SYS_COLS
        && let Some(t) = tid_at(key, ctx)?
    {
        row[n - super::scan::NUM_SYS_COLS] = t;
    }
    Ok(())
}

// ── UPDATE ──

/// Whose AFTER ROW events an event of an inheritance tree's UPDATE / DELETE is: the table
/// the statement names, one of its members, or a partition a row moved to (its leaf in the
/// routing).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Owner {
    Root,
    Member(usize),
    Leaf(usize),
}

/// The table a row's storage key is in (row identity: what an inheritance tree's source row
/// says about which member it came from, as its tableoid does).
fn key_table(dkey: &[u8]) -> u32 {
    dkey.get(..4)
        .map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The member a row of an inheritance tree's source is in, if not the table named.
fn member_at<T>(members: &[T], table: impl Fn(&T) -> u32, dkey: &[u8]) -> Option<usize> {
    if members.is_empty() {
        return None;
    }
    let oid = key_table(dkey);
    members.iter().position(|m| table(m) == oid)
}

/// A member's row as its table holds it (ExecGetUpdateNewTuple's fetch of the old tuple): its
/// own columns may be more than the named table's, in another order, so it is read again by
/// its key, unless its layout is the named table's.
fn member_row(
    t: &Table,
    map: &[Option<usize>],
    src: &[Value],
    dkey: &[u8],
    ctx: &ExecCtx,
) -> Result<Row> {
    let n = map.len();
    if t.columns.len() == n && map.iter().enumerate().all(|(i, m)| *m == Some(i)) {
        return Ok(src[..n].to_vec());
    }
    // (A foreign member's row is its server's: as its target scan returned it whole, else the
    // named table's columns put in place.)
    if t.foreign.is_some() {
        if let Some(row) = crate::ext::postgres_fdw::modify::take_member_row(dkey) {
            return Ok(row);
        }
        let mut row = vec![Value::Null; t.columns.len()];
        for (i, m) in map.iter().enumerate() {
            if let (Some(c), Some(v)) = (m, src.get(i)) {
                row[*c] = v.clone();
            }
        }
        // (Its own columns the named table lacks, when its row triggers see the row or
        // a BEFORE ROW trigger makes the UPDATE send every column.)
        let missing: Vec<usize> = (0..t.columns.len()).filter(|c| !t.columns[*c].dropped && !map.contains(&Some(*c))).collect();
        if !missing.is_empty()
            && crate::ext::postgres_fdw::scan::is_postgres_fdw(&ctx.catalog(), t)
            && crate::ext::postgres_fdw::modify::catalog_tgtypes(&ctx.catalog(), t.oid).iter().any(|ty| ty & crate::catalog::trigger_type::ROW != 0)
        {
            crate::ext::postgres_fdw::modify::fill_member_row(t, &mut row, &missing, dkey, ctx)?;
        }
        return Ok(row);
    }
    let bytes = ctx.txn.borrow().get_latest(dkey)?.and_then(|(_, b)| b);
    match bytes {
        Some(b) => decode_table_row(t, &b),
        None => Err(crate::error::internal("member row not found")),
    }
}

/// The columns an UPDATE assigns, with the generated columns computed from them
/// (ExecGetAllUpdatedCols: UPDATE OF triggers on those fire too).
pub(crate) fn updated_cols(rules: &TableRules, sets: &[(usize, crate::sql::expr::Expr)]) -> Vec<usize> {
    let mut cols: Vec<usize> = sets.iter().map(|(c, _)| *c).collect();
    for (i, g) in rules.generated.iter().enumerate() {
        if let Some(g) = g
            && !cols.contains(&i)
            && g.columns().iter().any(|c| cols.contains(c))
        {
            cols.push(i);
        }
    }
    cols
}

/// Whether a table's triggers read transition tables of `event`.
pub(super) fn reads_transition(trigs: &[crate::sql::plan::BoundTrigger], event: u16) -> bool {
    trigs.iter().any(|b| {
        b.trig.tgtype & event != 0 && (b.trig.old_table.is_some() || b.trig.new_table.is_some())
    })
}

pub struct UpdateNode {
    plan: super::planref::PlanRef<UpdatePlan>,
    source: BoxNode,
    /// The rows updated so far, when the source can return a row more than once (a join).
    seen: Option<super::fasthash::HashSet<Vec<u8>>>,
    set_cols: Vec<usize>,
    /// Each member's columns assigned (as `set_cols`).
    member_cols: Vec<Vec<usize>>,
    after: Vec<AfterRow>,
    /// AFTER ROW events of rows moved to other partitions.
    routed_after: Vec<(usize, AfterRow)>,
    /// An inheritance tree's AFTER ROW events, in the order its rows changed.
    ordered: Vec<(Owner, AfterRow)>,
    /// An inheritance tree's rows for the named table's transition tables (as its rows).
    captured: Vec<AfterRow>,
    /// The named table's triggers read those.
    trans: bool,
    done: bool,
    /// A member of an inheritance UPDATE: the Append fires its AFTER triggers.
    deferred: bool,
    /// Keep the rows for the parent's transition tables.
    capture: bool,
}

/// Captured rows (see `trigger::CAPTURE`) taken out of a queue.
fn take_capture(after: &mut Vec<AfterRow>) -> Vec<AfterRow> {
    let (got, keep) = std::mem::take(after)
        .into_iter()
        .partition(|a| a.event & trigger::CAPTURE != 0);
    *after = keep;
    got
}

impl UpdateNode {
    pub fn new(plan: super::planref::PlanRef<UpdatePlan>, ctx: &ExecCtx) -> Result<Self> {
        // (ExecInitModifyTable: a foreign member can't give its rows to the transition
        // tables.)
        if plan.inh && reads_transition(&plan.rules.triggers, tt::UPDATE) && plan.members.iter().any(|m| m.rules.table.foreign.is_some()) {
            return Err(crate::ext::postgres_fdw::modify::transition_error());
        }
        ctx.txn.borrow_mut().output_cid();
        if !plan.rules.view {
            check_replica_identity(&plan.rules.table, crate::catalog::pubsub::RiCmd::Update, ctx)?;
            for m in &plan.members {
                check_replica_identity(&m.rules.table, crate::catalog::pubsub::RiCmd::Update, ctx)?;
            }
            let cat = ctx.catalog();
            let tables = std::iter::once(&plan.rules.table).chain(plan.members.iter().map(|m| &m.rules.table));
            if let Some(e) = tables.filter_map(|t| cat.fdw_modify_error(t, "update")).next() {
                return Err(e);
            }
        }
        let deferred = ctx.defer_after.replace(false);
        let capture = ctx.capture.replace(false);
        let set_cols = updated_cols(&plan.rules, &plan.sets);
        let member_cols = plan
            .members
            .iter()
            .map(|m| updated_cols(&m.rules, &m.sets))
            .collect();
        trigger::statement(
            &plan.rules.triggers,
            &plan.rules.table,
            true,
            tt::UPDATE,
            &set_cols,
            ctx,
        )?;
        let trans = plan.inh && reads_transition(&plan.rules.triggers, tt::UPDATE);
        let source = super::nodes::open(&plan.source, ctx)?;
        let seen = (!rows_once(&plan.source)).then(Default::default);
        Ok(UpdateNode {
            plan,
            source,
            seen,
            set_cols,
            member_cols,
            after: Vec::new(),
            routed_after: Vec::new(),
            ordered: Vec::new(),
            captured: Vec::new(),
            trans,
            done: false,
            deferred,
            capture,
        })
    }

    /// An inheritance tree's AFTER events: each table's row events in the order the rows
    /// changed, then the named table's statement events.
    fn finish_tree(&mut self, ctx: &ExecCtx) -> Result<()> {
        let p = &self.plan;
        let mut runs: Vec<trigger::Run> = Vec::new();
        let mut cur: Option<Owner> = None;
        for (o, a) in std::mem::take(&mut self.ordered) {
            if cur != Some(o) {
                cur = Some(o);
                runs.push(match o {
                    Owner::Root => trigger::Run {
                        trigs: &p.rules.triggers,
                        table: &p.rules.table,
                        set_cols: &self.set_cols,
                        rows: Vec::new(),
                        root: true,
                    },
                    Owner::Member(m) => trigger::Run {
                        trigs: &p.members[m].rules.triggers,
                        table: &p.members[m].rules.table,
                        set_cols: &self.member_cols[m],
                        rows: Vec::new(),
                        root: false,
                    },
                    Owner::Leaf(k) => {
                        let l = &p.route.as_ref().expect("a routed row").leaves[k];
                        trigger::Run {
                            trigs: &l.rules.triggers,
                            table: &l.rules.table,
                            set_cols: &[],
                            rows: Vec::new(),
                            root: false,
                        }
                    }
                });
            }
            runs.last_mut().expect("a run").rows.push(a);
        }
        trigger::fire_runs(
            &p.rules.triggers,
            &p.rules.table,
            &[tt::UPDATE],
            &self.set_cols,
            &self.captured,
            runs,
            ctx,
        )
    }
}

/// ExecCrossPartitionUpdate: delete the row from its partition (with DELETE triggers) and
/// insert the new version through the routing. Returns it over the partition's columns,
/// with the table it went to and its key.
#[allow(clippy::too_many_arguments)]
pub(super) fn move_row(
    rules: &TableRules,
    routing: &PartRouting,
    dkey: &[u8],
    old: &Row,
    new: Row,
    routed_after: &mut Vec<(usize, AfterRow)>,
    ctx: &ExecCtx,
) -> Result<Option<(Row, u32, Vec<u8>)>> {
    let t = &rules.table;
    let Some(mi) = routing
        .leaves
        .iter()
        .position(|l| l.rules.table.oid == t.oid)
    else {
        return Err(crate::error::internal(
            "moved row from outside the partition tree",
        ));
    };
    let me = &routing.leaves[mi];
    // (The RLS UPDATE check and the views' check options look at the row as the destination
    // partition's BEFORE INSERT triggers leave it: after the insert below.)
    // ExecCrossPartitionUpdateForeignKey: a foreign key of its own pointing at an ancestor
    // between the source partition and the updated table can't follow the move.
    {
        let cat = ctx.catalog();
        let root = routing.router.table.oid;
        let mut cur = t.inherits.first().copied();
        while let Some(a) = cur
            && a != root
        {
            let Some(at) = cat.tables.get(&a) else {
                break;
            };
            let referenced = cat.tables.values().any(|x| {
                x.constraints.iter().any(|c| {
                    c.parent == 0
                        && matches!(c.kind, ConstraintKind::ForeignKey { ref_table, .. } if ref_table == a)
                })
            });
            if referenced {
                let rname = cat.tables.get(&root).map_or("", |r| r.name.as_str());
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "cannot move tuple across partitions when a non-root ancestor of the source partition is directly referenced in a foreign key",
                )
                .detail(format!(
                    "A foreign key points to ancestor \"{}\" but not the root ancestor \"{rname}\".",
                    at.name
                ))
                .hint(format!("Consider defining the foreign key on table \"{rname}\".")));
            }
            cur = at.inherits.first().copied();
        }
    }
    if trigger::any(&rules.triggers, true, true, tt::DELETE, ctx)
        && !trigger::before_delete(&rules.triggers, t, old, ctx)?
    {
        return Ok(None);
    }
    delete_row(t, &rules.indexes, dkey, old, ctx)?;
    // Queued with the insert's events, which fire after it.
    if trigger::wants_after(&rules.triggers, tt::DELETE, ctx) {
        routed_after.push((
            mi,
            AfterRow {
                event: tt::DELETE | trigger::CROSS,
                old: Some(old.clone()),
                ..Default::default()
            },
        ));
    }
    // This partition's columns as the root's, and back.
    let root_row: Row = me
        .map
        .iter()
        .map(|m| m.map_or(Value::Null, |j| new[j].clone()))
        .collect();
    let Some((leaf, stored, key)) = trigger::moving_row(|| {
        insert_routed(routing, &[], root_row, routed_after, ctx)
    })?
    else {
        return Ok(None);
    };
    {
        let mut moved = vec![Value::Null; t.columns.len()];
        for (i, m) in me.map.iter().enumerate() {
            if let Some(j) = m
                && let Some(v) = stored.get(i)
            {
                moved[*j] = v.clone();
            }
        }
        check_rls(&rules.rls_checks, &moved, ctx)?;
        check_view_options(rules, &moved, ctx)?;
    }
    // Foreign keys referencing the partitioned table act on the move as an UPDATE of it
    // (ExecCrossPartitionUpdateForeignKey).
    if trigger::any(&rules.triggers, true, false, tt::UPDATE, ctx) {
        let root = &routing.router.table;
        let trigs = trigger::ri_bound(&ctx.catalog(), root.oid, true);
        if !trigs.is_empty() {
            let old_root: Row = me
                .map
                .iter()
                .map(|m| m.map_or(Value::Null, |j| old[j].clone()))
                .collect();
            let ev = AfterRow {
                event: tt::UPDATE,
                old: Some(old_root),
                new: Some(stored.clone()),
                key: Vec::new(),
            };
            trigger::queue_after(
                &trigs,
                root,
                vec![ev],
                &[],
                &[],
                true,
                false,
                Vec::new(),
                ctx,
            )?;
        }
    }
    ctx.moved.borrow_mut().insert(key.clone());
    ctx.txn.borrow_mut().moved_out.push(dkey.to_vec());
    let mut out = vec![Value::Null; t.columns.len()];
    for (i, m) in me.map.iter().enumerate() {
        if let Some(j) = m {
            out[*j] = stored[i].clone();
        }
    }
    Ok(Some((out, routing.leaves[leaf].rules.table.oid, key)))
}

/// How this transaction changed a row after the running statement's snapshot, which shows
/// it as it was (heap_update's TM_SelfModified).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SelfModified {
    /// It didn't.
    No,
    /// The statement itself did (a row joined twice, or changed by another part of the
    /// statement): it is changed only once.
    ByThis,
    /// A later command did: one a trigger or a function the statement ran.
    ByLater,
}

pub fn self_modified(dkey: &[u8], ctx: &ExecCtx) -> SelfModified {
    let Some(cid) = ctx.command_id() else {
        return SelfModified::No;
    };
    match ctx.txn.borrow().written_by(dkey) {
        Some(c) if c == cid => SelfModified::ByThis,
        Some(c) if c > cid => SelfModified::ByLater,
        _ => SelfModified::No,
    }
}

/// The error for a row a later command (a trigger's) changed before the statement could:
/// `what` is "updated", "deleted" or "updated or deleted".
pub fn triggered_change(what: &str) -> Error {
    Error::new(
        code::TRIGGERED_DATA_CHANGE_VIOLATION,
        format!(
            "tuple to be {what} was already modified by an operation triggered by the current command"
        ),
    )
    .hint("Consider using an AFTER trigger instead of a BEFORE trigger to propagate changes to other rows.")
}

pub(super) fn key_of(v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Bytea(b) => Ok(b.to_vec()),
        _ => Err(crate::error::internal("missing row key")),
    }
}

impl Node for UpdateNode {
    fn dml_table(&self) -> Option<Arc<Table>> {
        Some(self.plan.rules.table.clone())
    }

    fn captures(&self) -> bool {
        trigger::has_transition(&self.plan.rules.triggers, tt::UPDATE)
    }

    fn take_captured(&mut self) -> Vec<AfterRow> {
        take_capture(&mut self.after)
    }

    fn add_captured(&mut self, rows: Vec<AfterRow>) {
        self.after.extend(rows);
    }

    fn end(&mut self, ctx: &ExecCtx, stmt: bool) -> Result<()> {
        let rules = &self.plan.rules;
        let queue = trigger::take_queue(&rules.triggers, &mut self.after);
        trigger::fire_after(
            &rules.triggers,
            &rules.table,
            queue,
            &[tt::UPDATE],
            &self.set_cols,
            !stmt,
            stmt,
            ctx,
        )?;
        if !stmt && let Some(routing) = &self.plan.route {
            finish_routed(routing, std::mem::take(&mut self.routed_after), ctx)?;
        }
        Ok(())
    }

    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let sets: Vec<usize> = self.plan.sets.iter().map(|s| s.0).collect();
        let _target = DmlTarget::push(
            self.plan.rules.table.clone(),
            &self.plan.rules.table,
            &sets,
        );
        let n = self.plan.rules.table.columns.len();
        if self.done {
            return Ok(None);
        }
        loop {
            ctx.check()?;
            let Some(mut src) = self.source.next(ctx)? else {
                self.done = true;
                crate::ext::postgres_fdw::modify::flush_pending(ctx)?;
                crate::ext::postgres_fdw::modify::forget_member_rows();
                check_statement_rechecks(ctx)?;
                if self.plan.inh {
                    trigger::batch(ctx, || self.finish_tree(ctx))?;
                } else if !self.deferred {
                    trigger::batch(ctx, || {
                        self.end(ctx, false)?;
                        self.end(ctx, true)
                    })?;
                }
                return Ok(None);
            };
            if self.plan.rules.view {
                let rules = &self.plan.rules;
                let t = rules.table.clone();
                let old: Row = src[..n].to_vec();
                let mut new = old.clone();
                assign(&mut new, &self.plan.sets, &src, ctx)?;
                let Some(stored) =
                    trigger::instead(&rules.triggers, &t, tt::UPDATE, Some(&old), Some(new), ctx)?
                else {
                    continue;
                };
                check_view_options(rules, &stored, ctx)?;
                ctx.processed.set(ctx.processed.get() + 1);
                if self.plan.returning.is_empty() {
                    continue;
                }
                let mut out = stored;
                out.push(Value::Null);
                out.extend(src[n + 1..].iter().cloned());
                let out = with_old_new(out.clone(), self.plan.old_new, Some(&old), Some(&out[..n]));
                return Ok(Some(eval_all(&self.plan.returning, &out, ctx)?));
            }
            let dkey = key_of(&src[n])?;
            // The row is changed in the table it is in: the one named, or a member.
            let member = member_at(&self.plan.members, |m| m.rules.table.oid, &dkey);
            let (rules, msets, map, cols, owner) = match member {
                Some(m) => {
                    let mm = &self.plan.members[m];
                    (&mm.rules, &mm.sets, Some(&mm.map[..]), &self.member_cols[m], Owner::Member(m))
                }
                None => (&self.plan.rules, &self.plan.sets, None, &self.set_cols, Owner::Root),
            };
            let t = rules.table.clone();
            if self.seen.as_mut().is_some_and(|s| !s.insert(dkey.clone()))
                || (self.plan.route.is_some() && ctx.moved.borrow().contains(&dkey))
            {
                // A row joined more than once is updated once; one moved here by this
                // statement isn't updated again.
                continue;
            }
            match self_modified(&dkey, ctx) {
                SelfModified::No => {}
                SelfModified::ByThis => continue,
                SelfModified::ByLater => return Err(triggered_change("updated")),
            }
            let old: Row = match lock_row(&t, &dkey, RowMark::NoKeyUpdate, Wait::Block, Xltw::Update, ctx)? {
                Locked::Skip => continue,
                Locked::Same => match map {
                    Some(map) => member_row(&t, map, &src, &dkey, ctx)?,
                    None => src[..n].to_vec(),
                },
                Locked::Newer(bytes) => {
                    let fresh = decode_table_row(&t, &bytes)?;
                    let named = match map {
                        Some(map) => crate::sql::plan::row_as_parent(map, &fresh),
                        None => fresh.clone(),
                    };
                    if let Some(f) = &self.plan.recheck
                        && !eval_bool(f, &named, ctx)?
                    {
                        continue;
                    }
                    // (A TID scan's condition, on the new version's TID.)
                    if let Some(tid) = super::scan::stored_tid(&bytes)
                        && super::scan::tid_recheck(&self.plan.source, t.oid, tid, ctx)? == Some(false)
                    {
                        continue;
                    }
                    src.splice(0..n, named);
                    fresh
                }
            };
            // The rows as the named table's (RETURNING, its transition tables).
            let as_named = |r: &Row| match map {
                Some(map) => crate::sql::plan::row_as_parent(map, r),
                None => r.clone(),
            };
            let mut new = old.clone();
            assign(&mut new, msets, &src, ctx)?;
            if trigger::any(&rules.triggers, true, true, tt::UPDATE, ctx) {
                match trigger::before_row(
                    &rules.triggers,
                    &t,
                    tt::UPDATE,
                    Some(&old),
                    new,
                    cols,
                    ctx,
                )? {
                    Some(r) => new = r,
                    None => continue,
                }
            }
            // Changed while the new values were computed, or by a BEFORE trigger.
            match self_modified(&dkey, ctx) {
                SelfModified::No => {}
                SelfModified::ByThis => continue,
                SelfModified::ByLater => return Err(triggered_change("updated")),
            }
            // heap_update: changing a key column takes the row FOR UPDATE (what FOR KEY
            // SHARE lockers, foreign key checks among them, conflict with).
            let keys = crate::exec::scan::key_columns(&t, &ctx.catalog());
            if keys.iter().any(|&i| old.get(i) != new.get(i)) {
                lock_row_mode(&t, &dkey, RowLockMode::Update, Wait::Block, Xltw::Update, ctx)?;
            }
            // A row leaving its partition moves through the partitioned table updated. (Not
            // a foreign partition's: the remote server updates it in place, ExecForeignUpdate.)
            if let Some(routing) = &self.plan.route
                && t.foreign.is_none()
            {
                for (i, g) in rules.generated.iter().enumerate() {
                    if let Some(e) = g {
                        new[i] = eval(e, &new, ctx)?;
                    }
                }
                if !partition_ok(rules, &new, ctx)? {
                    let mut routed = Vec::new();
                    // (postgresBeginForeignInsert: not into a foreign partition this UPDATE
                    // changes too.)
                    let targets = self.plan.members.iter().filter(|m| m.rules.table.foreign.is_some()).map(|m| m.rules.table.oid).collect();
                    let _targets = crate::ext::postgres_fdw::modify::update_targets(targets);
                    // (A foreign partition's statement returns what RETURNING reads.)
                    let _routed_back = if self.plan.returning.is_empty() {
                        None
                    } else {
                        let named = &self.plan.rules.table;
                        let exprs: Vec<&Expr> = self.plan.returning.iter().collect();
                        let b = crate::ext::postgres_fdw::modify::Back::of(named, &[], tt::UPDATE, &exprs, None);
                        crate::ext::postgres_fdw::modify::routed_back(named, b)
                    };
                    let moved = move_row(rules, routing, &dkey, &old, new, &mut routed, ctx);
                    if self.plan.inh {
                        self.ordered
                            .extend(routed.into_iter().map(|(k, a)| (Owner::Leaf(k), a)));
                    } else {
                        self.routed_after.extend(routed);
                    }
                    let Some(moved) = moved? else {
                        continue;
                    };
                    ctx.processed.set(ctx.processed.get() + 1);
                    if self.trans {
                        self.captured.push(AfterRow {
                            event: tt::UPDATE,
                            old: Some(as_named(&old)),
                            new: Some(as_named(&moved.0)),
                            key: Vec::new(),
                        });
                    } else if self.capture {
                        self.after.push(AfterRow {
                            event: tt::UPDATE | trigger::CAPTURE,
                            old: Some(old.clone()),
                            new: Some(moved.0.clone()),
                            key: Vec::new(),
                        });
                    }
                    if self.plan.returning.is_empty() {
                        continue;
                    }
                    let (stored, table, new_key) = moved;
                    if self.plan.sys
                        && routing
                            .leaves
                            .iter()
                            .find(|l| l.rules.table.oid == table)
                            .is_some_and(same_layout)
                        && reads_xact_columns(&self.plan.returning, n + 1)
                    {
                        return Err(xact_columns_unavailable());
                    }
                    let mut out = as_named(&stored);
                    out.push(Value::Null);
                    out.extend(src[n + 1..].iter().cloned());
                    if self.plan.sys {
                        let xid = ctx.txn.borrow_mut().cur_xid();
                        for (i, v) in sys_values(&new_key, xid, 0, table).into_iter().enumerate() {
                            out[n + 1 + i] = v;
                        }
                        if let Some(Value::Tid(b, o)) = tid_at(&new_key, ctx)? {
                            out[n + 1] = Value::Tid(b, o);
                        }
                    }
                    let sys = self.plan.sys.then_some(n + 1);
                    let out = with_old_new(
                        out.clone(),
                        self.plan.old_new,
                        Some(&image(&src, n, sys)),
                        Some(&image(&out, n, sys)),
                    );
                    return Ok(Some(eval_all(&self.plan.returning, &out, ctx)?));
                }
            }
            let set_cols = crate::ext::postgres_fdw::modify::target_cols(&t, msets.iter().map(|(c, _)| *c), &rules.generated, &crate::ext::postgres_fdw::modify::tgtypes(&rules.triggers));
            let stored = if t.foreign.is_some() {
                // (A postgres_fdw table's row comes back as the remote server stored it, for
                // RETURNING, the views' check options and AFTER ROW triggers; the views'
                // check options run on it.)
                let exprs: Vec<&Expr> = self.plan.returning.iter().chain(rules.view_checks.iter().map(|(_, e)| e)).collect();
                let back = crate::ext::postgres_fdw::modify::Back::of(&t, &crate::ext::postgres_fdw::modify::tgtypes(&rules.triggers), tt::UPDATE, &exprs, self.plan.sys.then_some(n + 1))
                    .with_old_new(&t, self.plan.old_new.as_ref(), &exprs);
                let _back = crate::ext::postgres_fdw::modify::with_back(back);
                let stored = crate::ext::postgres_fdw::modify::with_set_cols(set_cols, || update_row(rules, &dkey, &old, new, ctx))?;
                check_view_options(rules, &stored, ctx)?;
                stored
            } else {
                update_row(rules, &dkey, &old, new, ctx)?
            };
            if self.plan.inh {
                if trigger::any(&rules.triggers, true, false, tt::UPDATE, ctx) {
                    self.ordered.push((
                        owner,
                        AfterRow {
                            event: tt::UPDATE,
                            old: Some(old.clone()),
                            new: Some(stored.clone()),
                            key: stored_key(&t, &stored, &dkey),
                        },
                    ));
                }
                if self.trans {
                    self.captured.push(AfterRow {
                        event: tt::UPDATE,
                        old: Some(as_named(&old)),
                        new: Some(as_named(&stored)),
                        key: Vec::new(),
                    });
                }
            } else {
                if trigger::wants_after(&rules.triggers, tt::UPDATE, ctx) {
                    self.after.push(AfterRow {
                        event: tt::UPDATE,
                        old: Some(old.clone()),
                        new: Some(stored.clone()),
                        key: stored_key(&t, &stored, &dkey),
                    });
                }
                if self.capture {
                    self.after.push(AfterRow {
                        event: tt::UPDATE | trigger::CAPTURE,
                        old: Some(old.clone()),
                        new: Some(stored.clone()),
                        key: Vec::new(),
                    });
                }
            }
            ctx.processed.set(ctx.processed.get() + 1);
            if self.plan.returning.is_empty() {
                continue;
            }
            let new_key = if t.storage_pkey().is_some() {
                data_key(&t, &stored, None)
            } else {
                dkey.clone()
            };
            let mut out = as_named(&stored);
            out.push(Value::Null); // key slot
            out.extend(src[n + 1..].iter().cloned());
            if self.plan.sys {
                let xid = ctx.txn.borrow_mut().cur_xid();
                for (i, v) in sys_values(&new_key, xid, 0, t.oid).into_iter().enumerate() {
                    out[n + 1 + i] = v;
                }
                if t.foreign.is_some() {
                    // (A foreign table's row: the ctid its server returned.)
                    out[n + 1] = crate::ext::postgres_fdw::modify::last_ctid().unwrap_or(Value::Tid(u32::MAX, 0));
                } else if let Some(Value::Tid(b, o)) = tid_at(&new_key, ctx)? {
                    out[n + 1] = Value::Tid(b, o);
                }
            }
            let sys = self.plan.sys.then_some(n + 1);
            let out = with_old_new(
                out.clone(),
                self.plan.old_new,
                Some(&image(&src, n, sys)),
                Some(&image(&out, n, sys)),
            );
            return Ok(Some(eval_all(&self.plan.returning, &out, ctx)?));
        }
    }
}

// ── DELETE ──

pub struct DeleteNode {
    plan: super::planref::PlanRef<DeletePlan>,
    source: BoxNode,
    /// The rows deleted so far, when the source can return a row more than once (a join).
    seen: Option<super::fasthash::HashSet<Vec<u8>>>,
    after: Vec<AfterRow>,
    /// An inheritance tree's AFTER ROW events, in the order its rows were deleted.
    ordered: Vec<(Owner, AfterRow)>,
    /// An inheritance tree's rows for the named table's transition tables (as its rows).
    captured: Vec<AfterRow>,
    trans: bool,
    done: bool,
    deferred: bool,
    capture: bool,
    /// Its rows' index entries are deleted together at its end (`defers_entries`).
    defer_entries: bool,
}

impl DeleteNode {
    /// Whether nothing the statement runs before it ends can read the deleted rows' index
    /// entries through the transaction's writes (its own snapshot doesn't see them): no
    /// BEFORE ROW trigger, RETURNING or anything volatile, so they can wait for its end
    /// (`Txn::defer_entry_delete`).
    fn defers_entries(plan: &DeletePlan, ctx: &ExecCtx) -> bool {
        !plan.view
            && plan.returning.is_empty()
            && !trigger::any(&plan.triggers, true, true, tt::DELETE, ctx)
            && plan.members.iter().all(|m| !trigger::any(&m.triggers, true, true, tt::DELETE, ctx))
            && !crate::sql::paths::plan_volatile_deep(&plan.source)
    }

    pub fn new(plan: super::planref::PlanRef<DeletePlan>, ctx: &ExecCtx) -> Result<Self> {
        if plan.inh && reads_transition(&plan.triggers, tt::DELETE) && plan.members.iter().any(|m| m.table.foreign.is_some()) {
            return Err(crate::ext::postgres_fdw::modify::transition_error());
        }
        ctx.txn.borrow_mut().output_cid();
        if !plan.view {
            check_replica_identity(&plan.table, crate::catalog::pubsub::RiCmd::Delete, ctx)?;
            for m in &plan.members {
                check_replica_identity(&m.table, crate::catalog::pubsub::RiCmd::Delete, ctx)?;
            }
            let cat = ctx.catalog();
            let tables = std::iter::once(&plan.table).chain(plan.members.iter().map(|m| &m.table));
            if let Some(e) = tables.filter_map(|t| cat.fdw_modify_error(t, "delete from")).next() {
                return Err(e);
            }
        }
        let deferred = ctx.defer_after.replace(false);
        let capture = ctx.capture.replace(false);
        trigger::statement(&plan.triggers, &plan.table, true, tt::DELETE, &[], ctx)?;
        let trans = plan.inh && reads_transition(&plan.triggers, tt::DELETE);
        let source = super::nodes::open(&plan.source, ctx)?;
        let seen = (!rows_once(&plan.source)).then(Default::default);
        let defer_entries = Self::defers_entries(&plan, ctx);
        Ok(DeleteNode {
            defer_entries,
            plan,
            source,
            seen,
            after: Vec::new(),
            ordered: Vec::new(),
            captured: Vec::new(),
            trans,
            done: false,
            deferred,
            capture,
        })
    }

    /// An inheritance tree's AFTER events (see `UpdateNode::finish_tree`).
    fn finish_tree(&mut self, ctx: &ExecCtx) -> Result<()> {
        let p = &self.plan;
        let mut runs: Vec<trigger::Run> = Vec::new();
        let mut cur: Option<Owner> = None;
        for (o, a) in std::mem::take(&mut self.ordered) {
            if cur != Some(o) {
                cur = Some(o);
                let (trigs, table) = match o {
                    Owner::Member(m) => (&p.members[m].triggers, &p.members[m].table),
                    _ => (&p.triggers, &p.table),
                };
                runs.push(trigger::Run {
                    trigs,
                    table,
                    set_cols: &[],
                    rows: Vec::new(),
                    root: o == Owner::Root,
                });
            }
            runs.last_mut().expect("a run").rows.push(a);
        }
        trigger::fire_runs(
            &p.triggers,
            &p.table,
            &[tt::DELETE],
            &[],
            &self.captured,
            runs,
            ctx,
        )
    }
}

impl Node for DeleteNode {
    fn dml_table(&self) -> Option<Arc<Table>> {
        Some(self.plan.table.clone())
    }

    fn captures(&self) -> bool {
        trigger::has_transition(&self.plan.triggers, tt::DELETE)
    }

    fn take_captured(&mut self) -> Vec<AfterRow> {
        take_capture(&mut self.after)
    }

    fn add_captured(&mut self, rows: Vec<AfterRow>) {
        self.after.extend(rows);
    }

    fn end(&mut self, ctx: &ExecCtx, stmt: bool) -> Result<()> {
        let queue = trigger::take_queue(&self.plan.triggers, &mut self.after);
        trigger::fire_after(
            &self.plan.triggers,
            &self.plan.table,
            queue,
            &[tt::DELETE],
            &[],
            !stmt,
            stmt,
            ctx,
        )
    }

    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let n = self.plan.table.columns.len();
        if self.done {
            return Ok(None);
        }
        loop {
            ctx.check()?;
            let Some(mut src) = self.source.next(ctx)? else {
                self.done = true;
                crate::ext::postgres_fdw::modify::flush_pending(ctx)?;
                crate::ext::postgres_fdw::modify::forget_member_rows();
                if self.defer_entries {
                    ctx.txn.borrow_mut().flush_entry_deletes()?;
                }
                if self.plan.inh {
                    trigger::batch(ctx, || self.finish_tree(ctx))?;
                } else if !self.deferred {
                    trigger::batch(ctx, || {
                        self.end(ctx, false)?;
                        self.end(ctx, true)
                    })?;
                }
                return Ok(None);
            };
            if self.plan.view {
                let t = self.plan.table.clone();
                let old: Row = src[..n].to_vec();
                if trigger::instead(&self.plan.triggers, &t, tt::DELETE, Some(&old), None, ctx)?
                    .is_none()
                {
                    continue;
                }
                ctx.processed.set(ctx.processed.get() + 1);
                if self.plan.returning.is_empty() {
                    continue;
                }
                let src = with_old_new(src, self.plan.old_new, Some(&old), None);
                return Ok(Some(eval_all(&self.plan.returning, &src, ctx)?));
            }
            let dkey = key_of(&src[n])?;
            // The row is deleted from the table it is in: the one named, or a member.
            let member = member_at(&self.plan.members, |m| m.table.oid, &dkey);
            let (t, triggers, indexes, map, owner) = match member {
                Some(m) => {
                    let mm = &self.plan.members[m];
                    (&mm.table, &mm.triggers, &mm.indexes, Some(&mm.map[..]), Owner::Member(m))
                }
                None => (
                    &self.plan.table,
                    &self.plan.triggers,
                    &self.plan.indexes,
                    None,
                    Owner::Root,
                ),
            };
            let t = t.clone();
            if self.seen.as_mut().is_some_and(|s| !s.insert(dkey.clone())) {
                continue;
            }
            let before = trigger::any(triggers, true, true, tt::DELETE, ctx);
            match self_modified(&dkey, ctx) {
                SelfModified::No => {}
                SelfModified::ByThis => continue,
                // (Found locking the row for its BEFORE triggers: GetTupleForTrigger's.)
                SelfModified::ByLater => {
                    return Err(triggered_change(if before { "updated" } else { "deleted" }));
                }
            }
            let old: Row = match lock_row(&t, &dkey, RowMark::Update, Wait::Block, Xltw::Delete, ctx)? {
                Locked::Skip => continue,
                Locked::Same => match map {
                    Some(map) => member_row(&t, map, &src, &dkey, ctx)?,
                    None => src[..n].to_vec(),
                },
                Locked::Newer(bytes) => {
                    let fresh = decode_table_row(&t, &bytes)?;
                    let named = match map {
                        Some(map) => crate::sql::plan::row_as_parent(map, &fresh),
                        None => fresh.clone(),
                    };
                    if let Some(f) = &self.plan.recheck
                        && !eval_bool(f, &named, ctx)?
                    {
                        continue;
                    }
                    // (A TID scan's condition, on the new version's TID.)
                    if let Some(tid) = super::scan::stored_tid(&bytes)
                        && super::scan::tid_recheck(&self.plan.source, t.oid, tid, ctx)? == Some(false)
                    {
                        continue;
                    }
                    src.splice(0..n, named);
                    fresh
                }
            };
            if before && !trigger::before_delete(triggers, &t, &old, ctx)? {
                continue;
            }
            // (Only a BEFORE trigger, run since the check above, can have changed it.)
            match if before { self_modified(&dkey, ctx) } else { SelfModified::No } {
                SelfModified::No => {}
                SelfModified::ByThis => continue,
                SelfModified::ByLater => return Err(triggered_change("deleted")),
            }
            {
                // (A postgres_fdw table's statement returns what RETURNING and AFTER ROW
                // triggers read.)
                let _back = t.foreign.is_some().then(|| {
                    let exprs: Vec<&Expr> = self.plan.returning.iter().collect();
                    let n = self.plan.table.columns.len();
                    let back = crate::ext::postgres_fdw::modify::Back::of(&t, &crate::ext::postgres_fdw::modify::tgtypes(triggers), tt::DELETE, &exprs, self.plan.sys.then_some(n + 1))
                        .with_old_new(&t, self.plan.old_new.as_ref(), &exprs);
                    crate::ext::postgres_fdw::modify::with_back(back)
                });
                delete_row_with(&t, indexes, &dkey, &old, self.defer_entries, ctx)?;
            }
            if self.plan.inh {
                if trigger::any(triggers, true, false, tt::DELETE, ctx) {
                    self.ordered.push((
                        owner,
                        AfterRow {
                            event: tt::DELETE,
                            old: Some(old.clone()),
                            ..Default::default()
                        },
                    ));
                }
                if self.trans {
                    self.captured.push(AfterRow {
                        event: tt::DELETE,
                        old: Some(match map {
                            Some(map) => crate::sql::plan::row_as_parent(map, &old),
                            None => old.clone(),
                        }),
                        ..Default::default()
                    });
                }
            } else {
                if trigger::wants_after(triggers, tt::DELETE, ctx) {
                    self.after.push(AfterRow {
                        event: tt::DELETE,
                        old: Some(old.clone()),
                        ..Default::default()
                    });
                }
                if self.capture {
                    self.after.push(AfterRow {
                        event: tt::DELETE | trigger::CAPTURE,
                        old: Some(old.clone()),
                        ..Default::default()
                    });
                }
            }
            ctx.processed.set(ctx.processed.get() + 1);
            if self.plan.returning.is_empty() {
                continue;
            }
            // The deleted version's xmax is this transaction.
            if self.plan.sys && src.len() > n + 4 {
                let xid = ctx.txn.borrow_mut().cur_xid();
                src[n + 4] = Value::Oid(xid as u32);
            }
            let old = image(
                &src,
                n,
                self.plan.old_new.is_some_and(|o| o.sys).then_some(n + 1),
            );
            let src = with_old_new(src, self.plan.old_new, Some(&old), None);
            return Ok(Some(eval_all(&self.plan.returning, &src, ctx)?));
        }
    }
}

/// A row's `n` columns, then its system columns when they're at `sys`.
pub(super) fn image(row: &[Value], n: usize, sys: Option<usize>) -> Row {
    let mut r = row[..n].to_vec();
    if let Some(p) = sys {
        r.extend_from_slice(&row[p..p + NUM_SYS_COLS]);
    }
    r
}

/// RETURNING old / new: the row RETURNING sees, padded to its width, then the old and the
/// new row, each followed by a flag saying it exists.
pub(super) fn with_old_new(
    mut row: Row,
    on: Option<OldNew>,
    old: Option<&[Value]>,
    new: Option<&[Value]>,
) -> Row {
    let Some(on) = on else { return row };
    row.resize(on.width, Value::Null);
    for r in [old, new] {
        match r {
            Some(r) => {
                row.extend(r.iter().take(on.ncols).cloned());
                row.resize(row.len() + on.ncols.saturating_sub(r.len()), Value::Null);
                row.push(Value::Bool(true));
            }
            None => {
                row.resize(row.len() + on.ncols, Value::Null);
                row.push(Value::Bool(false));
            }
        }
    }
    row
}

/// TRUNCATE: delete every row and index entry of a table.
pub fn truncate(t: &Arc<Table>, ctx: &ExecCtx) -> Result<()> {
    // (Rows, index entries and out-of-line values.)
    let start = crate::storage::keys::index_prefix(t.oid, 0);
    let end = super::toast::table_end(t.oid);
    // (One range deletion, however many rows, entries and values: `storage::rdel`.)
    ctx.txn.borrow_mut().delete_range(start, end)?;
    // (Its indexes' cached entries go with their records: `index_am::evict`.)
    crate::ext::index_am::evict(ctx.sess.db.instance, &crate::storage::keys::index_prefix(t.oid, 0)[..4]);
    super::sizing::truncated(t, &ctx.sess.db);
    super::sizing::indexes_emptied(t, ctx);
    Ok(())
}
