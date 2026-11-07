//! Direct modification (postgresPlanDirectModify / BeginDirectModify): an UPDATE or DELETE
//! whose conditions and new values can all run remotely is sent as one statement.

use super::connection;
use super::deparse::{Deparser, relation_name};
use super::scan::is_postgres_fdw;
use crate::catalog::{Catalog, Table, trigger_type as tt};
use crate::error::Result;
use crate::exec::{BoxNode, ExecCtx, Node, Row};
use crate::remote::conn::Status;
use crate::sql::expr::Expr;
use crate::sql::plan::{DeletePlan, Plan, ScanPlan, UpdatePlan};
use crate::types::Value;
use std::sync::Arc;

/// The remote statement of a direct modification, and the columns its RETURNING list
/// returns (in table order).
pub struct Direct {
    pub sql: String,
    pub returned: Vec<usize>,
    pub params: Vec<Expr>,
    /// A join's statement (the target joined remotely, `UPDATE ... FROM` / `DELETE ...
    /// USING`): where its RETURNING columns go in the source row (deparseExplicitTargetList).
    pub join: Option<JoinBack>,
}

/// `Direct::join`: the width of the source row RETURNING reads, and per remote RETURNING
/// column where it goes.
pub struct JoinBack {
    pub width: usize,
    pub items: Vec<Back>,
}

/// A remote RETURNING column of a join's statement: one source row column (its type), or a
/// non-target relation's whole row, its fields going to their source row columns.
pub enum Back {
    Col(usize, crate::types::DataType),
    Whole(Vec<Option<usize>>, Vec<(String, crate::types::DataType)>),
}

/// The scan of the target a plain UPDATE / DELETE reads (no join, no projection).
fn target_scan(p: &Plan, oid: u32) -> Option<&ScanPlan> {
    match p {
        Plan::Scan(s) if s.table.oid == oid && s.mark.is_none() && s.current_of.is_none() && s.sample.is_none() => Some(s),
        Plan::Project { input, exprs } if exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i)) => {
            target_scan(input, oid)
        }
        _ => None,
    }
}

/// has_row_triggers: a row trigger (BEFORE or AFTER) of the event keeps the statement local.
fn has_row_triggers(trigs: &[crate::sql::plan::BoundTrigger], event: u16) -> bool {
    trigs.iter().any(|b| b.trig.tgtype & tt::ROW != 0 && b.trig.tgtype & event != 0)
}

/// deparseReturningList: the columns RETURNING reads (all of them for a whole row); of the
/// system columns, the tableoid is the node's own (none goes remotely).
fn returning(t: &Table, list: &[Expr], d: &Deparser) -> Option<(String, Vec<usize>)> {
    if list.is_empty() {
        return Some((String::new(), Vec::new()));
    }
    let n = t.columns.len();
    let tableoid = n + 1 + crate::exec::scan::NUM_SYS_COLS - 1;
    let mut used = vec![false; n];
    for e in list {
        for c in e.columns() {
            if c == tableoid {
                continue;
            }
            if c >= n {
                return None;
            }
            used[c] = true;
        }
    }
    let cols: Vec<usize> = (0..n).filter(|i| used[*i] && !t.columns[*i].dropped).collect();
    if cols.is_empty() {
        return Some((String::new(), cols));
    }
    let list = cols.iter().map(|i| d.column(*i)).collect::<Vec<_>>().join(", ");
    Some((format!(" RETURNING {list}"), cols))
}

/// An UPDATE sent whole: its remote statement, when PostgreSQL would send it so.
pub fn update(up: &UpdatePlan, cat: &Catalog, fmt: &crate::types::text::FormatCtx) -> Option<Direct> {
    if !update_ok(up, cat) {
        return None;
    }
    let t = &up.rules.table;
    let Some(s) = target_scan(&up.source, t.oid) else { return super::directjoin::join_update(up, cat, fmt) };
    let spec = super::scan::spec(s, None, cat, fmt);
        || !is_postgres_fdw(cat, t)
        || up.inh
        || !up.members.is_empty()
        || up.route.is_some()
        || up.rules.view
        || up.old_new.is_some()
        || !up.rules.rls_checks.is_empty()
        || !up.rules.view_checks.is_empty()
        || has_row_triggers(&up.rules.triggers, tt::UPDATE)
    if !spec.local.is_empty() || !spec.params.is_empty() {
        return None;
    }
    let d = Deparser::new(cat, t, fmt);
    let n = t.columns.len();
    let mut sets: Vec<&(usize, Expr)> = up.sets.iter().collect();
    sets.sort_by_key(|x| x.0);
    let mut parts = Vec::new();
    for (c, e) in sets {
        if e.columns().iter().any(|x| *x >= n) || !d.shippable(e) {
            return None;
        }
        parts.push(format!("{} = {}", d.column(*c), d.expr(e)));
    }
    let (ret, returned) = returning(t, &up.returning, &d)?;
    let mut sql = format!("UPDATE {} SET {}", relation_name(cat, t), parts.join(", "));
    if !spec.remote.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&d.conditions(&spec.remote));
    }
    sql.push_str(&ret);
    let params = d.params.borrow().clone();
    if !params.is_empty() {
        return None;
    }
    Some(Direct { sql, returned, params, join: None })
}

/// postgresPlanDirectModify's checks of an UPDATE apart from its source.
fn update_ok(up: &UpdatePlan, cat: &Catalog) -> bool {
    let t = &up.rules.table;
    !(t.foreign.is_none()
        || !is_postgres_fdw(cat, t)
        || up.inh
        || !up.members.is_empty()
        || up.route.is_some()
        || up.rules.view
        || up.old_new.is_some()
        || !up.rules.rls_checks.is_empty()
        || !up.rules.view_checks.is_empty()
        || has_row_triggers(&up.rules.triggers, tt::UPDATE)
        || t.columns.iter().any(|c| c.generated.is_some() && !c.virtual_gen && !c.dropped))
}

/// postgresPlanDirectModify's checks of a DELETE apart from its source.
fn delete_ok(dp: &DeletePlan, cat: &Catalog) -> bool {
    let t = &dp.table;
    !(t.foreign.is_none()
        || !is_postgres_fdw(cat, t)
        || dp.inh
        || !dp.members.is_empty()
        || dp.view
        || dp.old_new.is_some()
        || has_row_triggers(&dp.triggers, tt::DELETE))
}

/// An UPDATE's foreign target, or a DELETE's with row triggers of it: its whole row goes up
/// (add_row_identity_columns), part of its scan's width.
pub fn wholerow_target(p: &Plan) -> Option<u32> {
    match p {
        Plan::Update(up) if up.rules.table.foreign.is_some() && !up.inh => Some(up.rules.table.oid),
        Plan::Delete(dp) if dp.table.foreign.is_some() && !dp.inh && has_row_triggers(&dp.triggers, tt::DELETE) => Some(dp.table.oid),
        _ => None,
    }
}

/// The target of a query level's UPDATE / DELETE that may be joined remotely with the
/// statement's other postgres_fdw relations (its scan returning `rN.ctid` as the row's key):
/// one the statement could be sent whole for (`Planner::fdw_dml_target`).
pub fn join_target(cat: &Catalog, p: &Plan) -> Option<u32> {
    match p {
        Plan::Update(up) if up.rules.table.foreign.is_some() => update_ok(up, cat).then_some(up.rules.table.oid),
        Plan::Delete(dp) if dp.table.foreign.is_some() => delete_ok(dp, cat).then_some(dp.table.oid),
        _ => None,
    }
}

/// A DELETE sent whole.
pub fn delete(dp: &DeletePlan, cat: &Catalog, fmt: &crate::types::text::FormatCtx) -> Option<Direct> {
    if !delete_ok(dp, cat) {
        return None;
    }
    let t = &dp.table;
    let Some(s) = target_scan(&dp.source, t.oid) else { return super::directjoin::join_delete(dp, cat, fmt) };
    let spec = super::scan::spec(s, None, cat, fmt);
    if !spec.local.is_empty() || !spec.params.is_empty() {
        return None;
    }
    let d = Deparser::new(cat, t, fmt);
    let (ret, returned) = returning(t, &dp.returning, &d)?;
    let mut sql = format!("DELETE FROM {}", relation_name(cat, t));
    if !spec.remote.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&d.conditions(&spec.remote));
    }
    sql.push_str(&ret);
    Some(Direct { sql, returned, params: Vec::new(), join: None })
}

/// A join's RETURNING row (`JoinBack`) as the source row RETURNING reads.
fn join_row(jb: &JoinBack, vals: Vec<Option<String>>, ctx: &ExecCtx) -> Result<Row> {
    let mut row = vec![Value::Null; jb.width];
    for (item, v) in jb.items.iter().zip(vals) {
        let Some(s) = v else { continue };
        match item {
            Back::Col(i, ty) => {
                if let Some(x) = row.get_mut(*i) {
                    *x = crate::types::text::parse_value(&s, ty, ctx.fmt())?;
                }
            }
            Back::Whole(slots, fields) => {
                if let Value::Record(vals) = crate::types::text::parse_record(&s, fields, ctx.fmt())? {
                    for (slot, v) in slots.iter().zip(vals.iter()) {
                        if let Some(x) = slot.and_then(|i| row.get_mut(i)) {
                            *x = v.clone();
                        }
                    }
                }
            }
        }
    }
    Ok(row)
}

/// The running statement: sent at the first row asked for, then its RETURNING rows.
struct DirectNode {
    table: Arc<Table>,
    direct: Direct,
    returning: Vec<Expr>,
    rows: Option<std::vec::IntoIter<Row>>,
    /// The table's statement triggers, its event and the columns it sets: the node fires
    /// them as ModifyTable does (BEFORE at the start, AFTER at the end unless `deferred`).
    triggers: Vec<crate::sql::plan::BoundTrigger>,
    event: u16,
    set_cols: Vec<usize>,
    deferred: bool,
}

impl DirectNode {
    fn start(table: Arc<Table>, direct: Direct, returning: Vec<Expr>, triggers: Vec<crate::sql::plan::BoundTrigger>, event: u16, set_cols: Vec<usize>, ctx: &ExecCtx) -> Result<BoxNode> {
        let deferred = ctx.defer_after.replace(false);
        crate::exec::trigger::statement(&triggers, &table, true, event, &set_cols, ctx)?;
        Ok(Box::new(DirectNode { table, direct, returning, rows: None, triggers, event, set_cols, deferred }))
    }
}

/// The node of an UPDATE sent whole, when it is one. (Kept out of the executor's
/// recursive `open`: its frame is on every plan level's stack.)
#[inline(never)]
pub fn open_update(up: &UpdatePlan, ctx: &ExecCtx) -> Result<Option<BoxNode>> {
    let t = &up.rules.table;
    if t.foreign.is_none() {
        return Ok(None);
    }
    let Some(direct) = update(up, &ctx.catalog(), ctx.fmt()) else { return Ok(None) };
    let set_cols = crate::exec::dml::updated_cols(&up.rules, &up.sets);
    DirectNode::start(t.clone(), direct, up.returning.clone(), up.rules.triggers.clone(), tt::UPDATE, set_cols, ctx).map(Some)
}

/// The node of a DELETE sent whole, when it is one.
#[inline(never)]
pub fn open_delete(dp: &DeletePlan, ctx: &ExecCtx) -> Result<Option<BoxNode>> {
    if dp.table.foreign.is_none() {
        return Ok(None);
    }
    let Some(direct) = delete(dp, &ctx.catalog(), ctx.fmt()) else { return Ok(None) };
    DirectNode::start(dp.table.clone(), direct, dp.returning.clone(), dp.triggers.clone(), tt::DELETE, Vec::new(), ctx).map(Some)
}

impl Node for DirectNode {
    fn dml_table(&self) -> Option<Arc<Table>> {
        Some(self.table.clone())
    }

    fn end(&mut self, ctx: &ExecCtx, stmt: bool) -> Result<()> {
        if stmt {
            crate::exec::trigger::fire_after(&self.triggers, &self.table, Vec::new(), &[self.event], &self.set_cols, false, true, ctx)?;
        }
        Ok(())
    }

    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.rows.is_none() {
            let m = super::scan::mapping_for(&self.table, None, ctx)?;
            let key = connection::get_connection(ctx, &m, false)?;
            let intr = crate::remote::interrupts(ctx);
            let sql = self.direct.sql.clone();
            let r = connection::with_conn(ctx, key, |c| {
                let r = c.exec(&sql, &intr)?;
                if !matches!(r.status, Status::CommandOk | Status::TuplesOk) {
                    return Err(connection::report(&r, c, Some(&sql)));
                }
                Ok(r)
            });
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    connection::note_error(ctx, key);
                    return Err(e);
                }
            };
            ctx.processed.set(ctx.processed.get() + r.cmd_tuples());
            let t = &self.table;
            let mut out = Vec::new();
            if !self.returning.is_empty() {
                // (Nothing returned remotely: a row of NULLs per row changed,
                // get_returning_data without has_returning.)
                if let Some(jb) = &self.direct.join {
                    let rows = if jb.items.is_empty() { vec![Vec::new(); r.cmd_tuples() as usize] } else { r.rows };
                    for vals in rows {
                        let row = join_row(jb, vals, ctx)?;
                        let vals: Result<Row> = self.returning.iter().map(|e| crate::sql::expr::eval(e, &row, ctx)).collect();
                        out.push(vals?);
                    }
                    self.rows = Some(out.into_iter());
                    return self.next(ctx);
                }
                let rows: Vec<Vec<Option<String>>> = if self.direct.returned.is_empty() {
                    vec![Vec::new(); r.cmd_tuples() as usize]
                } else {
                    r.rows
                };
                for vals in rows {
                    // (The row as ModifyTable has it: the columns, the key slot, the system
                    // columns, its tableoid the table's.)
                    let n = t.columns.len();
                    let mut row = vec![Value::Null; n + 1 + crate::exec::scan::NUM_SYS_COLS];
                    row[n + crate::exec::scan::NUM_SYS_COLS] = Value::Oid(t.oid);
                    for (k, &i) in self.direct.returned.iter().enumerate() {
                        if let Some(Some(s)) = vals.get(k) {
                            let col = &t.columns[i];
                            row[i] = crate::types::text::parse_value(s, &col.ty, ctx.fmt()).map_err(|e| {
                                e.context(format!("column \"{}\" of foreign table \"{}\"", col.name, t.name))
                            })?;
                        }
                    }
                    let vals: Result<Row> = self.returning.iter().map(|e| crate::sql::expr::eval(e, &row, ctx)).collect();
                    out.push(vals?);
                }
            }
            self.rows = Some(out.into_iter());
        }
        let next = self.rows.as_mut().and_then(Iterator::next);
        if next.is_none() && !self.deferred {
            // (Once: the statement's AFTER triggers, as ModifyTable's end does.)
            self.deferred = true;
            crate::exec::trigger::batch(ctx, || self.end(ctx, true))?;
        }
        Ok(next)
    }
}

/// The scans of an inherited UPDATE / DELETE's members that are its Appends' children (the
/// source no more than projections over the tree's Appends: no join, PostgreSQL's
/// find_modifytable_subplan).
fn member_scans<'p>(p: &'p mut Plan, out: &mut Vec<&'p mut ScanPlan>) -> bool {
    match p {
        Plan::Scan(s) => {
            if s.table.foreign.is_some() && s.with_key {
                out.push(s);
            }
            true
        }
        Plan::Project { input, .. } => member_scans(Arc::make_mut(input), out),
        Plan::Append { inputs, inh: Some(_), .. } => inputs.iter_mut().all(|i| member_scans(Arc::make_mut(i), out)),
        Plan::Empty { .. } => true,
        _ => false,
    }
}

/// `e` over the named table's columns as over the member's (`map`), when it reads no other.
fn as_member(e: &Expr, map: &[Option<usize>]) -> Option<Expr> {
    let mut ok = true;
    let e = e
        .clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Col(c, t) => match map.get(c).copied().flatten() {
                    Some(j) => Expr::Col(j, t),
                    None => {
                        ok = false;
                        Expr::Col(c, t)
                    }
                },
                x => x,
            })
        })
        .ok()?;
    ok.then_some(e)
}

/// The member columns RETURNING reads (the named table's, mapped; its tableoid is the
/// node's own), None when it reads another relation's or a system column.
fn member_returned(list: &[Expr], named: usize, map: &[Option<usize>], member: &Table) -> Option<Vec<usize>> {
    let tableoid = named + 1 + crate::exec::scan::NUM_SYS_COLS - 1;
    let mut used = vec![false; member.columns.len()];
    for e in list {
        for c in e.columns() {
            if c == tableoid {
                continue;
            }
            used[map.get(c).copied().flatten()?] = true;
        }
    }
    Some((0..used.len()).filter(|i| used[*i] && !member.columns[*i].dropped).collect())
}

/// postgresPlanDirectModify for an inherited UPDATE / DELETE's foreign members (each an
/// Append child of its own): a member with no row triggers of the event (nor stored
/// generated columns for an UPDATE), all of whose conditions and new values ship, is
/// changed by one remote statement its scan runs (`FdwPushdown::direct`).
pub fn mark_members(cat: &Catalog, fmt: &crate::types::text::FormatCtx, p: &mut Plan) {
    struct M {
        table: Arc<Table>,
        row_trigs: bool,
        sets: Option<Vec<(usize, Expr)>>,
        map: Vec<Option<usize>>,
        checks: bool,
    }
    // (A named table with transition tables: its members' rows go through the node, whose
    // ExecInitModifyTable refuses foreign members.)
    let (trigs, ev) = match p {
        Plan::Update(up) => (&up.rules.triggers, tt::UPDATE),
        Plan::Delete(dp) => (&dp.triggers, tt::DELETE),
        _ => return,
    };
    if trigs.iter().any(|b| b.trig.tgtype & ev != 0 && (b.trig.old_table.is_some() || b.trig.new_table.is_some())) {
        return;
    }
    let (source, members, returning, named, event) = match p {
        Plan::Update(up) => {
            if !up.inh || up.rules.view || up.old_new.is_some() || !up.rules.rls_checks.is_empty() || !up.rules.view_checks.is_empty() {
                return;
            }
            let members: Vec<M> = up
                .members
                .iter()
                .map(|m| M {
                    table: m.rules.table.clone(),
                    row_trigs: has_row_triggers(&m.rules.triggers, tt::UPDATE),
                    sets: Some(m.sets.clone()),
                    map: m.map.clone(),
                    checks: !m.rules.rls_checks.is_empty() || !m.rules.view_checks.is_empty(),
                })
                .collect();
            (&mut up.source, members, up.returning.clone(), up.rules.table.columns.len(), tt::UPDATE)
        }
        Plan::Delete(dp) => {
            if !dp.inh || dp.view || dp.old_new.is_some() {
                return;
            }
            let members: Vec<M> = dp
                .members
                .iter()
                .map(|m| M { table: m.table.clone(), row_trigs: has_row_triggers(&m.triggers, tt::DELETE), sets: None, map: m.map.clone(), checks: false })
                .collect();
            (&mut dp.source, members, dp.returning.clone(), dp.table.columns.len(), tt::DELETE)
        }
        _ => return,
    };
    if !members.iter().any(|m| m.table.foreign.is_some() && is_postgres_fdw(cat, &m.table)) {
        return;
    }
    let mut scans = Vec::new();
    if !member_scans(Arc::make_mut(source), &mut scans) {
        return;
    }
    for s in scans {
        let Some(m) = members.iter().find(|m| m.table.oid == s.table.oid) else { continue };
        let t = &m.table;
        if !is_postgres_fdw(cat, t)
            || m.checks
            || m.row_trigs
            || s.mark.is_some()
            || s.current_of.is_some()
            || s.sample.is_some()
            || s.fdw.as_ref().is_some_and(|f| f.member || !f.order.is_empty() || f.has_limit)
            || (event == tt::UPDATE && t.columns.iter().any(|c| c.generated.is_some() && !c.virtual_gen && !c.dropped))
            || !super::scan::spec(s, None, cat, fmt).local.is_empty()
        {
            continue;
        }
        let d = Deparser::new(cat, t, fmt);
        let mut sets = Vec::new();
        if let Some(list) = &m.sets {
            let n = t.columns.len();
            let mut ok = true;
            for (c, e) in list {
                match as_member(e, &m.map) {
                    Some(x) if *c < n && d.shippable(&x) => sets.push((*c, x)),
                    _ => ok = false,
                }
            }
            if !ok {
                continue;
            }
            sets.sort_by_key(|x| x.0);
        }
        let Some(returned) = member_returned(&returning, named, &m.map, t) else { continue };
        let mut f = s.fdw.as_deref().cloned().unwrap_or_default();
        f.direct = Some(Arc::new(crate::sql::plan::DirectMember { update: m.sets.is_some(), sets, returned, returning: !returning.is_empty() }));
        s.fdw = Some(Arc::new(f));
    }
}

/// A member's remote statement (deparseDirectUpdateSql / deparseDirectDeleteSql) and its
/// parameters.
pub fn member_sql(s: &ScanPlan, dm: &crate::sql::plan::DirectMember, cat: &Catalog, fmt: &crate::types::text::FormatCtx) -> (String, Vec<Expr>) {
    let t = &s.table;
    let spec = super::scan::spec(s, None, cat, fmt);
    let d = Deparser::new(cat, t, fmt);
    let mut sql = if dm.update {
        let parts: Vec<String> = dm.sets.iter().map(|(c, e)| format!("{} = {}", d.column(*c), d.expr(e))).collect();
        format!("UPDATE {} SET {}", relation_name(cat, t), parts.join(", "))
    } else {
        format!("DELETE FROM {}", relation_name(cat, t))
    };
    if !spec.remote.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&d.conditions(&spec.remote));
    }
    if !dm.returned.is_empty() {
        let list = dm.returned.iter().map(|i| d.column(*i)).collect::<Vec<_>>().join(", ");
        sql.push_str(&format!(" RETURNING {list}"));
    }
    let params = d.params.borrow().clone();
    (sql, params)
}

/// The key of a member row its remote statement changed (`FdwPushdown::direct`): the
/// member's key prefix and no remote ctid; the DML node only returns it.
pub fn member_key(oid: u32) -> Vec<u8> {
    let mut k = crate::storage::keys::index_prefix(oid, 0);
    k.push(0xff);
    k
}

/// Whether `key` is a `member_key`.
pub fn is_member_key(key: &[u8]) -> bool {
    key.len() == 9 && key[8] == 0xff
}

/// A member's direct modification: at the first row asked for, the remote statement; then
/// its RETURNING rows as the member's scan returns rows (columns, `member_key`).
pub fn open_member(s: &ScanPlan, dm: &crate::sql::plan::DirectMember, ctx: &ExecCtx) -> Result<BoxNode> {
    let (sql, params) = member_sql(s, dm, &ctx.catalog(), ctx.fmt());
    Ok(Box::new(MemberNode { table: s.table.clone(), sql, params, returned: dm.returned.clone(), returning: dm.returning, sys: s.sys, rows: None }))
}

struct MemberNode {
    table: Arc<Table>,
    sql: String,
    params: Vec<Expr>,
    returned: Vec<usize>,
    returning: bool,
    /// The scan returns the system columns after the key (a foreign row's: no ctid).
    sys: bool,
    rows: Option<std::vec::IntoIter<Row>>,
}

impl MemberNode {
    fn run(&mut self, ctx: &ExecCtx) -> Result<Vec<Row>> {
        // (Parameters as literals of their types, as a scan's cursor sends them.)
        let fmt = super::deparse::transmission_fmt(ctx.fmt());
        let mut sql = self.sql.clone();
        for (i, p) in self.params.iter().enumerate().rev() {
            let v = crate::sql::expr::eval(p, &[], ctx)?;
            let lit = if v.is_null() {
                "NULL".to_string()
            } else {
                super::deparse::string_literal(&crate::types::text::format_value(&v, &p.ty(), &fmt))
            };
            sql = sql.replace(&format!("${}::", i + 1), &format!("{lit}::"));
        }
        let m = super::scan::mapping_for(&self.table, None, ctx)?;
        let key = connection::get_connection(ctx, &m, false)?;
        let intr = crate::remote::interrupts(ctx);
        let r = connection::with_conn(ctx, key, |c| {
            let r = c.exec(&sql, &intr)?;
            if !matches!(r.status, Status::CommandOk | Status::TuplesOk) {
                return Err(connection::report(&r, c, Some(&sql)));
            }
            Ok(r)
        });
        let r = match r {
            Ok(r) => r,
            Err(e) => {
                connection::note_error(ctx, key);
                return Err(e);
            }
        };
        ctx.processed.set(ctx.processed.get() + r.cmd_tuples());
        if !self.returning {
            return Ok(Vec::new());
        }
        let t = &self.table;
        let mkey = Value::Bytea(Arc::from(member_key(t.oid)));
        // (Nothing returned remotely: a row of NULLs per row changed, for RETURNING.)
        let rows: Vec<Vec<Option<String>>> = if self.returned.is_empty() { vec![Vec::new(); r.cmd_tuples() as usize] } else { r.rows };
        let mut out = Vec::with_capacity(rows.len());
        for vals in rows {
            let mut row = vec![Value::Null; t.columns.len()];
            for (k, &i) in self.returned.iter().enumerate() {
                if let Some(Some(s)) = vals.get(k) {
                    let col = &t.columns[i];
                    row[i] = crate::types::text::parse_value(s, &col.ty, ctx.fmt())
                        .map_err(|e| e.context(format!("column \"{}\" of foreign table \"{}\"", col.name, t.name)))?;
                }
            }
            row.push(mkey.clone());
            if self.sys {
                row.extend([Value::Tid(u32::MAX, 0), Value::Oid(0), Value::Oid(0), Value::Oid(0), Value::Oid(0), Value::Oid(t.oid)]);
            }
            out.push(row);
        }
        Ok(out)
    }
}

impl Node for MemberNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.rows.is_none() {
            let rows = self.run(ctx)?;
            self.rows = Some(rows.into_iter());
        }
        Ok(self.rows.as_mut().and_then(Iterator::next))
    }
}
