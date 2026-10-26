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
    let t = &up.rules.table;
    if t.foreign.is_none()
        || !is_postgres_fdw(cat, t)
        || up.inh
        || !up.members.is_empty()
        || up.route.is_some()
        || up.rules.view
        || up.old_new.is_some()
        || !up.rules.rls_checks.is_empty()
        || !up.rules.view_checks.is_empty()
        || has_row_triggers(&up.rules.triggers, tt::UPDATE)
        || t.columns.iter().any(|c| c.generated.is_some() && !c.virtual_gen && !c.dropped)
    {
        return None;
    }
    let s = target_scan(&up.source, t.oid)?;
    let spec = super::scan::spec(s, None, cat, fmt);
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
    Some(Direct { sql, returned, params })
}

/// A DELETE sent whole.
pub fn delete(dp: &DeletePlan, cat: &Catalog, fmt: &crate::types::text::FormatCtx) -> Option<Direct> {
    let t = &dp.table;
    if t.foreign.is_none()
        || !is_postgres_fdw(cat, t)
        || dp.inh
        || !dp.members.is_empty()
        || dp.view
        || dp.old_new.is_some()
        || has_row_triggers(&dp.triggers, tt::DELETE)
    {
        return None;
    }
    let s = target_scan(&dp.source, t.oid)?;
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
    Some(Direct { sql, returned, params: Vec::new() })
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
