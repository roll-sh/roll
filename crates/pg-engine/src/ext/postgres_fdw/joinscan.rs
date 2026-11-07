//! A Foreign Scan of a join (postgresBeginForeignScan / IterateForeignScan /
//! ExplainForeignScan for scanrelid 0): the join's remote query read through a cursor,
//! each remote row put into the join row, the local conditions checked on it.

use super::join::{Base, Kind, RelInfo};
use super::joinsql::{Sql, Tail};
use crate::catalog::quote_ident;
use crate::error::Result;
use crate::exec::{BoxNode, ExecCtx, Node, Row};
use crate::sql::expr::Expr;
use crate::sql::plan::ForeignRel;
use crate::types::{DataType, Value};
use std::sync::Arc;

/// postgres_fdw's part of a `Plan::Foreign` of a join.
#[derive(Clone)]
pub struct JoinScan {
    pub info: Arc<RelInfo>,
    pub bases: Vec<Base>,
    /// The join row columns the remote SELECT returns, in its order (fdw_scan_tlist).
    pub tlist: Vec<usize>,
    /// ORDER BY / LIMIT sent with it.
    pub order: Vec<(Expr, bool, bool)>,
    pub limit: Option<(Option<Expr>, Option<Expr>)>,
    /// Base relations whose whole row the plan above reads (a whole-row reference), as
    /// (position in `tlist`, base), in position order: the remote query returns the row as
    /// one ROW there (deparseVar of a whole-row Var), its columns filled from it
    /// (`pushdown::whole_rows`).
    pub whole: Vec<(usize, usize)>,
    /// Under a LockRows over a level locking local relations too: every base's whole row
    /// as its row mark's copy (`exec::scan::copy_key`), as (position in `tlist` it goes
    /// before, base), in base order; the node returns base k's as `FDW_MARK_COL + k`.
    pub marks: Vec<(usize, usize)>,
    /// The join row columns its level's target list reads (needed above the level's whole
    /// join): a base's reltarget has them first, its row mark's junk after them.
    pub top: Vec<usize>,
}

impl JoinScan {
    /// The join row column's type.
    fn col_type(&self, g: usize) -> DataType {
        super::join::col_type(&self.bases, g)
    }

    /// conversion_error_callback: the column a value failed for, by the names the query
    /// knows it by.
    pub(super) fn col_context(&self, g: usize) -> String {
        let Some((b, k)) = self.bases.iter().find_map(|b| b.col(g).map(|k| (b, k))) else { return String::new() };
        let s = &b.scan;
        let Some(c) = s.table.columns.get(k) else { return String::new() };
        let cname = s
            .col_aliases
            .as_ref()
            .and_then(|a| a.iter().find(|(n, _)| *n == c.name).map(|(_, x)| x.clone()))
            .unwrap_or_else(|| c.name.clone());
        let rname = s.alias.clone().unwrap_or_else(|| s.table.name.clone());
        format!("column \"{cname}\" of foreign table \"{rname}\"")
    }

    /// The remote query and its parameters.
    pub fn sql(&self, ctx: &ExecCtx) -> (String, Vec<Expr>) {
        let cat = ctx.catalog();
        let fmt = ctx.fmt();
        let q = Sql::new(&cat, fmt, &self.bases);
        let tlist: Vec<Expr> = self.tlist.iter().map(|&g| Expr::Col(g, self.col_type(g))).collect();
        let (lock, rows) = self.row_marks();
        let whole = self.inserts().into_iter().map(|(at, k, _)| (at, self.whole_row_sql(k))).collect();
        let tail = Tail { order: self.order.clone(), limit: self.limit.clone(), lock, rows, whole, ..Default::default() };
        let sql = q.select(&self.info, Some(&tlist), &self.info.remote_conds, &tail);
        (sql, q.params.take())
    }

    /// The whole rows the remote SELECT list has besides `tlist`'s columns, in order: (their
    /// position in the list, base, whether a row mark's copy).
    fn inserts(&self) -> Vec<(usize, usize, bool)> {
        let mut v: Vec<(usize, usize, bool)> =
            self.whole.iter().map(|&(at, k)| (at, k, false)).chain(self.marks.iter().map(|&(at, k)| (at, k, true))).collect();
        v.sort_by_key(|&(at, _, mark)| (at, mark));
        v.into_iter().enumerate().map(|(n, (at, k, mark))| (at + n, k, mark)).collect()
    }

    /// The base relations whose whole rows the remote query returns (a locking level's
    /// ROW_MARK_COPY marks, every relation's): the locked ones in their clauses' order,
    /// then the others in range table order (preprocess_rowmarks).
    pub fn mark_bases(&self) -> Vec<usize> {
        if !self.marks.is_empty() || !self.bases.iter().any(|b| b.scan.mark.is_some()) {
            return Vec::new();
        }
        let mut order: Vec<usize> = (0..self.bases.len()).collect();
        order.sort_by_key(|&k| {
            let s = &self.bases[k].scan;
            (s.mark.is_none(), s.mark.map(|_| s.lock_order), s.rti.0)
        });
        order
    }

    /// A base relation's whole row as the remote query returns it (NULL for a null-extended
    /// row): `CASE WHEN (rN.*)::text IS NOT NULL THEN ROW(...) END`.
    pub(super) fn whole_row_sql(&self, k: usize) -> String {
        let alias = format!("r{}", self.bases[k].scan.rtindex.0);
        let t = &self.bases[k].scan.table;
        let cols: Vec<String> = (0..t.columns.len())
            .filter(|&i| !t.columns[i].dropped)
            .map(|i| format!("{alias}.{}", quote_ident(&super::deparse::column_name(t, i))))
            .collect();
        let list = if cols.is_empty() { "NULL".to_string() } else { cols.join(", ") };
        format!("CASE WHEN ({alias}.*)::text IS NOT NULL THEN ROW({list}) END")
    }

    /// The UPDATE / DELETE target joined remotely (`Planner::fdw_dml_target`): its scan
    /// returns the row's key, `rN.ctid`.
    fn is_target(&self, k: usize) -> bool {
        let s = &self.bases[k].scan;
        s.with_key && s.fdw.is_none() && s.mark.is_none()
    }

    /// The table whose row key join row column `g` is (the target's `rN.ctid`).
    fn key_of(&self, g: usize) -> Option<u32> {
        let k = (0..self.bases.len()).find(|&k| self.is_target(k))?;
        let b = &self.bases[k];
        (b.col(g) == Some(b.scan.table.columns.len())).then_some(b.scan.table.oid)
    }

    /// deparseLockingClause (`FOR UPDATE OF rN` per locked relation, in range table order,
    /// not those of a lower subquery) and the marked relations' whole rows
    /// (`CASE WHEN (rN.*)::text IS NOT NULL THEN ROW(...) END`).
    fn row_marks(&self) -> (String, Vec<String>) {
        let marked = self.mark_bases();
        let target = (0..self.bases.len()).find(|&k| self.is_target(k));
        if marked.is_empty() && self.marks.is_empty() && target.is_none() {
            return Default::default();
        }
        let alias = |k: usize| format!("r{}", self.bases[k].scan.rtindex.0);
        let rows = marked.iter().map(|&k| self.whole_row_sql(k)).collect();
        let mut by_rti: Vec<usize> = (0..self.bases.len()).filter(|k| self.info.lower_subquery >> k & 1 == 0).collect();
        by_rti.sort_by_key(|&k| self.bases[k].scan.rtindex.0);
        let mut lock = String::new();
        for k in by_rti {
            // (The result relation's rows: FOR UPDATE, all_result_relids.)
            let m = match self.bases[k].scan.mark {
                _ if target == Some(k) => crate::sql::plan::RowMark::Update,
                Some((m, _)) => m,
                None => continue,
            };
            lock.push_str(match m {
                crate::sql::plan::RowMark::Update | crate::sql::plan::RowMark::NoKeyUpdate => " FOR UPDATE",
                crate::sql::plan::RowMark::Share | crate::sql::plan::RowMark::KeyShare => " FOR SHARE",
            });
            if self.info.relids.count_ones() > 1 {
                lock.push_str(&format!(" OF {}", alias(k)));
            }
        }
        (lock, rows)
    }

    /// fpinfo->relation_name, with EXPLAIN's names of the base relations.
    pub fn relations(&self, r: &RelInfo, refname: &dyn Fn(usize) -> String, cat: &crate::catalog::Catalog) -> String {
        match &r.kind {
            Kind::Base(k) => {
                let t = &self.bases[*k].scan.table;
                let mut s = format!("{}.{}", quote_ident(cat.schema_name(t.schema)), quote_ident(&t.name));
                let rn = refname(*k);
                if !rn.is_empty() && rn != t.name {
                    s.push(' ');
                    s.push_str(&quote_ident(&rn));
                }
                s
            }
            Kind::Join { jt, outer, inner, .. } => {
                let name = match jt {
                    crate::sql::plan::JoinKind::Inner => "INNER",
                    crate::sql::plan::JoinKind::Left => "LEFT",
                    crate::sql::plan::JoinKind::Full => "FULL",
                    crate::sql::plan::JoinKind::Right => "RIGHT",
                    _ => "SEMI",
                };
                format!("({}) {name} JOIN ({})", self.relations(outer, refname, cat), self.relations(inner, refname, cat))
            }
        }
    }
}

impl crate::ext::fdw::ForeignRelOps for JoinScan {
    fn sort_only_cols(&self, fr: &ForeignRel) -> Vec<usize> {
        if self.order.is_empty() {
            return Vec::new();
        }
        self.tlist.iter().copied().filter(|g| !fr.out.contains(g) && self.order.iter().any(|(e, ..)| e.columns().contains(g))).collect()
    }
    fn open(&self, fr: &ForeignRel, ctx: &ExecCtx) -> Result<BoxNode> {
        let (sql, params) = self.sql(ctx);
        let first = &self.bases[0].scan;
        let m = super::scan::mapping_for(&first.table, first.check_as, ctx)?;
        let (key, number) = super::connection::scan_connection(ctx, fr as *const ForeignRel as usize, &m)?;
        // (The row the remote columns go into: one slot per column fetched, then those
        // the node returns that aren't.)
        let mut slots: Vec<usize> = self.tlist.clone();
        for &g in &fr.out {
            if !slots.contains(&g) {
                slots.push(g);
            }
        }
        for &(_, k) in &self.whole {
            for g in self.bases[k].cols.iter().flatten() {
                if !slots.contains(g) {
                    slots.push(*g);
                }
            }
        }
        let slot = |g: usize| slots.iter().position(|&x| x == g).unwrap_or(usize::MAX);
        let mut types: Vec<Field> = self
            .tlist
            .iter()
            .map(|&g| match self.key_of(g) {
                Some(oid) => Field::Key(slot(g), oid),
                None => Field::Col(slot(g), self.col_type(g), self.col_context(g)),
            })
            .collect();
        for (at, k, mark) in self.inserts() {
            let b = &self.bases[k];
            let t = &b.scan.table;
            let live: Vec<usize> = (0..t.columns.len()).filter(|&i| !t.columns[i].dropped).collect();
            let rname = b.scan.alias.clone().unwrap_or_else(|| t.name.clone());
            let fields = live.iter().map(|&i| (t.columns[i].name.clone(), t.columns[i].ty.clone())).collect();
            let what = format!("whole-row reference to foreign table \"{rname}\"");
            let field = if mark {
                Field::Mark { slot: slot(crate::sql::plan::FDW_MARK_COL + k), oid: t.oid, ncols: t.columns.len(), live, fields, what }
            } else {
                Field::Whole { slots: live.iter().map(|&i| b.cols.get(i).copied().flatten().map_or(usize::MAX, slot)).collect(), fields, what }
            };
            types.insert(at.min(types.len()), field);
        }
        Ok(Box::new(JoinNode {
            cur: super::scan::Cursor::new(key, self.info.opts.fetch_size, sql, params).numbered(number),
            types,
            width: slots.len(),
            local: self.info.local_conds.iter().map(|e| e.clone().remap_cols(&slot)).collect(),
            out: fr.out.iter().map(|&g| slot(g)).collect(),
        }))
    }

    fn explain(&self, _fr: &ForeignRel, refname: &dyn Fn(usize) -> String, ctx: &ExecCtx, verbose: bool) -> Vec<(&'static str, String)> {
        let cat = ctx.catalog();
        let mut v = vec![("Relations", self.relations(&self.info, refname, &cat))];
        if verbose {
            v.push(("Remote SQL", self.sql(ctx).0));
        }
        v
    }

    fn filter(&self) -> Vec<Expr> {
        self.info.local_conds.clone()
    }

    fn mark_rows(&self, _fr: &ForeignRel) -> Vec<usize> {
        self.mark_bases()
    }

    fn with_copy_marks(&self, fr: &ForeignRel) -> Option<(Arc<dyn crate::ext::fdw::ForeignRelOps>, Vec<usize>)> {
        // (Not over a sort or LIMIT sent with it, nor relations deparsed as subqueries,
        // whose rows have no `rN` alias here.)
        if !self.order.is_empty() || self.limit.is_some() || !self.marks.is_empty() || self.info.lower_subquery != 0 {
            return None;
        }
        // (Each base's columns as its reltarget has them: those the target list reads, its
        // junk (the whole row), then those only its join clauses read
        // (add_vars_to_targetlist), in their order; with whole-row references fetched, the
        // mark after the base's last.)
        let base_of = |g: usize| self.bases.iter().position(|b| b.cols.contains(&Some(g)));
        let regroup = |list: &[usize]| -> (Vec<usize>, Vec<usize>) {
            if !self.whole.is_empty() {
                let mut lo = 0;
                let at = (0..self.bases.len())
                    .map(|k| {
                        lo = list.iter().rposition(|&g| base_of(g) == Some(k)).map_or(lo, |i| (i + 1).max(lo));
                        lo
                    })
                    .collect();
                return (list.to_vec(), at);
            }
            let mut res = Vec::with_capacity(list.len());
            let mut at = Vec::with_capacity(self.bases.len());
            for (k, b) in self.bases.iter().enumerate() {
                let clause_rank = |g: usize| {
                    let i = b.cols.iter().position(|c| *c == Some(g))?;
                    b.scan.reltarget.0.iter().position(|&x| x == i)
                };
                let mine: Vec<usize> = list.iter().copied().filter(|&g| base_of(g) == Some(k)).collect();
                let read = |g: usize| self.top.contains(&g) || clause_rank(g).is_none();
                res.extend(mine.iter().copied().filter(|&g| read(g)));
                at.push(res.len());
                let mut clause: Vec<usize> = mine.into_iter().filter(|&g| !read(g)).collect();
                clause.sort_by_key(|&g| clause_rank(g));
                res.extend(clause);
            }
            res.extend(list.iter().copied().filter(|&g| base_of(g).is_none()));
            (res, at)
        };
        let (cols, at) = regroup(&fr.out);
        let mut out = Vec::with_capacity(cols.len() + at.len());
        for i in 0..=cols.len() {
            out.extend((0..at.len()).filter(|&k| at[k] == i).map(|k| crate::sql::plan::FDW_MARK_COL + k));
            out.extend(cols.get(i));
        }
        let mut js = self.clone();
        let (tlist, at) = regroup(&self.tlist);
        js.tlist = tlist;
        js.marks = at.into_iter().enumerate().map(|(k, p)| (p, k)).collect();
        Some((Arc::new(js), out))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A remote column of a join's row: one column (its slot, type, conversion error context),
/// or a base relation's whole row, its fields going to their columns' slots.
enum Field {
    Col(usize, DataType, String),
    /// The target's `rN.ctid` as the row's key (`modify::row_key` of table oid).
    Key(usize, u32),
    Whole { slots: Vec<usize>, fields: Vec<(String, DataType)>, what: String },
    /// A base relation's whole row kept as its row mark's copy (`exec::scan::copy_key` of
    /// table `oid`'s `ncols` columns, the `live` ones fetched): NULL for a null-extended row.
    Mark { slot: usize, oid: u32, ncols: usize, live: Vec<usize>, fields: Vec<(String, DataType)>, what: String },
}

struct JoinNode {
    cur: super::scan::Cursor,
    types: Vec<Field>,
    width: usize,
    local: Vec<Expr>,
    out: Vec<usize>,
}

impl Node for JoinNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            let Some(vals) = self.cur.next(ctx)? else { return Ok(None) };
            let mut row = vec![Value::Null; self.width];
            for (f, v) in self.types.iter().zip(vals) {
                let Some(s) = v else { continue };
                match f {
                    Field::Col(g, ty, what) => {
                        row[*g] = crate::types::text::parse_value(&s, ty, ctx.fmt()).map_err(|e| e.context(what.clone()))?;
                    }
                    Field::Key(g, oid) => {
                        row[*g] = Value::Bytea(Arc::from(super::modify::row_key(*oid, &s)));
                    }
                    Field::Mark { slot, oid, ncols, live, fields, what } => {
                        if let Value::Record(vals) = crate::types::text::parse_record(&s, fields, ctx.fmt()).map_err(|e| e.context(what.clone()))? {
                            let mut cols = vec![Value::Null; *ncols];
                            for (&i, v) in live.iter().zip(vals.iter()) {
                                cols[i] = v.clone();
                            }
                            if let Some(x) = row.get_mut(*slot) {
                                *x = Value::Bytea(Arc::from(crate::exec::scan::copy_key(*oid, &cols)));
                            }
                        }
                    }
                    Field::Whole { slots, fields, what } => {
                        let rec = crate::types::text::parse_record(&s, fields, ctx.fmt()).map_err(|e| e.context(what.clone()))?;
                        if let Value::Record(vals) = rec {
                            for (&g, v) in slots.iter().zip(vals.iter()) {
                                if let Some(x) = row.get_mut(g) {
                                    *x = v.clone();
                                }
                            }
                        }
                    }
                }
            }
            let mut ok = true;
            for c in &self.local {
                if !matches!(crate::sql::expr::eval(c, &row, ctx)?, Value::Bool(true)) {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Ok(Some(self.out.iter().map(|&g| row.get(g).cloned().unwrap_or(Value::Null)).collect()));
            }
        }
    }
}
