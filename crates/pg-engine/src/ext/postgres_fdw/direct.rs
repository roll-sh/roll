//! Direct modification (postgresPlanDirectModify / BeginDirectModify): an UPDATE or DELETE
//! whose conditions and new values can all run remotely is sent as one statement.

use super::connection;
use super::deparse::{Deparser, relation_name};
use super::scan::is_postgres_fdw;
use crate::catalog::{Catalog, Table};
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

/// (Statement triggers too: the direct statement's node fires none.)
fn has_row_triggers(trigs: &[crate::sql::plan::BoundTrigger]) -> bool {
    !trigs.is_empty()
}

/// deparseReturningList: the columns RETURNING reads (all of them for a whole row).
fn returning(t: &Table, list: &[Expr], d: &Deparser) -> Option<(String, Vec<usize>)> {
    if list.is_empty() {
        return Some((String::new(), Vec::new()));
    }
    let n = t.columns.len();
    let mut used = vec![false; n];
    for e in list {
        for c in e.columns() {
            if c >= n {
                return None;
            }
            used[c] = true;
        }
    }
    let cols: Vec<usize> = (0..n).filter(|i| used[*i] && !t.columns[*i].dropped).collect();
    let list = if cols.is_empty() { "NULL".to_string() } else { cols.iter().map(|i| d.column(*i)).collect::<Vec<_>>().join(", ") };
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
        || up.recheck.is_some()
        || up.old_new.is_some()
        || !up.rules.rls_checks.is_empty()
        || !up.rules.view_checks.is_empty()
        || has_row_triggers(&up.rules.triggers)
        || t.columns.iter().any(|c| c.generated.is_some())
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
        || dp.recheck.is_some()
        || dp.old_new.is_some()
        || has_row_triggers(&dp.triggers)
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
}

/// The node of an UPDATE sent whole, when it is one. (Kept out of the executor's
/// recursive `open`: its frame is on every plan level's stack.)
#[inline(never)]
pub fn open_update(up: &UpdatePlan, ctx: &ExecCtx) -> Option<BoxNode> {
    let t = &up.rules.table;
    t.foreign.as_ref()?;
    let direct = update(up, &ctx.catalog(), ctx.fmt())?;
    Some(Box::new(DirectNode { table: t.clone(), direct, returning: up.returning.clone(), rows: None }))
}

/// The node of a DELETE sent whole, when it is one.
#[inline(never)]
pub fn open_delete(dp: &DeletePlan, ctx: &ExecCtx) -> Option<BoxNode> {
    dp.table.foreign.as_ref()?;
    let direct = delete(dp, &ctx.catalog(), ctx.fmt())?;
    Some(Box::new(DirectNode { table: dp.table.clone(), direct, returning: dp.returning.clone(), rows: None }))
}

impl Node for DirectNode {
    fn dml_table(&self) -> Option<Arc<Table>> {
        Some(self.table.clone())
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
                for vals in r.rows {
                    let mut row = vec![Value::Null; t.columns.len() + 1];
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
        Ok(self.rows.as_mut().and_then(Iterator::next))
    }
}
