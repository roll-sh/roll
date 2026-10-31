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
        let tail = Tail { order: self.order.clone(), limit: self.limit.clone(), lock, rows, ..Default::default() };
        let sql = q.select(&self.info, Some(&tlist), &self.info.remote_conds, &tail);
        (sql, q.params.take())
    }

    /// The base relations whose whole rows the remote query returns (a locking level's
    /// ROW_MARK_COPY marks, every relation's): the locked ones in their clauses' order,
    /// then the others in range table order (preprocess_rowmarks).
    pub fn mark_bases(&self) -> Vec<usize> {
        if !self.bases.iter().any(|b| b.scan.mark.is_some()) {
            return Vec::new();
        }
        let mut order: Vec<usize> = (0..self.bases.len()).collect();
        order.sort_by_key(|&k| {
            let s = &self.bases[k].scan;
            (s.mark.is_none(), s.mark.map(|_| s.lock_order), s.rti.0)
        });
        order
    }

    /// deparseLockingClause (`FOR UPDATE OF rN` per locked relation, in range table order,
    /// not those of a lower subquery) and the marked relations' whole rows
    /// (`CASE WHEN (rN.*)::text IS NOT NULL THEN ROW(...) END`).
    fn row_marks(&self) -> (String, Vec<String>) {
        let marked = self.mark_bases();
        if marked.is_empty() {
            return Default::default();
        }
        let alias = |k: usize| format!("r{}", self.bases[k].scan.rtindex.0);
        let rows = marked
            .iter()
            .map(|&k| {
                let t = &self.bases[k].scan.table;
                let cols: Vec<String> = (0..t.columns.len())
                    .filter(|&i| !t.columns[i].dropped)
                    .map(|i| format!("{}.{}", alias(k), quote_ident(&super::deparse::column_name(t, i))))
                    .collect();
                let list = if cols.is_empty() { "NULL".to_string() } else { cols.join(", ") };
                format!("CASE WHEN ({}.*)::text IS NOT NULL THEN ROW({list}) END", alias(k))
            })
            .collect();
        let mut by_rti: Vec<usize> = (0..self.bases.len()).filter(|k| self.info.lower_subquery >> k & 1 == 0).collect();
        by_rti.sort_by_key(|&k| self.bases[k].scan.rtindex.0);
        let mut lock = String::new();
        for k in by_rti {
            let Some((m, _)) = self.bases[k].scan.mark else { continue };
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
        let key = super::connection::get_connection(ctx, &m, false)?;
        // (The row the remote columns go into: one slot per column fetched, then those
        // the node returns that aren't.)
        let mut slots: Vec<usize> = self.tlist.clone();
        for &g in &fr.out {
            if !slots.contains(&g) {
                slots.push(g);
            }
        }
        let slot = |g: usize| slots.iter().position(|&x| x == g).unwrap_or(usize::MAX);
        Ok(Box::new(JoinNode {
            cur: super::scan::Cursor::new(key, self.info.opts.fetch_size, sql, params),
            types: self.tlist.iter().map(|&g| (slot(g), self.col_type(g), self.col_context(g))).collect(),
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

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct JoinNode {
    cur: super::scan::Cursor,
    types: Vec<(usize, DataType, String)>,
    width: usize,
    local: Vec<Expr>,
    out: Vec<usize>,
}

impl Node for JoinNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            let Some(vals) = self.cur.next(ctx)? else { return Ok(None) };
            let mut row = vec![Value::Null; self.width];
            for ((g, ty, what), v) in self.types.iter().zip(vals) {
                if let Some(s) = v {
                    row[*g] = crate::types::text::parse_value(&s, ty, ctx.fmt()).map_err(|e| e.context(what.clone()))?;
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
