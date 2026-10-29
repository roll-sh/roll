//! The planner's and executor's hooks for foreign data wrappers that run more than a base
//! relation's scan remotely (FdwRoutine's GetForeignJoinPaths / GetForeignUpperPaths,
//! BeginForeignScan / IterateForeignScan / ExplainForeignScan of a join or upper relation).
//! A wrapper answers for the relations of its tables (`wrapper_of`); what it decided goes into
//! the plan as a `Plan::Foreign` whose private part implements `ForeignRelOps`.

use crate::catalog::{Catalog, Table};
use crate::error::Result;
use crate::exec::{BoxNode, ExecCtx};
use crate::sql::plan::ForeignRel;

/// What a Foreign Scan of a join or upper relation does, as its wrapper decided it.
pub trait ForeignRelOps: Send + Sync + 'static {
    /// Start the scan (BeginForeignScan): the node returns the relation's `out` columns.
    fn open(&self, fr: &ForeignRel, ctx: &ExecCtx) -> Result<BoxNode>;
    /// ExplainForeignScan's lines (label, text): `refname(i)` is the name EXPLAIN gives
    /// the relation of scan `i`.
    fn explain(&self, fr: &ForeignRel, refname: &dyn Fn(usize) -> String, ctx: &ExecCtx, verbose: bool) -> Vec<(&'static str, String)>;
    /// The node's own target list as EXPLAIN shows it, over the relation's row with the
    /// aggregates `Expr::Agg` names (None: its `out` columns).
    fn output(&self, _fr: &ForeignRel) -> Option<(Vec<crate::sql::expr::Expr>, Vec<crate::sql::plan::AggCall>)> {
        None
    }
    /// The conditions checked locally on its rows (EXPLAIN's Filter), over the relation's
    /// row.
    fn filter(&self) -> Vec<crate::sql::expr::Expr> {
        Vec::new()
    }
    /// A grouped relation's GROUP BY entries among its columns (EXPLAIN shows those its
    /// query's target list doesn't name as resjunk columns).
    fn group_cols(&self) -> Vec<usize> {
        Vec::new()
    }
    fn as_any(&self) -> &dyn std::any::Any;
}

/// The wrappers with join and upper-relation pushdown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wrapper {
    PostgresFdw,
}

/// The wrapper of foreign table `t` that pushes joins down, if it has one.
pub fn wrapper_of(cat: &Catalog, t: &Table) -> Option<Wrapper> {
    crate::ext::postgres_fdw::scan::is_postgres_fdw(cat, t).then_some(Wrapper::PostgresFdw)
}

/// Run a Foreign Scan of a join or upper relation.
#[inline(never)]
pub fn open(fr: &ForeignRel, ctx: &ExecCtx) -> Result<BoxNode> {
    fr.private.0.open(fr, ctx)
}
