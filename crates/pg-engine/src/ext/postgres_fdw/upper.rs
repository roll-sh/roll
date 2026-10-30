//! Foreign Scans of upper relations (postgresGetForeignUpperPaths' UPPERREL_GROUP_AGG,
//! foreign_grouping_ok): a query level's aggregation over a foreign scan or join run
//! remotely, `SELECT <grouped target list> FROM ... GROUP BY <positions> HAVING ...`.

use super::joinscan::JoinScan;
use super::joinsql::{Sql, Tail};
use crate::error::Result;
use crate::exec::{BoxNode, ExecCtx, Node, Row};
use crate::sql::expr::Expr;
use crate::sql::plan::{AggCall, ForeignRel};
use crate::types::Value;

/// postgres_fdw's part of a `Plan::Foreign` of an upper relation.
#[derive(Clone)]
pub struct UpperScan {
    /// The scan or join relation aggregated (its description and base relations).
    pub input: JoinScan,
    /// The aggregates, their arguments over the join row (`Expr::Agg(i)`).
    pub aggs: Vec<AggCall>,
    /// The grouped target list (fpinfo->grouped_tlist), over the join row.
    pub tlist: Vec<Expr>,
    /// The GROUP BY's entries of `tlist` (1-based).
    pub group_by: Vec<usize>,
    /// HAVING clauses sent, over the join row.
    pub having: Vec<Expr>,
    /// HAVING clauses checked here, over the node's row (`tlist`'s values).
    pub local: Vec<Expr>,
    /// ORDER BY / LIMIT sent with it, over the join row.
    pub order: Vec<(Expr, bool, bool)>,
    pub limit: Option<(Option<Expr>, Option<Expr>)>,
}

impl UpperScan {
    pub fn sql(&self, ctx: &ExecCtx) -> (String, Vec<Expr>) {
        let cat = ctx.catalog();
        let mut q = Sql::new(&cat, ctx.fmt(), &self.input.bases);
        q.aggs = &self.aggs;
        let tail = Tail {
            group_by: self.group_by.clone(),
            having: self.having.clone(),
            order: self.order.clone(),
            limit: self.limit.clone(),
            ..Default::default()
        };
        let info = &self.input.info;
        let sql = q.select(info, Some(&self.tlist), &info.remote_conds, &tail);
        (sql, q.params.take())
    }
}

impl crate::ext::fdw::ForeignRelOps for UpperScan {
    fn open(&self, _fr: &ForeignRel, ctx: &ExecCtx) -> Result<BoxNode> {
        let (sql, params) = self.sql(ctx);
        let first = &self.input.bases[0].scan;
        let m = super::scan::mapping_for(&first.table, first.check_as, ctx)?;
        let key = super::connection::get_connection(ctx, &m, false)?;
        Ok(Box::new(UpperNode {
            cur: super::scan::Cursor::new(key, self.input.info.opts.fetch_size, sql, params),
            types: self.tlist.iter().map(|e| e.ty()).collect(),
            // (conversion_error_callback: a plain column names itself, anything else its
            // place in the remote select list.)
            contexts: self
                .tlist
                .iter()
                .enumerate()
                .map(|(i, e)| match e {
                    Expr::Col(g, _) => self.input.col_context(*g),
                    _ => format!("processing expression at position {} in select list", i + 1),
                })
                .collect(),
            local: self.local.clone(),
        }))
    }

    fn explain(&self, _fr: &ForeignRel, refname: &dyn Fn(usize) -> String, ctx: &ExecCtx, verbose: bool) -> Vec<(&'static str, String)> {
        let cat = ctx.catalog();
        let rels = self.input.relations(&self.input.info, refname, &cat);
        let mut v = vec![("Relations", format!("Aggregate on ({rels})"))];
        if verbose {
            v.push(("Remote SQL", self.sql(ctx).0));
        }
        v
    }

    fn output(&self, _fr: &ForeignRel) -> Option<(Vec<Expr>, Vec<AggCall>)> {
        Some((self.tlist.clone(), self.aggs.clone()))
    }

    fn group_cols(&self) -> Vec<usize> {
        self.group_by.iter().map(|p| p - 1).collect()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct UpperNode {
    cur: super::scan::Cursor,
    types: Vec<crate::types::DataType>,
    contexts: Vec<String>,
    local: Vec<Expr>,
}

impl Node for UpperNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            let Some(vals) = self.cur.next(ctx)? else { return Ok(None) };
            let mut row = Vec::with_capacity(self.types.len());
            for ((ty, what), v) in self.types.iter().zip(&self.contexts).zip(vals) {
                row.push(match v {
                    Some(s) => crate::types::text::parse_value(&s, ty, ctx.fmt()).map_err(|e| e.context(what.clone()))?,
                    None => Value::Null,
                });
            }
            let mut ok = true;
            for c in &self.local {
                if !matches!(crate::sql::expr::eval(c, &row, ctx)?, Value::Bool(true)) {
                    ok = false;
                    break;
                }
            }
            if ok {
                return Ok(Some(row));
            }
        }
    }
}
