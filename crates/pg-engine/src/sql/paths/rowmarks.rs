//! SELECT ... FOR UPDATE / SHARE: the rows a query level returns are locked once, at the
//! level's top (grouping_planner's LockRows, under its LIMIT), after its joins and quals,
//! not as its scans read them. Every table of the level gets a row mark
//! (preprocess_rowmarks): the locked ones take their lock, the others are ROW_MARK_REFERENCE,
//! re-read only to recheck a row a concurrent update changed (EvalPlanQual). Each scan of the
//! level returns its row's storage key (its ctid) up the plan for it.

use crate::catalog::Table;
use crate::sql::expr::Expr;
use crate::sql::plan::{JoinKind, LockMark, Plan, RowMark, ScanPlan, SortKey, Wait};
use crate::types::DataType;
use std::sync::Arc;

/// One row mark while the keys go up.
struct Key {
    /// The key's position in the row.
    col: usize,
    /// The tables it covers (an inheritance tree's members).
    tables: Vec<Arc<Table>>,
    lock: Option<(RowMark, Wait)>,
    /// The relation's place in the range table (its first member's).
    rti: usize,
}

/// A plan with its relations' keys added: the plan, its old output positions' new ones, and
/// the keys (in the level's order: left to right).
type Keyed = (Plan, Vec<usize>, Vec<Key>);

thread_local! {
    /// Planning a query level whose only lock is one pushed down from the level above
    /// (no hasForUpdate): its row marks are that level's (pulled up) or locked when that
    /// level plans it as a subquery apart, not here.
    pub(crate) static PUSHED_LEVEL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The level's plan with a LockRows at its top (below its LIMITs) when its scans have row
/// marks and the plan's shape lets the keys go up; else as it is (each locked scan then
/// locks the rows it returns).
pub(super) fn hoist(plan: &mut Plan) {
    if PUSHED_LEVEL.get() {
        return;
    }
    // (A subquery planned apart, locked by its own clause or one pushed into it, is a level
    // of its own: its LockRows at its top, under the Subquery Scan.)
    // (Only down to such a subquery: the nodes on the way are made the plan's own.)
    fn wants(p: &Plan) -> bool {
        match p {
            Plan::Barrier { input, .. } => has_marks(input) || wants(input),
            Plan::LockRows { .. } => false,
            _ => crate::sql::explain::plan_inputs(p).into_iter().any(|c| wants(c)),
        }
    }
    fn apart(p: &mut Plan) {
        match p {
            Plan::Barrier { input, .. } => hoist(Arc::make_mut(input)),
            _ => {
                for c in crate::sql::explain::plan_inputs_mut(p) {
                    if wants(&**c) {
                        apart(Arc::make_mut(c));
                    }
                }
            }
        }
    }
    if wants(plan) {
        apart(plan);
    }
    hoist_level(plan);
}

fn hoist_level(plan: &mut Plan) {
    if let Plan::Limit { input, .. } = plan {
        return hoist_level(Arc::make_mut(input));
    }
    // (The projection dropping the resjunk columns the level carries, its sort keys, isn't
    // a plan node: the LockRows below it returns them too.)
    if let Plan::Project { input, exprs } = plan
        && exprs.len() < crate::sql::binder::plan_width(input)
        && exprs.iter().enumerate().all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i))
    {
        return hoist_level(Arc::make_mut(input));
    }
    if !has_marks(plan) {
        return;
    }
    let Some((keyed, map, keys)) = key(plan) else {
        return;
    };
    if keys.iter().all(|k| k.lock.is_none()) {
        return;
    }
    let w = crate::sql::binder::plan_width(plan);
    // The row marks' columns as preprocess_rowmarks orders them (the target list's resjunk
    // ctids): the locked relations', then the others', each in range table order.
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by_key(|&j| (keys[j].lock.is_none(), keys[j].rti));
    // (The level's columns, then the keys in that order.)
    let out: Vec<usize> = (0..w)
        .map(|i| map.get(i).copied().unwrap_or(i))
        .chain(order.iter().map(|&j| keys[j].col))
        .collect();
    let n = crate::sql::binder::plan_width(&keyed);
    let input = if n == out.len() && out.iter().enumerate().all(|(i, c)| *c == i) {
        keyed
    } else if let Plan::Project { input, exprs } = keyed {
        // (The projection computing the level's target list computes them so.)
        Plan::Project {
            input,
            exprs: out.iter().map(|&c| exprs[c].clone()).collect(),
        }
    } else {
        let types = super::analyzejoins::col_types(&keyed);
        Plan::Project {
            input: Arc::new(keyed),
            exprs: out
                .iter()
                .map(|&c| Expr::Col(c, types.get(c).cloned().unwrap_or(DataType::Unknown)))
                .collect(),
        }
    };
    let marks = keys
        .into_iter()
        .enumerate()
        .map(|(j, k)| LockMark {
            col: w + order.iter().position(|&o| o == j).unwrap_or(j),
            tables: k.tables,
            lock: k.lock,
        })
        .collect();
    *plan = Plan::LockRows {
        input: Arc::new(input),
        marks: Arc::new(marks),
        width: w,
    };
}

/// Whether a scan of this query level (not of a subquery planned apart) has a row mark.
fn has_marks(p: &Plan) -> bool {
    match p {
        Plan::Scan(s) => s.mark.is_some(),
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Context { input, .. }
        | Plan::With { body: input, .. } => has_marks(input),
        Plan::Join { left, right, .. } => has_marks(left) || has_marks(right),
        Plan::Append { inputs, inh: Some(_), .. } => inputs.iter().any(|i| has_marks(i)),
        _ => false,
    }
}

/// The level's table scans whose keys a LockRows reads, grouped by row mark (an inheritance
/// tree's members together), in the order `hoist` put their keys: what EvalPlanQual
/// substitutes its test rows into.
pub fn level_scans(p: &Plan) -> Vec<Vec<&ScanPlan>> {
    fn go<'p>(p: &'p Plan, out: &mut Vec<Vec<&'p ScanPlan>>) {
        match p {
            Plan::Scan(s) => {
                if s.with_key && !s.sys {
                    out.push(vec![s]);
                }
            }
            Plan::Filter { input, .. }
            | Plan::Project { input, .. }
            | Plan::ProjectSet { input, .. }
            | Plan::Sort { input, .. }
            | Plan::Limit { input, .. }
            | Plan::Context { input, .. }
            | Plan::With { body: input, .. } => go(input, out),
            Plan::Join {
                kind, left, right, ..
            } => {
                go(left, out);
                if !matches!(kind, JoinKind::Semi | JoinKind::Anti) {
                    go(right, out);
                }
            }
            Plan::Append { inputs, inh: Some(_), .. } => {
                let mut members = Vec::new();
                for i in inputs {
                    let mut one = Vec::new();
                    go(i, &mut one);
                    members.extend(one.into_iter().flatten());
                }
                if !members.is_empty() {
                    out.push(members);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    go(p, &mut out);
    out
}

fn ident(n: usize) -> Vec<usize> {
    (0..n).collect()
}

fn remap(e: &Expr, map: &[usize]) -> Expr {
    e.clone().remap_cols(&|c| map.get(c).copied().unwrap_or(c))
}

fn remap_keys(keys: &[SortKey], map: &[usize]) -> Vec<SortKey> {
    keys.iter()
        .map(|k| SortKey {
            expr: remap(&k.expr, map),
            ..k.clone()
        })
        .collect()
}

/// `p` with every table scan of the level returning its row's key: None when some node's
/// shape isn't one the keys can go up through.
fn key(p: &Plan) -> Option<Keyed> {
    let w = crate::sql::binder::plan_width(p);
    Some(match p {
        Plan::Scan(s) => {
            if s.with_key || s.sys || s.current_of.is_some() || s.limit.is_some() {
                return None;
            }
            let mut s = s.clone();
            s.with_key = true;
            let lock = s.mark.take();
            let n = s.table.columns.len();
            let key = Key {
                col: n,
                tables: vec![s.table.clone()],
                lock,
                rti: s.rti.0,
            };
            (Plan::Scan(s), ident(n), vec![key])
        }
        // Sources of no table's rows (a subquery planned apart, a function, VALUES, a CTE):
        // nothing to lock or re-read.
        Plan::One
        | Plan::Empty { .. }
        | Plan::Values { .. }
        | Plan::FunctionScan { .. }
        | Plan::PlCall { .. }
        | Plan::SysScan { .. }
        | Plan::Barrier { .. }
        | Plan::CteScan { .. }
        | Plan::WorkScan { .. }
        | Plan::EnrScan { .. } => (p.clone(), ident(w), Vec::new()),
        Plan::Filter { input, pred } => {
            let (i, map, keys) = key(input)?;
            let pred = remap(pred, &map);
            (Plan::Filter { input: Arc::new(i), pred }, map, keys)
        }
        Plan::Context {
            input,
            frame,
            perms,
            volatile,
        } => {
            let (i, map, keys) = key(input)?;
            (
                Plan::Context {
                    input: Arc::new(i),
                    frame: frame.clone(),
                    perms: perms.clone(),
                    volatile: *volatile,
                },
                map,
                keys,
            )
        }
        Plan::Project { input, exprs } => {
            let (i, map, keys) = key(input)?;
            let mut out: Vec<Expr> = exprs.iter().map(|e| remap(e, &map)).collect();
            let n = out.len();
            let mut nkeys = Vec::with_capacity(keys.len());
            for k in keys {
                out.push(Expr::Col(k.col, DataType::Bytea));
                nkeys.push(Key { col: out.len() - 1, ..k });
            }
            (
                Plan::Project {
                    input: Arc::new(i),
                    exprs: out,
                },
                ident(n),
                nkeys,
            )
        }
        Plan::ProjectSet { input, exprs, srfs } => {
            let (i, map, keys) = key(input)?;
            let mut out: Vec<Expr> = exprs.iter().map(|e| remap(e, &map)).collect();
            let srfs = srfs
                .iter()
                .map(|c| {
                    let mut c = c.clone();
                    c.args = c.args.iter().map(|a| remap(a, &map)).collect();
                    c
                })
                .collect();
            let n = out.len();
            let mut nkeys = Vec::with_capacity(keys.len());
            for k in keys {
                out.push(Expr::Col(k.col, DataType::Bytea));
                nkeys.push(Key { col: out.len() - 1, ..k });
            }
            (
                Plan::ProjectSet {
                    input: Arc::new(i),
                    exprs: out,
                    srfs,
                },
                ident(n),
                nkeys,
            )
        }
        Plan::Sort { input, keys: sk } => {
            let (i, map, keys) = key(input)?;
            (
                Plan::Sort {
                    input: Arc::new(i),
                    keys: remap_keys(sk, &map),
                },
                map,
                keys,
            )
        }
        Plan::Limit {
            input,
            limit,
            offset,
            with_ties,
        } => {
            let (i, map, keys) = key(input)?;
            (
                Plan::Limit {
                    input: Arc::new(i),
                    limit: limit.clone(),
                    offset: offset.clone(),
                    with_ties: with_ties.as_ref().map(|k| remap_keys(k, &map)),
                },
                map,
                keys,
            )
        }
        Plan::With { ctes, body, names } => {
            let (b, map, keys) = key(body)?;
            (
                Plan::With {
                    ctes: ctes.clone(),
                    body: Arc::new(b),
                    names: names.clone(),
                },
                map,
                keys,
            )
        }
        // An inheritance tree's scans: one row mark, each member keyed alike (a key names
        // its own table).
        Plan::Append {
            inputs,
            inh: inh @ Some(_),
            ..
        } => {
            let mut out = Vec::with_capacity(inputs.len());
            let mut first: Option<(Vec<usize>, Key)> = None;
            for i in inputs {
                let (p, map, mut keys) = key(i)?;
                if keys.len() != 1 {
                    return None;
                }
                let k = keys.pop().unwrap();
                match &mut first {
                    Some((m, f)) if *m != map || f.col != k.col || f.lock != k.lock => return None,
                    Some((_, f)) => {
                        f.tables.extend(k.tables);
                        f.rti = f.rti.min(k.rti);
                    }
                    None => first = Some((map, k)),
                }
                out.push(Arc::new(p));
            }
            let (map, k) = first?;
            (
                Plan::Append {
                    inputs: out,
                    inh: inh.clone(),
                    alias: None,
                    pw: None,
                },
                map,
                vec![k],
            )
        }
        Plan::Join {
            kind,
            left,
            right,
            cond,
            hash,
            params,
            nleft,
            nright,
            meta,
        } => {
            let semi = matches!(kind, JoinKind::Semi | JoinKind::Anti);
            let (l, lmap, lkeys) = key(left)?;
            // (A semi / anti join returns no row of its inner side.)
            let (r, rmap, rkeys) = if semi {
                let n = crate::sql::binder::plan_width(right);
                ((**right).clone(), ident(n), Vec::new())
            } else {
                key(right)?
            };
            let wl = crate::sql::binder::plan_width(&l);
            let wr = crate::sql::binder::plan_width(&r);
            let (nl, nr) = (*nleft, *nright);
            let comb: Vec<usize> = (0..nl + nr)
                .map(|c| {
                    if c < nl {
                        lmap.get(c).copied().unwrap_or(c)
                    } else {
                        wl + rmap.get(c - nl).copied().unwrap_or(c - nl)
                    }
                })
                .collect();
            let cond = cond.as_ref().map(|e| remap(e, &comb));
            let hash = hash.as_ref().map(|(a, b)| {
                (
                    a.iter().map(|e| remap(e, &lmap)).collect(),
                    b.iter().map(|e| remap(e, &rmap)).collect(),
                )
            });
            let params = params.iter().map(|e| remap(e, &lmap)).collect();
            let mut meta = meta.clone();
            meta.kept = meta.kept.iter().map(|c| comb.get(*c).copied().unwrap_or(*c)).collect();
            meta.pushed = meta.pushed.iter().map(|c| comb.get(*c).copied().unwrap_or(*c)).collect();
            if let Some(m) = &mut meta.memoize {
                m.keys = m.keys.iter().map(|e| remap(e, &lmap)).collect();
            }
            let map: Vec<usize> = comb[..nl + if semi { 0 } else { nr }].to_vec();
            let mut keys = lkeys;
            keys.extend(rkeys.into_iter().map(|k| Key { col: wl + k.col, ..k }));
            (
                Plan::Join {
                    kind: *kind,
                    left: Arc::new(l),
                    right: Arc::new(r),
                    cond,
                    hash,
                    params,
                    nleft: wl,
                    nright: wr,
                    meta,
                },
                map,
                keys,
            )
        }
        _ => return None,
    })
}
