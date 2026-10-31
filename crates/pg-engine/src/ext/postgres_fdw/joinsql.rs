//! The remote query of a foreign join (deparseSelectStmtForRel, deparseFromExprForRel,
//! deparseRangeTblRef): `SELECT ... FROM (r1 INNER JOIN r2 ON (...)) WHERE ...`, a FULL
//! JOIN's sides with conditions as subqueries (`sN(c1, ...)`), semi joins as EXISTS.

use super::deparse::{Deparser, RelRef, relation_name};
use super::join::{Base, Kind, RelInfo};
use crate::catalog::{Catalog, Table};
use crate::sql::expr::Expr;
use crate::sql::plan::JoinKind;

/// Deparsing one remote query over a set of base relations.
pub struct Sql<'a> {
    pub cat: &'a Catalog,
    pub fmt: &'a crate::types::text::FormatCtx,
    pub bases: &'a [Base],
    /// The remote query's parameters, in `$n` order.
    pub params: std::cell::RefCell<Vec<Expr>>,
    /// Parameters as placeholders (a remote EXPLAIN's query).
    pub placeholders: bool,
    /// An upper relation's aggregates (`Expr::Agg(i)`).
    pub aggs: &'a [crate::sql::plan::AggCall],
}

/// What the SELECT of a relation adds to its FROM and WHERE.
#[derive(Default)]
pub struct Tail {
    /// An upper relation's GROUP BY (positions in its SELECT list) and HAVING.
    pub group_by: Vec<usize>,
    pub having: Vec<Expr>,
    /// ORDER BY keys (expression, DESC, NULLS FIRST).
    pub order: Vec<(Expr, bool, bool)>,
    pub limit: Option<(Option<Expr>, Option<Expr>)>,
    /// FOR UPDATE / FOR SHARE (with `OF rN` for joins).
    pub lock: String,
    /// The row-marked relations' whole rows (ROW_MARK_COPY's junk columns), after the
    /// select list.
    pub rows: Vec<String>,
}

fn jointype_name(jt: JoinKind) -> &'static str {
    match jt {
        JoinKind::Inner => "INNER",
        JoinKind::Left => "LEFT",
        JoinKind::Right => "RIGHT",
        JoinKind::Full => "FULL",
        _ => "SEMI",
    }
}

impl<'a> Sql<'a> {
    pub fn new(cat: &'a Catalog, fmt: &'a crate::types::text::FormatCtx, bases: &'a [Base]) -> Sql<'a> {
        Sql { cat, fmt, bases, params: Default::default(), placeholders: false, aggs: &[] }
    }

    /// A deparser of expressions (over the join row) in `scope`: its relations' columns,
    /// qualified when it has more than one, a subquery's columns as its aliases
    /// (is_subquery_var).
    fn deparser(&self, scope: &RelInfo) -> Deparser<'a> {
        let first = (0..self.bases.len()).find(|k| scope.relids & (1u64 << k) != 0).unwrap_or(0);
        let table: &'a Table = &self.bases[first].scan.table;
        let mut d = Deparser::new(self.cat, table, self.fmt);
        let multi = scope.relids.count_ones() > 1;
        for (k, b) in self.bases.iter().enumerate() {
            if scope.relids & (1u64 << k) != 0 {
                d.rels.push(RelRef {
                    cols: &b.cols,
                    table: &b.scan.table,
                    alias: format!("r{}", b.scan.rtindex.0),
                });
            }
        }
        if multi {
            subquery_vars(scope, &mut d.subq);
        }
        d.placeholders = self.placeholders;
        d.aggs = self.aggs;
        *d.params.borrow_mut() = self.params.take();
        d
    }

    /// `e` over the join row, as a single relation's deparser reads it (its scan row).
    fn local(&self, scope: &RelInfo, e: &Expr) -> Expr {
        if scope.relids.count_ones() > 1 {
            return e.clone();
        }
        let Kind::Base(k) = scope.kind else { return e.clone() };
        let b = &self.bases[k];
        e.clone().remap_cols(&|g| b.col(g).unwrap_or(usize::MAX))
    }

    fn with<T>(&self, scope: &RelInfo, f: impl FnOnce(&Deparser<'a>) -> T) -> T {
        let mut d = self.deparser(scope);
        if scope.relids.count_ones() <= 1 {
            d.rels.clear();
        }
        let r = f(&d);
        self.params.replace(d.params.take());
        r
    }

    /// deparseExpr of `e` in `scope`.
    pub fn expr(&self, scope: &RelInfo, e: &Expr) -> String {
        let e = self.local(scope, e);
        self.with(scope, |d| d.expr(&e))
    }

    /// appendConditions.
    pub fn conditions(&self, scope: &RelInfo, conds: &[Expr]) -> String {
        let parts: Vec<Expr> = conds.iter().map(|c| self.local(scope, c)).collect();
        self.with(scope, |d| d.conditions(&parts))
    }

    /// Whether `e` can run remotely in `scope` (is_foreign_expr).
    pub fn shippable(&self, scope: &RelInfo, e: &Expr) -> bool {
        let e = self.local(scope, e);
        self.with(scope, |d| d.shippable(&e))
    }

    /// deparseSelectStmtForRel of a join (or base) relation: `tlist` the columns fetched
    /// (None: its reltarget, as a subquery's), `conds` its WHERE.
    pub fn select(&self, rel: &RelInfo, tlist: Option<&[Expr]>, conds: &[Expr], tail: &Tail) -> String {
        let list: Vec<String> = match tlist {
            Some(t) => t.iter().map(|e| self.expr(rel, e)).collect(),
            None => rel.reltarget.iter().map(|&g| self.expr(rel, &Expr::Col(g, crate::types::DataType::Unknown))).collect(),
        };
        let list: Vec<String> = list.into_iter().chain(tail.rows.iter().cloned()).collect();
        let mut sql = format!("SELECT {}", if list.is_empty() { "NULL".to_string() } else { list.join(", ") });
        let mut additional = Vec::new();
        sql.push_str(" FROM ");
        sql.push_str(&self.rel_from_sql(rel, rel.relids.count_ones() > 1, &mut additional));
        self.where_clause(&mut sql, rel, conds, &additional);
        if !tail.group_by.is_empty() {
            sql.push_str(" GROUP BY ");
            sql.push_str(&tail.group_by.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "));
        }
        if !tail.having.is_empty() {
            sql.push_str(" HAVING ");
            sql.push_str(&self.conditions(rel, &tail.having));
        }
        if !tail.order.is_empty() {
            let keys: Vec<String> = tail
                .order
                .iter()
                .filter(|(e, _, _)| !matches!(e, Expr::Const(..)))
                .map(|(e, desc, nf)| {
                    format!("{} {} NULLS {}", self.expr(rel, e), if *desc { "DESC" } else { "ASC" }, if *nf { "FIRST" } else { "LAST" })
                })
                .collect();
            if !keys.is_empty() {
                sql.push_str(" ORDER BY ");
                sql.push_str(&keys.join(", "));
            }
        }
        if let Some((l, o)) = &tail.limit {
            if let Some(l) = l {
                sql.push_str(&format!(" LIMIT {}", self.expr(rel, l)));
            }
            if let Some(o) = o {
                sql.push_str(&format!(" OFFSET {}", self.expr(rel, o)));
            }
        }
        sql.push_str(&tail.lock);
        sql
    }

    /// appendWhereClause: the conditions, then the semi joins' EXISTS.
    fn where_clause(&self, sql: &mut String, scope: &RelInfo, conds: &[Expr], additional: &[String]) {
        if conds.is_empty() && additional.is_empty() {
            return;
        }
        sql.push_str(" WHERE ");
        let mut parts = Vec::new();
        if !conds.is_empty() {
            parts.push(self.conditions(scope, conds));
        }
        parts.extend(additional.iter().cloned());
        sql.push_str(&parts.join(" AND "));
    }

    /// deparseFromExprForRel.
    fn rel_from_sql(&self, rel: &RelInfo, use_alias: bool, additional: &mut Vec<String>) -> String {
        match &rel.kind {
            Kind::Base(k) => {
                let b = &self.bases[*k];
                let mut s = relation_name(self.cat, &b.scan.table);
                if use_alias {
                    s.push_str(&format!(" r{}", b.scan.rtindex.0));
                }
                s
            }
            Kind::Join { jt, outer, inner, joinclauses, make_outer_subquery, make_inner_subquery } => {
                let mut add_o = Vec::new();
                let mut add_i = Vec::new();
                let o = self.range_tbl_ref(outer, *make_outer_subquery, &mut add_o);
                let i = self.range_tbl_ref(inner, *make_inner_subquery, &mut add_i);
                let out = if *jt == JoinKind::Semi {
                    let mut e = format!("EXISTS (SELECT NULL FROM {i}");
                    self.where_clause(&mut e, rel, joinclauses, &add_i);
                    add_i.clear();
                    e.push(')');
                    additional.push(e);
                    o
                } else {
                    let on = if joinclauses.is_empty() { "(TRUE)".to_string() } else { format!("({})", self.conditions(rel, joinclauses)) };
                    format!("({o} {} JOIN {i} ON {on})", jointype_name(*jt))
                };
                additional.extend(add_o);
                additional.extend(add_i);
                out
            }
        }
    }

    /// deparseRangeTblRef: a relation of a join, as a subquery when it must be one.
    fn range_tbl_ref(&self, rel: &RelInfo, make_subquery: bool, additional: &mut Vec<String>) -> String {
        if !make_subquery {
            return self.rel_from_sql(rel, true, additional);
        }
        let sub = self.select(rel, None, &rel.remote_conds, &Tail::default());
        let mut s = format!("({sub}) s{}", rel.relation_index);
        if !rel.reltarget.is_empty() {
            let cols: Vec<String> = (1..=rel.reltarget.len()).map(|i| format!("c{i}")).collect();
            s.push_str(&format!("({})", cols.join(", ")));
        }
        s
    }
}

/// is_subquery_var: the columns of `scope` that a subquery below returns, as `sN.cM`.
fn subquery_vars(scope: &RelInfo, out: &mut std::collections::HashMap<usize, String>) {
    let Kind::Join { outer, inner, make_outer_subquery, make_inner_subquery, .. } = &scope.kind else { return };
    for (side, sub) in [(outer, *make_outer_subquery), (inner, *make_inner_subquery)] {
        if sub {
            for (k, &g) in side.reltarget.iter().enumerate() {
                out.insert(g, format!("s{}.c{}", side.relation_index, k + 1));
            }
        } else {
            subquery_vars(side, out);
        }
    }
}
