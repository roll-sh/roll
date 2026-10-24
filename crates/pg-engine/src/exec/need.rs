//! Which columns of a plan node's rows anything above it reads: a node opened with a mask
//! (`ExecCtx::need`) may leave the other columns NULL, and a table scan then skips decoding
//! them (projection pushdown). Nothing observable changes: every expression a node
//! evaluates (its filters, keys, arguments and outputs, whether their values are used or
//! not) still sees its columns' real values, so results and errors are the same.
//!
//! A mask's column `i` is needed when `i < len && mask[i]`; no mask: every column is.
//! Under EXPLAIN ANALYZE nothing is masked (the sizes it reports read whole rows).

use crate::sql::expr::Expr;
use crate::sql::plan::{AggCall, SortKey};

/// Mark the columns of the input row `e` reads. False: `e` holds something whose input
/// columns aren't plain column references (the whole row is needed).
pub fn mark(e: &Expr, out: &mut Vec<bool>) -> bool {
    let mut ok = true;
    e.walk(&mut |x| match x {
        Expr::Col(i, _) => {
            if out.len() <= *i {
                out.resize(*i + 1, false);
            }
            out[*i] = true;
        }
        Expr::Xml(_) | Expr::Srf(..) | Expr::Agg(..) | Expr::Window(..) | Expr::Grouping(_) => {
            ok = false
        }
        _ => {}
    });
    ok
}

/// The columns `exprs` read (None: all of them).
pub fn of<'a>(exprs: impl IntoIterator<Item = &'a Expr>) -> Option<Vec<bool>> {
    add(Some(Vec::new()), exprs)
}

/// `need` and the columns `exprs` read (None: all).
pub fn add<'a>(need: Option<Vec<bool>>, exprs: impl IntoIterator<Item = &'a Expr>) -> Option<Vec<bool>> {
    let mut out = need?;
    for e in exprs {
        if !mark(e, &mut out) {
            return None;
        }
    }
    Some(out)
}

/// The expressions of sort keys.
pub fn keys(keys: &[SortKey]) -> impl Iterator<Item = &Expr> {
    keys.iter().map(|k| &k.expr)
}

/// Every expression an aggregate call evaluates over its input rows. None: one this can't
/// see through (a user-defined aggregate's are evaluated over its state, not the row, but
/// stay conservative).
pub fn agg_exprs(a: &AggCall) -> Option<Vec<&Expr>> {
    if a.user.is_some() {
        return None;
    }
    let mut v: Vec<&Expr> = a.args.iter().collect();
    v.extend(a.filter.iter());
    v.extend(keys(&a.order_by));
    Some(v)
}

/// `need` restricted to columns `from..from + n`, renumbered from 0 (None: all).
pub fn slice(need: Option<&[bool]>, from: usize, n: usize) -> Option<Vec<bool>> {
    let need = need?;
    Some((from..from + n).map(|i| need.get(i).copied().unwrap_or(false)).collect())
}

/// A join's left (outer) input: the join's needed left columns, the left keys and the
/// parameters (over the left row), and the left columns of the join condition (over the
/// left row then the right one). None: all.
pub fn join_left<'a>(
    need: Option<&[bool]>,
    nleft: usize,
    left_exprs: impl IntoIterator<Item = &'a Expr>,
    cond: Option<&Expr>,
) -> Option<Vec<bool>> {
    let mut out = add(slice(need, 0, nleft), left_exprs)?;
    if let Some(c) = cond {
        let mut m = Vec::new();
        if !mark(c, &mut m) {
            return None;
        }
        for (i, _) in m.iter().enumerate().take(nleft).filter(|(_, b)| **b) {
            if out.len() <= i {
                out.resize(i + 1, false);
            }
            out[i] = true;
        }
    }
    Some(out)
}

/// Whether column `i` is needed.
pub fn has(need: Option<&[bool]>, i: usize) -> bool {
    need.is_none_or(|n| n.get(i).copied().unwrap_or(false))
}
