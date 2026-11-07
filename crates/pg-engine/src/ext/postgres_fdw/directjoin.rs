//! Direct modification of a join (postgresPlanDirectModify over a Foreign Scan of a join
//! holding the target): `UPDATE t r1 SET ... FROM <the other relations> WHERE ...` and
//! `DELETE FROM t r1 USING ...` (deparseDirectUpdateSql / deparseDirectDeleteSql with a
//! join relation), RETURNING the columns of every relation it reads
//! (build_remote_returning, deparseExplicitTargetList).

use super::direct::{Back, Direct, JoinBack};
use super::joinscan::JoinScan;
use super::joinsql::Sql;
use crate::catalog::{Catalog, Table};
use crate::sql::expr::Expr;
use crate::sql::plan::{DeletePlan, Plan, UpdatePlan};

/// The source of an UPDATE / DELETE whose target is joined remotely: its join's scan, the
/// base that is the target, and per source row column the join row column it is (None: a
/// constant the projection puts there).
struct Source<'p> {
    js: &'p JoinScan,
    target: usize,
    cols: Vec<Option<usize>>,
}

fn source(p: &Plan, oid: u32) -> Option<Source<'_>> {
    let (fr, cols) = match p {
        Plan::Foreign(fr) => (fr, fr.out.iter().map(|&g| Some(g)).collect::<Vec<_>>()),
        Plan::Project { input, exprs } => match &**input {
            Plan::Foreign(fr) => (fr, exprs.iter().map(|e| if let Expr::Col(j, _) = e { fr.out.get(*j).copied() } else { None }).collect()),
            _ => return None,
        },
        _ => return None,
    };
    let js = fr.private.0.as_any().downcast_ref::<JoinScan>()?;
    // (fscan->scan.plan.qual: nothing checked locally, nor a sort or LIMIT sent with it.)
    if !js.order.is_empty() || js.limit.is_some() || !js.info.local_conds.is_empty() || !js.info.is_join() {
        return None;
    }
    let target = js.bases.iter().position(|b| b.scan.table.oid == oid && b.scan.with_key)?;
    Some(Source { js, target, cols })
}

impl Source<'_> {
    /// `e` over the source row as over the join row, when every column it reads is there.
    fn over_join(&self, e: &Expr) -> Option<Expr> {
        if !e.columns().iter().all(|&c| self.cols.get(c).copied().flatten().is_some()) {
            return None;
        }
        Some(e.clone().remap_cols(&|c| self.cols[c].unwrap_or(usize::MAX)))
    }

    /// The target relation's alias and name: `<remote name> rN`.
    fn target_rel(&self, cat: &Catalog) -> String {
        let s = &self.js.bases[self.target].scan;
        format!("{} r{}", super::deparse::relation_name(cat, &s.table), s.rtindex.0)
    }

    /// ` FROM ... WHERE ...`: the other relations (`word` FROM or USING) and the conditions,
    /// the join's remote ones then the clauses of the inner joins the target is in.
    fn from_where(&self, q: &Sql, word: &str) -> String {
        let mut ignore = Vec::new();
        let mut additional = Vec::new();
        let from = q.from_without(&self.js.info, self.target, &mut ignore, &mut additional);
        let mut conds = self.js.info.remote_conds.clone();
        conds.extend(ignore);
        let mut sql = format!(" {word} {from}");
        q.where_of(&mut sql, &self.js.info, &conds, &additional);
        sql
    }

    /// build_remote_returning: the target's columns when its whole row is read, then every
    /// column (or another relation's whole row) RETURNING reads, in order, once each.
    /// None when it reads a system column of the target.
    fn returning(&self, t: &Table, list: &[Expr], width: usize, q: &Sql) -> Option<(String, JoinBack)> {
        let n = t.columns.len();
        let base_of = |g: usize| self.js.bases.iter().position(|b| b.cols.contains(&Some(g)));
        let pos = |g: usize| self.cols.iter().position(|c| *c == Some(g));
        let mut items: Vec<Back> = Vec::new();
        let mut sql: Vec<String> = Vec::new();
        let mut seen: Vec<usize> = Vec::new();
        let tcol = |i: usize| self.js.bases[self.target].cols.get(i).copied().flatten();
        let mut bad = false;
        let mut target_whole = false;
        for e in list {
            e.walk(&mut |x| target_whole |= matches!(x, Expr::Row(items, _) if crate::sql::binder::is_whole_row(x) && items.iter().all(|c| matches!(c, Expr::Col(i, _) if *i < n))));
        }
        let push_col = |i: usize, items: &mut Vec<Back>, sql: &mut Vec<String>, seen: &mut Vec<usize>| -> bool {
            if seen.contains(&i) {
                return true;
            }
            let g = if i < n { tcol(i) } else { self.cols.get(i).copied().flatten() };
            let Some(g) = g else { return false };
            let ty = if i < n { t.columns[i].ty.clone() } else { super::join::col_type(&self.js.bases, g) };
            seen.push(i);
            sql.push(q.expr(&self.js.info, &Expr::Col(g, ty.clone())));
            items.push(Back::Col(i, ty));
            true
        };
        if target_whole {
            for i in (0..n).filter(|&i| !t.columns[i].dropped) {
                bad |= !push_col(i, &mut items, &mut sql, &mut seen);
            }
        }
        let mut wholes: Vec<usize> = Vec::new();
        // (Sub-expressions of a whole row already taken: not read again on their own.)
        let mut skip: Vec<*const Expr> = Vec::new();
        for e in list {
            e.walk(&mut |x| {
                if bad || skip.iter().any(|p| std::ptr::eq(*p, x)) {
                    return;
                }
                let row = crate::sql::binder::nullable_whole_row(x).or_else(|| crate::sql::binder::is_whole_row(x).then_some(x));
                if let Some(Expr::Row(cols, _)) = row
                    && let Some(Expr::Col(first, _)) = cols.first()
                    && *first >= n
                {
                    let Some(k) = self.cols.get(*first).copied().flatten().and_then(base_of) else {
                        bad = true;
                        return;
                    };
                    x.walk(&mut |y| skip.push(y as *const Expr));
                    if !wholes.contains(&k) {
                        wholes.push(k);
                        let bt = &self.js.bases[k].scan.table;
                        let live: Vec<usize> = (0..bt.columns.len()).filter(|&c| !bt.columns[c].dropped).collect();
                        let slots = live.iter().map(|&c| self.js.bases[k].cols.get(c).copied().flatten().and_then(pos)).collect();
                        let fields = live.iter().map(|&c| (bt.columns[c].name.clone(), bt.columns[c].ty.clone())).collect();
                        sql.push(self.js.whole_row_sql(k));
                        items.push(Back::Whole(slots, fields));
                    }
                    return;
                }
                if let Expr::Col(i, _) = x {
                    // (The key is the node's own; the target's system columns stay local.)
                    let target_sys = *i > n && self.cols.get(*i).copied().flatten().and_then(base_of) == Some(self.target);
                    if target_sys {
                        bad = true;
                    } else if *i != n {
                        bad |= !push_col(*i, &mut items, &mut sql, &mut seen);
                    }
                }
            });
        }
        if bad {
            return None;
        }
        let text = if sql.is_empty() { String::new() } else { format!(" RETURNING {}", sql.join(", ")) };
        Some((text, JoinBack { width, items }))
    }
}

/// An UPDATE ... FROM sent whole.
pub fn join_update(up: &UpdatePlan, cat: &Catalog, fmt: &crate::types::text::FormatCtx) -> Option<Direct> {
    let t = &up.rules.table;
    let src = source(&up.source, t.oid)?;
    let q = Sql::new(cat, fmt, &src.js.bases);
    let d = super::deparse::Deparser::new(cat, t, fmt);
    let mut sets: Vec<&(usize, Expr)> = up.sets.iter().collect();
    sets.sort_by_key(|x| x.0);
    let mut parts = Vec::new();
    for (c, e) in sets {
        let e = src.over_join(e)?;
        if !q.shippable(&src.js.info, &e) {
            return None;
        }
        parts.push(format!("{} = {}", d.column(*c), q.expr(&src.js.info, &e)));
    }
    let mut sql = format!("UPDATE {} SET {}", src.target_rel(cat), parts.join(", "));
    sql.push_str(&src.from_where(&q, "FROM"));
    let width = crate::sql::binder::plan_width(&up.source);
    let (ret, back) = src.returning(t, &up.returning, width, &q)?;
    sql.push_str(&ret);
    if !q.params.borrow().is_empty() {
        return None;
    }
    Some(Direct { sql, returned: Vec::new(), params: Vec::new(), join: Some(back) })
}

/// A DELETE ... USING sent whole.
pub fn join_delete(dp: &DeletePlan, cat: &Catalog, fmt: &crate::types::text::FormatCtx) -> Option<Direct> {
    let t = &dp.table;
    let src = source(&dp.source, t.oid)?;
    let q = Sql::new(cat, fmt, &src.js.bases);
    let mut sql = format!("DELETE FROM {}", src.target_rel(cat));
    sql.push_str(&src.from_where(&q, "USING"));
    let width = crate::sql::binder::plan_width(&dp.source);
    let (ret, back) = src.returning(t, &dp.returning, width, &q)?;
    sql.push_str(&ret);
    if !q.params.borrow().is_empty() {
        return None;
    }
    Some(Direct { sql, returned: Vec::new(), params: Vec::new(), join: Some(back) })
}
