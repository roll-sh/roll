//! Plan nodes other than scans, aggregation, windows and DML.

use super::{BoxNode, ExecCtx, Node, Row, collect, eval_all};
use crate::error::{Error, Result, code};
use crate::sql::expr::{Expr, eval, eval_bool};
use crate::sql::func::{FnCtx, SrfIter, call2};
use crate::sql::plan::{JoinKind, Plan, SetOpKind, SortKey, SrfCall};
use crate::types::Value;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// Start running `plan`.
pub fn open(plan: &Arc<Plan>, ctx: &ExecCtx) -> Result<BoxNode> {
    crate::stack::check()?;
    let node = open_node(plan, ctx)?;
    Ok(match &*ctx.instr.borrow() {
        Some(i) => super::InstrNode::wrap(node, Arc::as_ptr(plan) as usize, i.clone()),
        None => node,
    })
}

fn open_node(plan: &Arc<Plan>, ctx: &ExecCtx) -> Result<BoxNode> {
    Ok(match &**plan {
        Plan::One => Box::new(VecNode::new(vec![Vec::new()])),
        Plan::Empty { .. } => Box::new(VecNode::new(Vec::new())),
        Plan::Values { rows, .. } => Box::new(ValuesNode {
            rows: rows.clone(),
            i: 0,
        }),
        Plan::Scan(s) => super::scan::open_scan(s, ctx)?,
        Plan::SysScan { table, filter, path, .. } => {
            let rows = crate::catalog::system::rows(*table, ctx)?;
            let mut deferred = crate::catalog::system::deferred_errors(*table, ctx, &rows);
            // An index path a sort relies on: the rows in that index's order.
            let rows = match path.as_deref() {
                Some(sp) if sp.ordered => {
                    let (rows, perm) = crate::sql::paths::sys_index_order(rows, &sp.scan, ctx)?;
                    for d in deferred.iter_mut() {
                        if let Some(at) = perm.iter().position(|&o| o == d.0) {
                            d.0 = at;
                        }
                    }
                    rows
                }
                _ => rows,
            };
            if !deferred.is_empty() {
                let read = ctx.sys_cols.borrow().get(&table.oid()).cloned().unwrap_or_default();
                let mut in_filter = std::collections::BTreeSet::new();
                if let Some(f) = filter {
                    f.walk(&mut |e| {
                        if let Expr::Col(i, _) = e {
                            in_filter.insert(*i);
                        }
                    });
                }
                return Ok(Box::new(SysScanNode {
                    rows: rows.into_iter().enumerate(),
                    deferred,
                    read,
                    in_filter,
                    filter: filter.clone(),
                }));
            }
            let node: BoxNode = Box::new(VecNode::new(rows));
            match filter {
                Some(f) => Box::new(FilterNode {
                    input: node,
                    pred: f.clone(),
                }),
                None => node,
            }
        }
        Plan::FunctionScan {
            calls, ordinality, ..
        } => Box::new(FunctionScanNode::new(calls, *ordinality, ctx)?),
        Plan::PlCall {
            func,
            args,
            ncols,
            coll,
        } => {
            let vals = eval_all(args, &[], ctx)?;
            let rows = if func.strict && vals.iter().any(Value::is_null) {
                // STRICT: NULL in → NULL out, without running the body.
                if func.returns_set {
                    Vec::new()
                } else {
                    vec![vec![Value::Null; *ncols]]
                }
            } else {
                crate::plpgsql::call_function_coll(ctx, func, vals, coll.as_deref())?
            };
            Box::new(VecNode::new(rows))
        }
        Plan::Barrier { input, .. } => return open(input, ctx),
        Plan::Context {
            input,
            frame,
            perms,
            volatile,
        } => {
            // A volatile SQL function's statement runs as a command of its own.
            let snap = volatile.then(|| ctx.fresh_snapshot());
            let run = || {
                ctx.sess.framed(frame.clone(), |_| {
                    if let Some(p) = perms {
                        crate::sql::binder::perms::check(p, ctx)?;
                        crate::sql::binder::perms::check_row_security(p, ctx)?;
                    }
                    open(input, ctx)
                })
            };
            let input = match &snap {
                Some(s) => ctx.with_snapshot(s.clone(), run)?,
                None => run()?,
            };
            Box::new(ContextNode {
                input,
                frame: frame.clone(),
                snap,
            })
        }
        Plan::Filter { input, pred } => Box::new(FilterNode {
            input: open(input, ctx)?,
            pred: pred.clone(),
        }),
        Plan::Project { input, exprs } => Box::new(ProjectNode {
            input: open(input, ctx)?,
            exprs: exprs.clone(),
        }),
        Plan::ProjectSet { input, exprs, srfs } => Box::new(ProjectSetNode {
            input: open(input, ctx)?,
            exprs: exprs.clone(),
            srfs: srfs.clone(),
            cur: None,
        }),
        Plan::Agg {
            input,
            groups,
            sets,
            aggs,
        } => Box::new(
            super::agg::AggNode::new(open(input, ctx)?, groups.clone(), sets.clone(), aggs.clone())
                .planned(plan.clone()),
        ),
        Plan::Window { input, calls } => {
            let node = super::window::WindowNode::new(open(input, ctx)?, calls.clone());
            Box::new(match &**input {
                Plan::Sort { keys, .. } => node.presorted(keys),
                _ => node,
            })
        }
        Plan::Sort { input, keys } => {
            let bound = ctx.sort_bound.take();
            Box::new(SortNode {
                presorted: super::pghash::presorted_keys(plan, ctx).min(keys.len()),
                input: Some(open(input, ctx)?),
                keys: keys.clone(),
                out: Vec::new().into_iter(),
                bound,
            })
        }
        Plan::Limit {
            input,
            limit,
            offset,
            with_ties,
        } => Box::new(LimitNode {
            input: {
                // ExecSetTupleBound: a sort right below needs only LIMIT + OFFSET rows.
                ctx.sort_bound.set(tuple_bound(input, limit, offset, with_ties, ctx));
                let n = open(input, ctx);
                ctx.sort_bound.set(None);
                n?
            },
            limit: limit.clone(),
            offset: offset.clone(),
            with_ties: with_ties.clone(),
            state: None,
            last: None,
            drain: input.writes(),
        }),
        Plan::Distinct { input, on, .. } => Box::new(DistinctNode {
            input: open(input, ctx)?,
            on: on.clone(),
            seen: HashMap::new(),
            prev: None,
            hashed: on.is_none().then(|| plan.clone()),
            out: None,
        }),
        Plan::SetOp {
            kind,
            all,
            left,
            right,
            nondet,
            ..
        } => Box::new(
            SetOpNode::new(*kind, *all, left.clone(), right.clone())
                .nondet(nondet)
                .planned(plan.clone()),
        ),
        Plan::Append { inputs, inh } => {
            let dml = inh.is_some()
                && inputs
                    .first()
                    .is_some_and(|p| matches!(&**p, Plan::Update(_) | Plan::Delete(_)));
            let inputs = if inh.is_some() && !dml {
                runtime_pruned(inputs, crate::sql::binder::partition::Known::Exec, ctx)
            } else {
                inputs.clone()
            };
            // The statements a rule made of one: separate commands.
            let commands = inh.is_none() && inputs.iter().any(|p| p.writes());
            Box::new(AppendNode {
                inputs,
                i: 0,
                cur: None,
                dml,
                done: Vec::new(),
                capture: false,
                commands,
            })
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
            if let Some((lk, rk)) = hash
                && params.is_empty()
                && meta.swapped
                && matches!(kind, JoinKind::Semi | JoinKind::Anti)
                && meta.method != Some(crate::sql::plan::JoinMethod::Merge)
            {
                // Hash Right Semi / Anti Join: the left rows hashed, the right rows
                // probing.
                Box::new(RightSemiNode {
                    left: left.clone(),
                    right: right.clone(),
                    lkeys: lk.clone(),
                    rkeys: rk.clone(),
                    cond: cond.clone(),
                    out: None,
                    anti: *kind == JoinKind::Anti,
                    nbuckets: meta.hash_buckets,
                })
            } else if let Some((lk, rk)) = hash
                && params.is_empty()
            {
                Box::new(HashJoinNode::new(
                    *kind,
                    open(left, ctx)?,
                    right.clone(),
                    lk.clone(),
                    rk.clone(),
                    cond.clone(),
                    *nleft,
                    *nright,
                    meta,
                ))
            } else if *kind == JoinKind::Full
                && params.iter().all(|e| e.columns().is_empty())
                && matches!(cond, Some(Expr::Const(Value::Bool(false) | Value::Null, _)))
            {
                // FULL JOIN ON false: a Merge Full Join whose constant-false clause makes
                // every outer row compare greater (mj_ConstFalseJoin): the inner rows come
                // out first, then the outer ones, each null-extended.
                Box::new(ConstFalseFullNode {
                    left: Some(open(left, ctx)?),
                    right: right.clone(),
                    params: params.clone(),
                    nleft: *nleft,
                    nright: *nright,
                    pending: None,
                })
            } else {
                let mut nl = NestLoopNode::new(
                    *kind,
                    open(left, ctx)?,
                    right.clone(),
                    cond.clone(),
                    params.clone(),
                    *nleft,
                    *nright,
                );
                if let Some(m) = meta.memoize.as_ref().filter(|_| !params.is_empty()) {
                    nl.memo = Some(Memoize {
                        keys: m.keys.clone(),
                        key: Arc::as_ptr(plan) as usize + 1,
                        cache: HashMap::new(),
                    });
                }
                Box::new(nl)
            }
        }
        Plan::With { ctes, body, .. } => {
            // CTEs are computed on demand (so LIMIT stops an endless recursive CTE, and unread
            // rows are never produced). A data-modifying one starts when first read; those not
            // read to their end run to completion after the main query, the last one first
            // (ExecPostprocessPlan). All of them read the statement's snapshot, so none sees
            // what another writes.
            let mut dml = Vec::new();
            for (id, p) in ctes {
                let state = if p.writes() {
                    let st = Rc::new(RefCell::new(CteState {
                        node: None,
                        rows: Vec::new(),
                        dml: Some(p.clone()),
                    }));
                    dml.push(st.clone());
                    st
                } else {
                    Rc::new(RefCell::new(CteState {
                        node: Some(open(p, ctx)?),
                        rows: Vec::new(),
                        dml: None,
                    }))
                };
                ctx.ctes.borrow_mut().insert(*id, state);
            }
            if dml.is_empty() {
                open(body, ctx)?
            } else {
                // The AFTER triggers of all its statements fire at its end, in the order
                // their events were queued (AfterTriggerEndQuery).
                let queues = !ctx.after_running.replace(true);
                let body = match open(body, ctx) {
                    Ok(b) => b,
                    Err(e) => {
                        if queues {
                            ctx.after_running.set(false);
                            ctx.after_queue.borrow_mut().clear();
                        }
                        return Err(e);
                    }
                };
                Box::new(WithNode { body, dml, queues })
            }
        }
        Plan::CteScan { id, .. } => {
            let state = ctx
                .ctes
                .borrow()
                .get(id)
                .cloned()
                .ok_or_else(|| crate::error::internal(format!("CTE {id} not materialised")))?;
            Box::new(CteScanNode { state, i: 0 })
        }
        Plan::Recursive {
            id,
            base,
            recursive,
            all,
            ..
        } => Box::new(RecursiveNode {
            id: *id,
            base: Some(open(base, ctx)?),
            recursive: recursive.clone(),
            all: *all,
            seen: HashMap::new(),
            working: Vec::new(),
            produced: None,
        }),
        Plan::WorkScan { id, .. } => {
            let rows = ctx.work.borrow().get(id).cloned().unwrap_or_default();
            Box::new(RcNode { rows, i: 0 })
        }
        Plan::EnrScan { name, .. } => {
            let rows = ctx
                .enrs
                .borrow()
                .iter()
                .rev()
                .find(|e| e.name == *name)
                .map(|e| e.rows.clone())
                .unwrap_or_default();
            Box::new(RcNode { rows, i: 0 })
        }
        Plan::LockRows { input, .. } => {
            // (Row marks: ExecutorStart takes es_output_cid.)
            ctx.txn.borrow_mut().output_cid();
            open(input, ctx)?
        }
        Plan::Insert(p) => Box::new(super::dml::InsertNode::new(p.clone(), ctx)?),
        Plan::Update(p) => Box::new(super::dml::UpdateNode::new(p.clone(), ctx)?),
        Plan::Delete(p) => Box::new(super::dml::DeleteNode::new(p.clone(), ctx)?),
        Plan::Merge(p) => Box::new(super::merge::MergeNode::new(p.clone(), ctx)?),
    })
}

pub struct VecNode {
    rows: std::vec::IntoIter<Row>,
}

impl VecNode {
    pub fn new(rows: Vec<Row>) -> Self {
        VecNode {
            rows: rows.into_iter(),
        }
    }
}

impl Node for VecNode {
    fn next(&mut self, _: &ExecCtx) -> Result<Option<Row>> {
        Ok(self.rows.next())
    }
}

struct RcNode {
    rows: Rc<Vec<Row>>,
    i: usize,
}

impl Node for RcNode {
    fn next(&mut self, _: &ExecCtx) -> Result<Option<Row>> {
        let r = self.rows.get(self.i).cloned();
        self.i += 1;
        Ok(r)
    }
}

struct ValuesNode {
    rows: Vec<Vec<Expr>>,
    i: usize,
}

impl Node for ValuesNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let Some(r) = self.rows.get(self.i) else {
            return Ok(None);
        };
        self.i += 1;
        Ok(Some(eval_all(r, &[], ctx)?))
    }
}

/// A system view scan where some rows' columns are errors (`system::deferred_errors`): as
/// Postgres computes a view column only for the rows it returns and when the query reads it,
/// an error is raised when its row passes the filter and the column is read (or when the
/// filter reads it).
struct SysScanNode {
    rows: std::iter::Enumerate<std::vec::IntoIter<Row>>,
    /// (row, column, error).
    deferred: Vec<(usize, usize, Error)>,
    /// The columns the statement reads (attnums).
    read: std::collections::BTreeSet<i16>,
    in_filter: std::collections::BTreeSet<usize>,
    filter: Option<Expr>,
}

impl Node for SysScanNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        for (i, r) in self.rows.by_ref() {
            ctx.check()?;
            let errs = self.deferred.iter().filter(|d| d.0 == i);
            if let Some(d) = errs.clone().find(|d| self.in_filter.contains(&d.1)) {
                return Err(d.2.clone());
            }
            if let Some(f) = &self.filter
                && !eval_bool(f, &r, ctx)?
            {
                continue;
            }
            if let Some(d) = errs
                .clone()
                .find(|d| self.read.contains(&(d.1 as i16 + 1)))
            {
                return Err(d.2.clone());
            }
            return Ok(Some(r));
        }
        Ok(None)
    }
}

pub struct FilterNode {
    pub input: BoxNode,
    pub pred: Expr,
}

impl Node for FilterNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        while let Some(r) = self.input.next(ctx)? {
            ctx.check()?;
            if eval_bool(&self.pred, &r, ctx)? {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }
}

struct ProjectNode {
    input: BoxNode,
    exprs: Vec<Expr>,
}

impl Node for ProjectNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        match self.input.next(ctx)? {
            Some(r) => Ok(Some(eval_all(&self.exprs, &r, ctx)?)),
            None => Ok(None),
        }
    }
}

/// Start a set-returning call (or a plain function, as one row).
pub fn start_srf(call: &SrfCall, row: &[Value], ctx: &ExecCtx) -> Result<SrfIter> {
    // XMLTABLE evaluates its arguments itself, as its scan reaches them.
    let args = if crate::sql::func::xmltable::is_xmltable(&call.func) {
        Vec::new()
    } else {
        eval_all(&call.args, row, ctx)?
    };
    if call.func.strict && args.iter().any(Value::is_null) {
        // A strict function's NULL: no rows from a set-returning one, one row of NULLs from
        // any other (a built-in returning a record isn't SETOF unless pg_proc says so).
        let sigs = crate::catalog::system::builtin_proc_sigs();
        let retset = match call.func.srf {
            None => false,
            Some(_) => match sigs.binary_search_by_key(&call.func.oid, |s| s.oid) {
                Ok(i) => sigs[i].retset,
                Err(_) => true,
            },
        };
        if retset || call.body.is_some() {
            return Ok(Box::new(std::iter::empty()));
        }
        // (One NULL: the whole result, a NULL record where it has several columns.)
        return Ok(Box::new(std::iter::once(Ok(vec![Value::Null]))));
    }
    if let Some(body) = &call.body {
        let rows = ctx.with_outer(Rc::new(args), || super::collect(body, ctx, None))?;
        return Ok(Box::new(rows.into_iter().map(Ok)));
    }
    let fc = FnCtx {
        exec: ctx,
        ret: &call.ty,
        args: &call.args,
        coll: call.func.coll.as_deref(),
    };
    match call.func.srf {
        Some(f) => f(&args, &fc),
        None => {
            let v = (call.func.imp)(&args, &fc)?;
            let row = match (&v, call.ncols) {
                (Value::Record(fields), n) if n > 1 => fields.as_ref().clone(),
                _ => vec![v],
            };
            Ok(Box::new(std::iter::once(Ok(row))))
        }
    }
}

struct ProjectSetNode {
    input: BoxNode,
    exprs: Vec<Expr>,
    srfs: Vec<SrfCall>,
    /// The current input row and its running SRFs.
    cur: Option<(Row, Vec<Option<SrfIter>>)>,
}

impl Node for ProjectSetNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            if self.cur.is_none() {
                let Some(row) = self.input.next(ctx)? else {
                    return Ok(None);
                };
                let iters = self
                    .srfs
                    .iter()
                    .map(|s| start_srf(s, &row, ctx).map(Some))
                    .collect::<Result<Vec<_>>>()?;
                self.cur = Some((row, iters));
            }
            let (row, iters) = self.cur.as_mut().unwrap();
            let mut any = false;
            let mut vals = Vec::with_capacity(iters.len());
            for (it, call) in iters.iter_mut().zip(&self.srfs) {
                let v = match it {
                    Some(i) => match i.next() {
                        Some(r) => {
                            any = true;
                            let r = r?;
                            if call.ncols > 1 && r.len() > 1 {
                                Value::Record(Arc::new(r))
                            } else {
                                r.into_iter().next().unwrap_or(Value::Null)
                            }
                        }
                        None => {
                            *it = None;
                            Value::Null
                        }
                    },
                    None => Value::Null,
                };
                vals.push(v);
            }
            if !any {
                self.cur = None;
                continue;
            }
            let mut full = row.clone();
            full.extend(vals);
            return Ok(Some(eval_all(&self.exprs, &full, ctx)?));
        }
    }
}

struct FunctionScanNode {
    iters: Vec<Option<SrfIter>>,
    ncols: Vec<usize>,
    ordinality: bool,
    n: i64,
}

impl FunctionScanNode {
    fn new(calls: &[SrfCall], ordinality: bool, ctx: &ExecCtx) -> Result<Self> {
        let iters = calls
            .iter()
            .map(|c| {
                // ROWS FROM: rescanned, a function whose arguments didn't change isn't run
                // again (ExecReScanFunctionScan rereads its tuplestore).
                let fixed = calls.len() > 1
                    && !c.args.iter().any(|a| {
                        let mut p = false;
                        a.walk(&mut |e| p |= matches!(e, Expr::Outer(..) | Expr::Param(..)));
                        p
                    });
                if !fixed {
                    return start_srf(c, &[], ctx).map(Some);
                }
                let key = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    (c as *const SrfCall as usize).hash(&mut h);
                    format!("{c:?}").hash(&mut h);
                    (h.finish() as usize) | (1 << (usize::BITS - 1))
                };
                let cached = ctx.subq_cache.borrow().get(&key).cloned();
                let rows = match cached {
                    Some(r) => r,
                    None => {
                        let rows: Vec<Row> = start_srf(c, &[], ctx)?.collect::<Result<_>>()?;
                        let rows = Rc::new(rows);
                        ctx.subq_cache.borrow_mut().insert(key, rows.clone());
                        rows
                    }
                };
                let it: SrfIter = Box::new((*rows).clone().into_iter().map(Ok));
                Ok(Some(it))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(FunctionScanNode {
            iters,
            ncols: calls.iter().map(|c| c.ncols).collect(),
            ordinality,
            n: 0,
        })
    }
}

impl Node for FunctionScanNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        ctx.check()?;
        let mut any = false;
        let mut row = Vec::new();
        for (it, n) in self.iters.iter_mut().zip(&self.ncols) {
            match it.as_mut().and_then(|i| i.next()) {
                Some(r) => {
                    any = true;
                    let mut r = r?;
                    // A function returning a composite value (unnest of a row array): its
                    // fields are the columns.
                    if *n > 1
                        && r.len() == 1
                        && let Value::Record(f) = &r[0]
                    {
                        r = f.to_vec();
                    }
                    r.resize(*n, Value::Null);
                    row.extend(r);
                }
                None => {
                    *it = None;
                    row.extend(std::iter::repeat_n(Value::Null, *n));
                }
            }
        }
        if !any {
            return Ok(None);
        }
        self.n += 1;
        if self.ordinality {
            row.push(Value::Int8(self.n));
        }
        Ok(Some(row))
    }
}

/// Whether two grouping key values fall in one group (NULLs together).
pub fn same_group(a: &Value, b: &Value) -> bool {
    match (a.is_null(), b.is_null()) {
        (true, true) => true,
        (false, false) => a == b,
        _ => false,
    }
}

/// Compare two sort-key values with NULL placement, direction and an optional USING operator.
pub fn compare_values(a: &Value, b: &Value, k: &SortKey, ctx: &ExecCtx) -> Result<Ordering> {
    match (a.is_null(), b.is_null()) {
        (true, true) => return Ok(Ordering::Equal),
        (true, false) => {
            return Ok(if k.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            });
        }
        (false, true) => {
            return Ok(if k.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            });
        }
        _ => {}
    }
    let o = match &k.using {
        // A collation's own "<": compare under it directly.
        Some(lt)
            if lt.name == "<"
                && !matches!(k.expr.ty(), crate::types::DataType::Bpchar(_))
                && let Some(coll) = &lt.coll =>
        {
            crate::types::collation::cmp_values(coll, a, b)
        }
        Some(lt) => {
            let ty = k.expr.ty();
            if call2(lt, a.clone(), b.clone(), &ty, ctx)? == Value::Bool(true) {
                Ordering::Less
            } else if call2(lt, b.clone(), a.clone(), &ty, ctx)? == Value::Bool(true) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        None => a.cmp_total(b),
    };
    Ok(if k.desc { o.reverse() } else { o })
}

pub fn sort_rows(rows: Vec<Row>, keys: &[SortKey], ctx: &ExecCtx) -> Result<Vec<Row>> {
    sort_rows_bounded(rows, keys, None, ctx)
}

/// Sort rows as Postgres's tuplesort does (rows with equal keys in its quicksort's
/// order); `bound`: only the first that many are wanted (a LIMIT above the sort).
pub fn sort_rows_bounded(
    rows: Vec<Row>,
    keys: &[SortKey],
    bound: Option<usize>,
    ctx: &ExecCtx,
) -> Result<Vec<Row>> {
    let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
    for r in rows {
        let k = keys
            .iter()
            .map(|k| eval(&k.expr, &r, ctx))
            .collect::<Result<Vec<_>>>()?;
        keyed.push((k, r));
    }
    let mut f = |a: &(Vec<Value>, Row), b: &(Vec<Value>, Row)| -> Result<Ordering> {
        for (i, k) in keys.iter().enumerate() {
            match compare_values(&a.0[i], &b.0[i], k, ctx)? {
                Ordering::Equal => {}
                o => return Ok(o),
            }
        }
        Ok(Ordering::Equal)
    };
    let mut cmp = super::pgsort::Cmp::new(&mut f);
    let sorted = super::pgsort::tuplesort(keyed, bound, &mut cmp);
    if let Some(e) = cmp.err {
        return Err(e);
    }
    Ok(sorted.into_iter().map(|(_, r)| r).collect())
}

/// An Incremental Sort's rows: the input sorted on the first `presorted` keys already.
fn incremental_sort_rows(
    rows: Vec<Row>,
    keys: &[SortKey],
    presorted: usize,
    bound: Option<usize>,
    ctx: &ExecCtx,
) -> Result<Vec<Row>> {
    let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
    for r in rows {
        let k = keys
            .iter()
            .map(|k| eval(&k.expr, &r, ctx))
            .collect::<Result<Vec<_>>>()?;
        keyed.push((k, r));
    }
    let cmp_range = |a: &(Vec<Value>, Row), b: &(Vec<Value>, Row), from: usize, to: usize| {
        for (i, k) in keys.iter().enumerate().take(to).skip(from) {
            match compare_values(&a.0[i], &b.0[i], k, ctx)? {
                Ordering::Equal => {}
                o => return Ok(o),
            }
        }
        Ok(Ordering::Equal)
    };
    let n = keys.len();
    let mut full = |a: &(Vec<Value>, Row), b: &(Vec<Value>, Row)| cmp_range(a, b, 0, n);
    let mut suffix = |a: &(Vec<Value>, Row), b: &(Vec<Value>, Row)| cmp_range(a, b, presorted, n);
    let mut err = None;
    let mut same = |a: &(Vec<Value>, Row), b: &(Vec<Value>, Row)| match cmp_range(a, b, 0, presorted) {
        Ok(o) => o == Ordering::Equal,
        Err(e) => {
            err.get_or_insert(e);
            true
        }
    };
    let mut cf = super::pgsort::Cmp::new(&mut full);
    let mut cs = super::pgsort::Cmp::new(&mut suffix);
    let out = super::pgsort::incremental_sort(keyed, bound, &mut cf, &mut cs, &mut same);
    if let Some(e) = err.or(cf.err).or(cs.err) {
        return Err(e);
    }
    Ok(out.into_iter().map(|(_, r)| r).collect())
}

struct SortNode {
    input: Option<BoxNode>,
    keys: Vec<SortKey>,
    out: std::vec::IntoIter<Row>,
    /// tuplesort_set_bound: the rows a LIMIT above wants.
    bound: Option<usize>,
    /// An Incremental Sort: the leading keys the input is sorted on already.
    presorted: usize,
}

impl Node for SortNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if let Some(mut input) = self.input.take() {
            // tuplesort_begin_heap sets up each key's comparison before any row is read: a
            // key with no collation fails right away.
            if self.keys.iter().any(|k| {
                k.using
                    .as_ref()
                    .is_some_and(|u| crate::sql::binder::is_indeterminate(u))
            }) {
                return Err(Error::new(
                    code::INDETERMINATE_COLLATION,
                    "could not determine which collation to use for string comparison",
                )
                .hint("Use the COLLATE clause to set the collation explicitly."));
            }
            let mut rows = Vec::new();
            while let Some(r) = input.next(ctx)? {
                ctx.check()?;
                rows.push(r);
            }
            self.out = if self.presorted > 0 {
                incremental_sort_rows(rows, &self.keys, self.presorted, self.bound, ctx)?
            } else {
                sort_rows_bounded(rows, &self.keys, self.bound, ctx)?
            }
            .into_iter();
        }
        Ok(self.out.next())
    }
}

struct LimitNode {
    input: BoxNode,
    limit: Option<Expr>,
    offset: Option<Expr>,
    with_ties: Option<Vec<SortKey>>,
    /// (remaining to emit, remaining to skip)
    state: Option<(Option<u64>, u64)>,
    last: Option<Vec<Value>>,
    /// The input writes: it runs to completion past the limit (a data-modifying
    /// statement always does).
    drain: bool,
}

/// The bound a LIMIT passes down to a sort below it (through projections), when its
/// counts are known and valid.
fn tuple_bound(
    input: &Plan,
    limit: &Option<Expr>,
    offset: &Option<Expr>,
    with_ties: &Option<Vec<SortKey>>,
    ctx: &ExecCtx,
) -> Option<usize> {
    let mut p = input;
    loop {
        match p {
            // (And through a subquery's scan without quals: ExecSetTupleBound.)
            Plan::Project { input, .. } | Plan::Barrier { input, .. } => p = input,
            Plan::Sort { .. } => break,
            _ => return None,
        }
    }
    if with_ties.is_some() {
        return None;
    }
    let n = |e: &Expr| -> Option<u64> {
        match eval(e, &[], ctx).ok()? {
            Value::Null => None,
            Value::Int8(v) if v >= 0 => Some(v as u64),
            Value::Int4(v) if v >= 0 => Some(v as u64),
            _ => None,
        }
    };
    let l = n(limit.as_ref()?)?;
    let o = match offset {
        Some(e) => match eval(e, &[], ctx).ok()? {
            Value::Null => 0,
            _ => n(e)?,
        },
        None => 0,
    };
    usize::try_from(l.checked_add(o)?).ok()
}

fn count_arg(e: &Expr, ctx: &ExecCtx, what: &str) -> Result<Option<u64>> {
    match eval(e, &[], ctx)? {
        Value::Null => Ok(None),
        v => {
            let n = match &v {
                Value::Numeric(n) => n.to_i64_round().unwrap_or(i64::MAX),
                Value::Float8(f) => f.round() as i64,
                Value::Float4(f) => f.round() as i64,
                v => v.as_i64().unwrap_or(0),
            };
            if n < 0 {
                let (c, msg) = if what == "LIMIT" {
                    (
                        code::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
                        "LIMIT must not be negative",
                    )
                } else {
                    (
                        code::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE,
                        "OFFSET must not be negative",
                    )
                };
                return Err(Error::new(c, msg));
            }
            Ok(Some(n as u64))
        }
    }
}

impl Node for LimitNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.state.is_none() {
            let limit = match &self.limit {
                Some(e) => count_arg(e, ctx, "LIMIT")?,
                None => None,
            };
            let offset = match &self.offset {
                Some(e) => count_arg(e, ctx, "OFFSET")?.unwrap_or(0),
                None => 0,
            };
            self.state = Some((limit, offset));
        }
        let (limit, skip) = self.state.as_mut().unwrap();
        while *skip > 0 {
            if self.input.next(ctx)?.is_none() {
                return Ok(None);
            }
            *skip -= 1;
        }
        match limit {
            Some(0) => {
                // WITH TIES: keep emitting rows equal to the last one on the sort keys.
                let (Some(keys), Some(last)) = (&self.with_ties, &self.last) else {
                    if self.drain {
                        while self.input.next(ctx)?.is_some() {}
                        self.drain = false;
                    }
                    return Ok(None);
                };
                let Some(r) = self.input.next(ctx)? else {
                    return Ok(None);
                };
                let k = keys
                    .iter()
                    .map(|k| eval(&k.expr, &r, ctx))
                    .collect::<Result<Vec<_>>>()?;
                for (i, key) in keys.iter().enumerate() {
                    if compare_values(&k[i], &last[i], key, ctx)? != Ordering::Equal {
                        self.with_ties = None;
                        return Ok(None);
                    }
                }
                Ok(Some(r))
            }
            Some(n) => {
                let r = self.input.next(ctx)?;
                *n -= 1;
                if *n == 0
                    && let (Some(keys), Some(r)) = (&self.with_ties, &r)
                {
                    self.last = Some(
                        keys.iter()
                            .map(|k| eval(&k.expr, r, ctx))
                            .collect::<Result<Vec<_>>>()?,
                    );
                }
                Ok(r)
            }
            None => self.input.next(ctx),
        }
    }
}

/// FULL JOIN ON false (see where it is opened).
struct ConstFalseFullNode {
    left: Option<BoxNode>,
    right: Arc<Plan>,
    /// Values from an enclosing query the right side reads (none from the left row).
    params: Vec<Expr>,
    nleft: usize,
    nright: usize,
    pending: Option<std::vec::IntoIter<Row>>,
}

impl Node for ConstFalseFullNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.pending.is_none() {
            let rows = if self.params.is_empty() {
                collect(&self.right, ctx, None)?
            } else {
                let p = eval_all(&self.params, &[], ctx)?;
                ctx.with_outer(Rc::new(p), || collect(&self.right, ctx, None))?
            };
            self.pending = Some(rows.into_iter());
        }
        if let Some(r) = self.pending.as_mut().and_then(|p| p.next()) {
            let mut o = vec![Value::Null; self.nleft];
            o.extend(r);
            return Ok(Some(o));
        }
        let Some(left) = self.left.as_mut() else {
            return Ok(None);
        };
        match left.next(ctx)? {
            Some(mut r) => {
                r.extend(std::iter::repeat_n(Value::Null, self.nright));
                Ok(Some(r))
            }
            None => {
                self.left = None;
                Ok(None)
            }
        }
    }
}

struct DistinctNode {
    input: BoxNode,
    on: Option<Vec<Expr>>,
    seen: HashMap<Row, ()>,
    prev: Option<Vec<Value>>,
    /// A HashAggregate: the plan node (its estimate sizes the table); its rows come out
    /// once the input is read, in the table's order.
    hashed: Option<Arc<Plan>>,
    out: Option<std::vec::IntoIter<Row>>,
}

/// Rows deduplicated through a TupleHashTable sized for `plan`'s estimate, in the
/// table's iteration order (the first row of each group).
fn hashed_distinct(
    rows: impl IntoIterator<Item = Row>,
    key: impl Fn(&Row) -> Row,
    types: Vec<crate::types::DataType>,
    plan: &Arc<Plan>,
    ctx: &ExecCtx,
) -> Result<Vec<Row>> {
    let mut index: HashMap<Row, usize> = HashMap::new();
    let mut firsts: Vec<Row> = Vec::new();
    let mut order = super::pghash::GroupOrder::new(types, ctx);
    for r in rows {
        ctx.check()?;
        let k = key(&r);
        let g = match index.get(&k) {
            Some(g) => *g,
            None => {
                firsts.push(r);
                index.insert(k.clone(), firsts.len() - 1);
                firsts.len() - 1
            }
        };
        order.row(&k, g);
    }
    let width = firsts.first().map_or(1, Vec::len);
    let est = super::pghash::estimated_rows(plan, ctx);
    let mut slots: Vec<Option<Row>> = firsts.into_iter().map(Some).collect();
    Ok(order
        .order(est, width, ctx)
        .into_iter()
        .filter_map(|i| slots[i].take())
        .collect())
}

/// Rows sorted ascending on columns `cols` (a Unique's input order).
fn sort_rows_by(rows: &mut [Row], cols: &[usize], ctx: &ExecCtx) -> Result<()> {
    let key = SortKey {
        expr: Expr::Col(0, crate::types::DataType::Unknown),
        desc: false,
        nulls_first: false,
        using: None,
    };
    let mut err = None;
    rows.sort_by(|a, b| {
        for &c in cols {
            match compare_values(&a[c], &b[c], &key, ctx) {
                Ok(Ordering::Equal) => {}
                Ok(o) => return o,
                Err(e) => {
                    err.get_or_insert(e);
                    return Ordering::Equal;
                }
            }
        }
        Ordering::Equal
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The types of a plan's output columns, as far as known.
fn output_types(p: &Plan, row: Option<&Row>) -> Vec<crate::types::DataType> {
    let n = row.map_or(0, Vec::len);
    let known: Vec<crate::types::DataType> = match p {
        Plan::Project { exprs, .. } => exprs.iter().map(Expr::ty).collect(),
        _ => Vec::new(),
    };
    (0..n)
        .map(|i| {
            known
                .get(i)
                .cloned()
                .filter(|t| *t != crate::types::DataType::Unknown)
                .unwrap_or_else(|| value_type(&row.unwrap()[i]))
        })
        .collect()
}

impl Node for DistinctNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if let Some(plan) = self.hashed.take() {
            let mut rows = Vec::new();
            while let Some(r) = self.input.next(ctx)? {
                ctx.check()?;
                rows.push(r);
            }
            let input = match &*plan {
                Plan::Distinct { input, .. } => input.clone(),
                _ => plan.clone(),
            };
            // (Shown as a Unique over its input sorted: the rows in that order.)
            let out = if let Some(cols) = crate::sql::explain::distinct_order(&plan, ctx) {
                let mut seen: HashMap<Row, ()> = HashMap::new();
                let mut out: Vec<Row> = Vec::new();
                for r in rows {
                    if seen.insert(r.clone(), ()).is_none() {
                        out.push(r);
                    }
                }
                sort_rows_by(&mut out, &cols, ctx)?;
                out
            } else {
                let types = output_types(&input, rows.first());
                hashed_distinct(rows, |r| r.clone(), types, &plan, ctx)?
            };
            self.out = Some(out.into_iter());
        }
        if let Some(out) = &mut self.out {
            return Ok(out.next());
        }
        while let Some(r) = self.input.next(ctx)? {
            ctx.check()?;
            match &self.on {
                None => {
                    if self.seen.contains_key(&r) {
                        continue;
                    }
                    self.seen.insert(r.clone(), ());
                    return Ok(Some(r));
                }
                Some(on) => {
                    let k = eval_all(on, &r, ctx)?;
                    // Keys under a nondeterministic collation compare by what it finds equal.
                    let colls = super::agg::nondeterministic_keys(on, ctx);
                    let k = super::agg::group_key(&k, &colls);
                    if self
                        .prev
                        .as_ref()
                        .is_some_and(|p| super::agg::cmp_rows(p, &k) == Ordering::Equal)
                    {
                        continue;
                    }
                    self.prev = Some(k);
                    return Ok(Some(r));
                }
            }
        }
        Ok(None)
    }
}

struct SetOpNode {
    kind: SetOpKind,
    all: bool,
    left: Arc<Plan>,
    right: Arc<Plan>,
    out: Option<std::vec::IntoIter<Row>>,
    /// Per column, the nondeterministic collation equality follows.
    nondet: Vec<Option<String>>,
    /// The plan node (a hashed UNION's estimate sizes its table).
    plan: Option<Arc<Plan>>,
}

impl SetOpNode {
    fn new(kind: SetOpKind, all: bool, left: Arc<Plan>, right: Arc<Plan>) -> Self {
        SetOpNode {
            kind,
            all,
            left,
            right,
            out: None,
            nondet: Vec::new(),
            plan: None,
        }
    }

    fn nondet(mut self, nondet: &[Option<String>]) -> Self {
        self.nondet = nondet.to_vec();
        self
    }

    fn planned(mut self, plan: Arc<Plan>) -> Self {
        self.plan = Some(plan);
        self
    }

    fn run(&self, ctx: &ExecCtx) -> Result<Vec<Row>> {
        let (left, right) = if self.kind == SetOpKind::Union && !self.all {
            // plan_union_children: nested UNIONs (and UNION ALLs) fold into this one's
            // Append.
            fn arms(p: &Arc<Plan>, out: &mut Vec<Arc<Plan>>) {
                match &**p {
                    Plan::SetOp {
                        kind: SetOpKind::Union,
                        left,
                        right,
                        ..
                    } => {
                        arms(left, out);
                        arms(right, out);
                    }
                    Plan::Append { inputs, inh: None } => {
                        inputs.iter().for_each(|i| arms(i, out));
                    }
                    _ => out.push(p.clone()),
                }
            }
            let mut list = Vec::new();
            arms(&self.left, &mut list);
            arms(&self.right, &mut list);
            let mut rows = Vec::new();
            for a in &list {
                rows.extend(collect(a, ctx, None)?);
            }
            (rows, Vec::new())
        } else {
            (collect(&self.left, ctx, None)?, collect(&self.right, ctx, None)?)
        };
        // Rows compare by their keys: strings under a nondeterministic collation by what it
        // finds equal.
        let colls: Vec<Option<Arc<crate::types::collation::Collation>>> = if self.nondet.is_empty()
        {
            Vec::new()
        } else {
            let cat = ctx.catalog();
            self.nondet
                .iter()
                .map(|n| {
                    n.as_ref()
                        .and_then(|n| crate::types::collation::by_name(&cat, n))
                })
                .collect()
        };
        let key = |r: &Row| -> Row { super::agg::group_key(r, &colls) };
        let mut out = Vec::new();
        match self.kind {
            SetOpKind::Union if self.all => out.extend(left.into_iter().chain(right)),
            SetOpKind::Union => {
                // A HashAggregate over the arms' Append.
                let types = output_types(&self.left, left.first().or(right.first()));
                // (Shown as a Unique over the rows sorted: in that order.)
                if let Some(plan) = &self.plan
                    && let Some(cols) = crate::sql::explain::distinct_order(plan, ctx)
                {
                    let mut seen: HashMap<Row, ()> = HashMap::new();
                    for r in left.into_iter().chain(right) {
                        if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(key(&r)) {
                            e.insert(());
                            out.push(r);
                        }
                    }
                    sort_rows_by(&mut out, &cols, ctx)?;
                    return Ok(out);
                }
                return match &self.plan {
                    Some(plan) => hashed_distinct(left.into_iter().chain(right), key, types, plan, ctx),
                    None => {
                        let mut seen: HashMap<Row, ()> = HashMap::new();
                        for r in left.into_iter().chain(right) {
                            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(key(&r)) {
                                e.insert(());
                                out.push(r);
                            }
                        }
                        Ok(out)
                    }
                };
            }
            SetOpKind::Intersect | SetOpKind::Except
                if self
                    .plan
                    .as_ref()
                    .is_some_and(|p| crate::sql::explain::distinct_order(p, ctx).is_none()) =>
            {
                return self.hashed_setop(left, right, &key, ctx);
            }
            SetOpKind::Intersect | SetOpKind::Except => {
                let mut counts: HashMap<Row, usize> = HashMap::new();
                for r in right {
                    *counts.entry(key(&r)).or_insert(0) += 1;
                }
                let mut emitted: HashMap<Row, ()> = HashMap::new();
                for r in left {
                    let k = key(&r);
                    let c = counts.get(&k).copied().unwrap_or(0);
                    let keep = if self.kind == SetOpKind::Intersect {
                        if self.all {
                            if c > 0 {
                                *counts.get_mut(&k).unwrap() -= 1;
                                true
                            } else {
                                false
                            }
                        } else {
                            c > 0 && !emitted.contains_key(&k)
                        }
                    } else if self.all {
                        if c > 0 {
                            *counts.get_mut(&k).unwrap() -= 1;
                            false
                        } else {
                            true
                        }
                    } else {
                        c == 0 && !emitted.contains_key(&k)
                    };
                    if keep {
                        if !self.all {
                            emitted.insert(k.clone(), ());
                        }
                        out.push(r);
                    }
                }
                // (A sorted SetOp: its rows in the columns' order.)
                if let Some(plan) = &self.plan
                    && let Some(cols) = crate::sql::explain::distinct_order(plan, ctx)
                {
                    sort_rows_by(&mut out, &cols, ctx)?;
                }
            }
        }
        Ok(out)
    }
}

impl SetOpNode {
    /// HashSetOp (nodeSetOp.c): the left input's rows grouped in a TupleHashTable with
    /// both inputs' counts; groups come out in the table's order, each as many times as
    /// the operation keeps it (its first row).
    fn hashed_setop(
        &self,
        left: Vec<Row>,
        right: Vec<Row>,
        key: &dyn Fn(&Row) -> Row,
        ctx: &ExecCtx,
    ) -> Result<Vec<Row>> {
        let intersect = self.kind == SetOpKind::Intersect;
        let (swapped, est) = crate::sql::explain::setop_shape(&self.left, &self.right, intersect, ctx);
        let (left, right, lplan) = if swapped {
            (right, left, &self.right)
        } else {
            (left, right, &self.left)
        };
        let types = output_types(lplan, left.first());
        let mut index: HashMap<Row, usize> = HashMap::new();
        let mut groups: Vec<(Row, usize, usize)> = Vec::new();
        let mut order = super::pghash::GroupOrder::new(types, ctx);
        for r in left {
            ctx.check()?;
            let k = key(&r);
            let g = match index.get(&k) {
                Some(g) => *g,
                None => {
                    groups.push((r, 0, 0));
                    index.insert(k.clone(), groups.len() - 1);
                    groups.len() - 1
                }
            };
            groups[g].1 += 1;
            order.row(&k, g);
        }
        for r in right {
            ctx.check()?;
            if let Some(g) = index.get(&key(&r)) {
                groups[*g].2 += 1;
            }
        }
        let width = groups.first().map_or(1, |g| g.0.len());
        let mut out = Vec::new();
        for g in order.order(est, width, ctx) {
            let (row, l, r) = &groups[g];
            let n = match (self.kind, self.all) {
                (SetOpKind::Intersect, false) => usize::from(*l > 0 && *r > 0),
                (SetOpKind::Intersect, true) => (*l).min(*r),
                (_, false) => usize::from(*l > 0 && *r == 0),
                (_, true) => l.saturating_sub(*r),
            };
            out.extend(std::iter::repeat_n(row.clone(), n));
        }
        Ok(out)
    }
}

impl Node for SetOpNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.out.is_none() {
            self.out = Some(self.run(ctx)?.into_iter());
        }
        Ok(self.out.as_mut().unwrap().next())
    }
}

struct AppendNode {
    inputs: Vec<Arc<Plan>>,
    i: usize,
    cur: Option<BoxNode>,
    /// UPDATE / DELETE of an inheritance tree: the members' AFTER triggers fire once all
    /// rows are done, row events first, as Postgres's after-trigger queue does.
    dml: bool,
    done: Vec<BoxNode>,
    /// The parent's transition tables take the members' rows.
    capture: bool,
    /// Each input is a command of its own (a rule's actions and the statement): it sees
    /// what the ones before it wrote.
    commands: bool,
}

impl Node for AppendNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            if self.cur.is_none() {
                let Some(p) = self.inputs.get(self.i) else {
                    if self.dml {
                        let mut done = std::mem::take(&mut self.done);
                        if self.capture && !done.is_empty() {
                            capture_into_parent(&mut done);
                        }
                        super::trigger::batch(ctx, || {
                            for n in &mut done {
                                n.end(ctx, false)?;
                            }
                            for n in &mut done {
                                n.end(ctx, true)?;
                            }
                            Ok(())
                        })?;
                    }
                    return Ok(None);
                };
                if self.commands && self.i > 0 {
                    ctx.next_command();
                }
                ctx.defer_after.set(self.dml);
                ctx.capture.set(self.capture && self.i > 0);
                let opened = open(p, ctx);
                ctx.defer_after.set(false);
                ctx.capture.set(false);
                let opened = opened?;
                if self.dml && self.i == 0 {
                    self.capture = opened.captures();
                }
                self.cur = Some(opened);
                self.i += 1;
            }
            match self.cur.as_mut().unwrap().next(ctx)? {
                Some(r) => return Ok(Some(r)),
                None => {
                    let n = self.cur.take().unwrap();
                    if self.dml {
                        self.done.push(n);
                    }
                }
            }
        }
    }
}

/// The partitions of an Append left once the clauses' values known `by` then prune (run-time
/// partition pruning).
pub fn runtime_pruned(
    inputs: &[Arc<Plan>],
    by: crate::sql::binder::partition::Known,
    ctx: &ExecCtx,
) -> Vec<Arc<Plan>> {
    /// A member's scan and the clauses over its rows (a filter above the projection to the
    /// parent's columns is read through it).
    fn member(p: &Plan) -> Option<(&crate::sql::plan::ScanPlan, Vec<Expr>)> {
        match p {
            Plan::Scan(s) => Some((s, s.filter.iter().cloned().collect())),
            Plan::Project { input, exprs }
                if exprs
                    .iter()
                    .all(|e| matches!(e, Expr::Col(..) | Expr::Const(..))) =>
            {
                member(input)
            }
            Plan::Filter { input, pred } => {
                let (s, mut quals) = member(input)?;
                // Over the projection's output: its columns are the scan's.
                let pred = match &**input {
                    Plan::Project { exprs, .. } => pred
                        .clone()
                        .transform(&mut |e| match e {
                            Expr::Col(i, _) => Ok(exprs.get(i).cloned().unwrap_or(e)),
                            e => Ok(e),
                        })
                        .ok()?,
                    _ => pred.clone(),
                };
                quals.push(pred);
                Some((s, quals))
            }
            _ => None,
        }
    }
    // (enable_partition_pruning off: none pruned when the plan runs either.)
    let on = ctx
        .sess
        .settings
        .borrow()
        .get_raw("enable_partition_pruning")
        .is_none_or(|v| !matches!(v.as_str(), "off" | "false" | "0"));
    inputs
        .iter()
        .filter(|p| match member(p) {
            Some((s, quals)) if on && !quals.is_empty() => match &s.part {
                Some(levels) => {
                    !crate::sql::binder::partition::pruned_at(&quals, levels, by, &mut |e| {
                        crate::sql::expr::eval(e, &[], ctx)
                    })
                }
                None => true,
            },
            _ => true,
        })
        .cloned()
        .collect()
}

/// Hand the members' captured rows to the parent (the first member), as its rows.
fn capture_into_parent(done: &mut [BoxNode]) {
    let Some(root) = done[0].dml_table() else {
        return;
    };
    let mut rows = Vec::new();
    for n in done[1..].iter_mut() {
        let got = n.take_captured();
        let Some(t) = n.dml_table().filter(|_| !got.is_empty()) else {
            continue;
        };
        let map: Vec<Option<usize>> = root
            .columns
            .iter()
            .map(|c| {
                if c.dropped {
                    None
                } else {
                    t.column_index(&c.name)
                }
            })
            .collect();
        let conv = |r: &Row| -> Row {
            map.iter()
                .map(|m| m.map_or(Value::Null, |j| r[j].clone()))
                .collect()
        };
        for a in got {
            rows.push(super::trigger::AfterRow {
                event: a.event,
                old: a.old.as_ref().map(conv),
                new: a.new.as_ref().map(conv),
                key: Vec::new(),
            });
        }
    }
    done[0].add_captured(rows);
}

fn concat_rows(l: &[Value], r: &[Value]) -> Row {
    let mut out = Vec::with_capacity(l.len() + r.len());
    out.extend_from_slice(l);
    out.extend_from_slice(r);
    out
}

/// A Memoize over a nested loop's parameterized inner side: the inner rows for each set
/// of key values, run once per distinct set (counted under `key` for EXPLAIN ANALYZE).
struct Memoize {
    keys: Vec<Expr>,
    key: usize,
    cache: HashMap<Vec<Value>, Rc<Vec<Row>>>,
}

/// Nested loop: the right side is materialised once, or re-run per left row for LATERAL.
struct NestLoopNode {
    kind: JoinKind,
    left: BoxNode,
    right_plan: Arc<Plan>,
    cond: Option<Expr>,
    params: Vec<Expr>,
    nleft: usize,
    nright: usize,
    right: Option<Rc<Vec<Row>>>,
    right_matched: Vec<bool>,
    cur: Option<(Row, Rc<Vec<Row>>, usize, bool)>,
    unmatched_right: Option<usize>,
    memo: Option<Memoize>,
    /// A RIGHT / FULL join whose right side reads only an enclosing query's values: run
    /// once, its rows' matches kept.
    fixed: bool,
}

impl NestLoopNode {
    fn new(
        kind: JoinKind,
        left: BoxNode,
        right_plan: Arc<Plan>,
        cond: Option<Expr>,
        params: Vec<Expr>,
        nleft: usize,
        nright: usize,
    ) -> Self {
        let fixed = matches!(kind, JoinKind::Right | JoinKind::Full)
            && !params.is_empty()
            && params.iter().all(|e| e.columns().is_empty());
        NestLoopNode {
            kind,
            left,
            right_plan,
            cond,
            params,
            nleft,
            nright,
            right: None,
            right_matched: Vec::new(),
            cur: None,
            unmatched_right: None,
            memo: None,
            fixed,
        }
    }

    fn right_rows(&mut self, left: &[Value], ctx: &ExecCtx) -> Result<Rc<Vec<Row>>> {
        if self.fixed {
            if self.right.is_none() {
                let p = eval_all(&self.params, &[], ctx)?;
                let rows = ctx.with_outer(Rc::new(p), || collect(&self.right_plan, ctx, None))?;
                self.right_matched = vec![false; rows.len()];
                self.right = Some(Rc::new(rows));
            }
            return Ok(self.right.clone().unwrap());
        }
        if !self.params.is_empty() {
            let memo_key = match &self.memo {
                Some(m) => Some(eval_all(&m.keys, left, ctx)?),
                None => None,
            };
            if let (Some(m), Some(k)) = (&self.memo, &memo_key)
                && let Some(rows) = m.cache.get(k)
            {
                let (n, key) = (rows.len() as u64, m.key);
                ctx.count(key, |st| {
                    st.loops += 1;
                    st.rows += n;
                    st.cache_hits += 1;
                });
                return Ok(rows.clone());
            }
            let p = eval_all(&self.params, left, ctx)?;
            let rows = ctx.with_outer(Rc::new(p), || collect(&self.right_plan, ctx, None))?;
            let rows = Rc::new(rows);
            if let (Some(m), Some(k)) = (&mut self.memo, memo_key) {
                // EMPTY_ENTRY_MEMORY_BYTES and CACHE_TUPLE_BYTES.
                let mut bytes = 0;
                if ctx.instrumented() {
                    bytes = (24 + 24 + minimal_len(&k, ctx)) as u64;
                    for r in rows.iter() {
                        bytes += (16 + minimal_len(r, ctx)) as u64;
                    }
                }
                let (n, key) = (rows.len() as u64, m.key);
                ctx.count(key, |st| {
                    st.loops += 1;
                    st.rows += n;
                    st.cache_misses += 1;
                    st.cache_bytes += bytes;
                });
                m.cache.insert(k, rows.clone());
            }
            return Ok(rows);
        }
        if self.right.is_none() {
            let rows = collect(&self.right_plan, ctx, None)?;
            self.right_matched = vec![false; rows.len()];
            self.right = Some(Rc::new(rows));
        }
        Ok(self.right.clone().unwrap())
    }
}

impl Node for NestLoopNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            if let Some(i) = self.unmatched_right.as_mut() {
                // RIGHT/FULL: right rows that matched nothing.
                let rows = self.right.clone().unwrap_or_default();
                while *i < rows.len() {
                    let j = *i;
                    *i += 1;
                    if !self.right_matched[j] {
                        let mut out = vec![Value::Null; self.nleft];
                        out.extend(rows[j].iter().cloned());
                        return Ok(Some(out));
                    }
                }
                return Ok(None);
            }
            if self.cur.is_none() {
                match self.left.next(ctx)? {
                    Some(l) => {
                        let rows = self.right_rows(&l, ctx)?;
                        self.cur = Some((l, rows, 0, false));
                    }
                    None => {
                        if matches!(self.kind, JoinKind::Right | JoinKind::Full) {
                            if self.right.is_none() {
                                let _ = self.right_rows(&[], ctx)?;
                            }
                            self.unmatched_right = Some(0);
                            continue;
                        }
                        return Ok(None);
                    }
                }
            }
            let (l, rows, i, matched) = self.cur.as_mut().unwrap();
            while *i < rows.len() {
                ctx.check()?;
                let j = *i;
                *i += 1;
                let combined = concat_rows(l, &rows[j]);
                let ok = match &self.cond {
                    Some(c) => eval_bool(c, &combined, ctx)?,
                    None => true,
                };
                if !ok {
                    continue;
                }
                *matched = true;
                if (self.params.is_empty() || self.fixed) && j < self.right_matched.len() {
                    self.right_matched[j] = true;
                }
                match self.kind {
                    JoinKind::Semi => {
                        let out = l.clone();
                        self.cur = None;
                        return Ok(Some(out));
                    }
                    JoinKind::Anti => {
                        self.cur = None;
                        break;
                    }
                    _ => return Ok(Some(combined)),
                }
            }
            let Some((l, _, _, matched)) = self.cur.take() else {
                continue;
            };
            if !matched {
                match self.kind {
                    JoinKind::Left | JoinKind::Full => {
                        let mut out = l;
                        out.extend(std::iter::repeat_n(Value::Null, self.nright));
                        return Ok(Some(out));
                    }
                    JoinKind::Anti => return Ok(Some(l)),
                    _ => {}
                }
            }
        }
    }
}

/// A hash join's inner rows in their hash table: the bucket chains Postgres builds
/// (pghash::JoinTable), each row's place in its chain, and the rows by key.
struct HashBuilt {
    rows: Vec<Row>,
    /// Rows by key, in their chain's order (the most recently inserted first).
    map: HashMap<Vec<Value>, Vec<usize>>,
    table: super::pghash::JoinTable,
}

/// The types hash keys hash as.
fn key_types(keys: &[Expr]) -> Vec<crate::types::DataType> {
    keys.iter().map(Expr::ty).collect()
}

/// A MinimalTuple's length for a row of values (the heap tuple's, less
/// MINIMAL_TUPLE_OFFSET).
fn minimal_len(row: &[Value], ctx: &ExecCtx) -> usize {
    let types: Vec<crate::types::DataType> = row.iter().map(value_type).collect();
    let fmt = ctx.fmt();
    let text = |v: &Value, ty: &crate::types::DataType| {
        crate::types::text::format_value(v, ty, fmt)
    };
    crate::catalog::relsize::heap_tuple_len(&types, row, &text).saturating_sub(8)
}

fn value_type(v: &Value) -> crate::types::DataType {
    use crate::types::DataType as T;
    match v {
        Value::Bool(_) => T::Bool,
        Value::Int2(_) => T::Int2,
        Value::Int4(_) => T::Int4,
        Value::Int8(_) => T::Int8,
        Value::Float4(_) => T::Float4,
        Value::Float8(_) => T::Float8,
        Value::Numeric(_) => T::NUMERIC,
        Value::Date(_) => T::Date,
        Value::Timestamp(_) => T::TIMESTAMP,
        Value::Timestamptz(_) => T::TIMESTAMPTZ,
        Value::Uuid(_) => T::Uuid,
        Value::Oid(_) => T::Oid,
        _ => T::Text,
    }
}

/// Build a hash join's table from `rows`: each row's keys hashed (ExecHashGetHashValue);
/// a row with a NULL key goes in (hashing as 0) only when unmatched inner rows are
/// returned (`keep_nulls`). `nbuckets`: the planner's bucket count (None: as many as
/// rows, at least 1024).
/// Hash join keys of the integer family compared across types (int4 = int8): as one
/// type, so equal values meet.
fn int_keys(k: Vec<Value>) -> Vec<Value> {
    k.into_iter()
        .map(|v| match v {
            Value::Int2(x) => Value::Int8(x as i64),
            Value::Int4(x) => Value::Int8(x as i64),
            v => v,
        })
        .collect()
}

fn build_hash(
    rows: Vec<Row>,
    keys: &[Expr],
    nbuckets: Option<u32>,
    keep_nulls: bool,
    ctx: &ExecCtx,
) -> Result<HashBuilt> {
    let types = key_types(keys);
    let cat = ctx.catalog();
    let mut tuples = Vec::with_capacity(rows.len());
    let mut by_key: Vec<(Vec<Value>, usize)> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let k = eval_all(keys, r, ctx)?;
        let null = k.iter().any(Value::is_null);
        if null && !keep_nulls {
            continue;
        }
        let h = super::pghash::combine(&k, &types, Some(&cat));
        tuples.push((i, h, 0));
        if !null {
            by_key.push((int_keys(k), i));
        }
    }
    let nb = nbuckets.unwrap_or_else(|| (tuples.len() as u32).max(1024));
    if tuples.len() > nb as usize + 1 {
        // The buckets get rebuilt: where the dense chunks break matters.
        for t in &mut tuples {
            t.2 = minimal_len(&rows[t.0], ctx);
        }
    }
    let table = super::pghash::JoinTable::build(nb, tuples);
    // Each row's place in its chain.
    let mut pos = vec![usize::MAX; rows.len()];
    for (p, i) in table.scan().enumerate() {
        pos[i] = p;
    }
    let mut map: HashMap<Vec<Value>, Vec<usize>> = HashMap::new();
    for (k, i) in by_key {
        map.entry(k).or_default().push(i);
    }
    for v in map.values_mut() {
        v.sort_by_key(|&i| pos[i]);
    }
    Ok(HashBuilt { rows, map, table })
}

/// Hash join on equality keys; the right side is the build side. A merge join runs this
/// way too (its matches in the inner side's order).
struct HashJoinNode {
    kind: JoinKind,
    left: BoxNode,
    right_plan: Arc<Plan>,
    lkeys: Vec<Expr>,
    rkeys: Vec<Expr>,
    cond: Option<Expr>,
    nleft: usize,
    nright: usize,
    table: Option<HashBuilt>,
    matched: Vec<bool>,
    cur: Option<(Row, Vec<usize>, usize, bool)>,
    unmatched_right: Option<std::vec::IntoIter<usize>>,
    nbuckets: Option<u32>,
    merge: bool,
    /// A merge join's key directions (`JoinMeta::merge_dirs`).
    merge_dirs: Vec<(bool, bool)>,
    /// A merge right / full join's rows.
    merged: std::vec::IntoIter<Row>,
    /// A merge join's first outer row, read before the inner side runs.
    first: Option<Row>,
}

impl HashJoinNode {
    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: JoinKind,
        left: BoxNode,
        right_plan: Arc<Plan>,
        lkeys: Vec<Expr>,
        rkeys: Vec<Expr>,
        cond: Option<Expr>,
        nleft: usize,
        nright: usize,
        meta: &crate::sql::plan::JoinMeta,
    ) -> Self {
        HashJoinNode {
            kind,
            left,
            right_plan,
            lkeys,
            rkeys,
            cond,
            nleft,
            nright,
            table: None,
            matched: Vec::new(),
            cur: None,
            unmatched_right: None,
            nbuckets: meta.hash_buckets,
            merge: meta.method == Some(crate::sql::plan::JoinMethod::Merge),
            merge_dirs: meta.merge_dirs.clone(),
            merged: Vec::new().into_iter(),
            first: None,
        }
    }

    fn build(&mut self, ctx: &ExecCtx) -> Result<()> {
        if self.table.is_some() {
            return Ok(());
        }
        let rows = collect(&self.right_plan, ctx, None)?;
        // (EXPLAIN ANALYZE: how the rows bunch on their keys, which batching can't split.)
        if let Some(instr) = ctx.instr.borrow().clone() {
            let mut counts: HashMap<String, u64> = HashMap::new();
            for r in &rows {
                let k = eval_all(&self.rkeys, r, ctx)?;
                *counts.entry(format!("{k:?}")).or_default() += 1;
            }
            let max = counts.values().copied().max().unwrap_or(0);
            let key = Arc::as_ptr(&self.right_plan) as usize;
            instr.update(key, |s| s.hash_max_group = s.hash_max_group.max(max));
        }
        let keep = matches!(self.kind, JoinKind::Right | JoinKind::Full);
        let mut built = build_hash(rows, &self.rkeys, self.nbuckets, keep, ctx)?;
        if self.merge {
            // A merge join meets equal inner rows in their (sorted) order.
            for v in built.map.values_mut() {
                v.sort_unstable();
            }
        }
        self.matched = vec![false; built.rows.len()];
        self.table = Some(built);
        Ok(())
    }
}

impl HashJoinNode {
    /// A Merge Right / Full Join (nodeMergejoin.c): both inputs sorted on the keys, walked
    /// together; an inner row nothing matched comes out when the walk passes it.
    fn merge_outer(&mut self, ctx: &ExecCtx) -> Result<Vec<Row>> {
        let mut outer = Vec::new();
        while let Some(r) = self.left.next(ctx)? {
            ctx.check()?;
            outer.push(r);
        }
        let inner = collect(&self.right_plan, ctx, None)?;
        // The keys' order: the inner input's sort, when it is a Sort (else ascending).
        let sort_keys: Vec<SortKey> = self
            .rkeys
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let found = match (&*self.right_plan, self.merge_dirs.get(i)) {
                    (_, Some(&(desc, nulls_first))) => Some(SortKey {
                        expr: k.clone(),
                        desc,
                        nulls_first,
                        using: None,
                    }),
                    (Plan::Sort { keys, .. }, None) => keys.iter().find(|s| s.expr.same_as(k)).cloned(),
                    _ => None,
                };
                found.unwrap_or(SortKey {
                    expr: k.clone(),
                    desc: false,
                    nulls_first: false,
                    using: None,
                })
            })
            .collect();
        let okeys: Vec<Vec<Value>> = outer
            .iter()
            .map(|r| eval_all(&self.lkeys, r, ctx).map(int_keys))
            .collect::<Result<_>>()?;
        let ikeys: Vec<Vec<Value>> = inner
            .iter()
            .map(|r| eval_all(&self.rkeys, r, ctx).map(int_keys))
            .collect::<Result<_>>()?;
        let cmp = |a: &[Value], b: &[Value]| -> Result<Ordering> {
            for (i, k) in sort_keys.iter().enumerate() {
                match compare_values(&a[i], &b[i], k, ctx)? {
                    Ordering::Equal => {}
                    o => return Ok(o),
                }
            }
            Ok(Ordering::Equal)
        };
        let null = |k: &[Value]| k.iter().any(Value::is_null);
        let fill_outer = self.kind == JoinKind::Full;
        let mut matched = vec![false; inner.len()];
        let mut out = Vec::new();
        let unmatched_inner = |r: &Row| {
            let mut o = vec![Value::Null; self.nleft];
            o.extend(r.iter().cloned());
            o
        };
        let lead_nulls = ikeys.iter().take_while(|k| null(k)).count();
        let (mut i, mut j) = (0, 0);
        while i < outer.len() {
            ctx.check()?;
            let ok = &okeys[i];
            if null(ok) {
                if fill_outer {
                    let mut o = outer[i].clone();
                    o.extend(std::iter::repeat_n(Value::Null, self.nright));
                    out.push(o);
                }
                i += 1;
                continue;
            }
            // (Inner rows whose keys are null come first under NULLS FIRST: passed over
            // before any match, as nodeMergejoin skips non-matchable inner tuples.)
            while j < inner.len()
                && ((j < lead_nulls) || (!null(&ikeys[j]) && cmp(ok, &ikeys[j])? == Ordering::Greater))
            {
                if !matched[j] {
                    out.push(unmatched_inner(&inner[j]));
                }
                j += 1;
            }
            let mut end = j;
            while end < inner.len() && !null(&ikeys[end]) && cmp(ok, &ikeys[end])? == Ordering::Equal {
                end += 1;
            }
            let mut any = false;
            for k in j..end {
                let combined = concat_rows(&outer[i], &inner[k]);
                if let Some(c) = &self.cond
                    && !eval_bool(c, &combined, ctx)?
                {
                    continue;
                }
                any = true;
                matched[k] = true;
                out.push(combined);
            }
            if !any && fill_outer {
                let mut o = outer[i].clone();
                o.extend(std::iter::repeat_n(Value::Null, self.nright));
                out.push(o);
            }
            i += 1;
        }
        for k in j..inner.len() {
            if !matched[k] {
                out.push(unmatched_inner(&inner[k]));
            }
        }
        Ok(out)
    }
}

impl Node for HashJoinNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.merge && matches!(self.kind, JoinKind::Right | JoinKind::Full) {
            if self.unmatched_right.is_none() {
                let rows = self.merge_outer(ctx)?;
                self.merged = rows.into_iter();
                self.unmatched_right = Some(Vec::new().into_iter());
            }
            return Ok(self.merged.next());
        }
        // ExecMergeJoin reads the outer side first: without rows, the inner side never
        // runs.
        if self.merge && self.table.is_none() {
            let Some(l) = self.left.next(ctx)? else {
                return Ok(None);
            };
            self.first = Some(l);
        }
        self.build(ctx)?;
        loop {
            if let Some(it) = self.unmatched_right.as_mut() {
                let rows = &self.table.as_ref().unwrap().rows;
                for j in it.by_ref() {
                    if !self.matched[j] {
                        let mut out = vec![Value::Null; self.nleft];
                        out.extend(rows[j].iter().cloned());
                        return Ok(Some(out));
                    }
                }
                return Ok(None);
            }
            if self.cur.is_none() {
                let next = match self.first.take() {
                    Some(r) => Some(r),
                    None => self.left.next(ctx)?,
                };
                match next {
                    Some(l) => {
                        ctx.check()?;
                        let k = int_keys(eval_all(&self.lkeys, &l, ctx)?);
                        let cands = if k.iter().any(Value::is_null) {
                            Vec::new()
                        } else {
                            self.table
                                .as_ref()
                                .unwrap()
                                .map
                                .get(&k)
                                .cloned()
                                .unwrap_or_default()
                        };
                        self.cur = Some((l, cands, 0, false));
                    }
                    None => {
                        if matches!(self.kind, JoinKind::Right | JoinKind::Full) {
                            // ExecScanHashTableForUnmatched: bucket by bucket (a merge
                            // join: in the inner side's order).
                            let t = self.table.as_ref().unwrap();
                            let order: Vec<usize> = if self.merge {
                                (0..t.rows.len()).collect()
                            } else {
                                t.table.scan().collect()
                            };
                            self.unmatched_right = Some(order.into_iter());
                            continue;
                        }
                        return Ok(None);
                    }
                }
            }
            let (l, cands, i, matched) = self.cur.as_mut().unwrap();
            while *i < cands.len() {
                let j = cands[*i];
                *i += 1;
                let rows = &self.table.as_ref().unwrap().rows;
                let combined = concat_rows(l, &rows[j]);
                if let Some(c) = &self.cond
                    && !eval_bool(c, &combined, ctx)?
                {
                    continue;
                }
                *matched = true;
                self.matched[j] = true;
                match self.kind {
                    JoinKind::Semi => {
                        let out = l.clone();
                        self.cur = None;
                        return Ok(Some(out));
                    }
                    JoinKind::Anti => {
                        self.cur = None;
                        break;
                    }
                    _ => return Ok(Some(combined)),
                }
            }
            let Some((l, _, _, matched)) = self.cur.take() else {
                continue;
            };
            if !matched {
                match self.kind {
                    JoinKind::Left | JoinKind::Full => {
                        let mut out = l;
                        out.extend(std::iter::repeat_n(Value::Null, self.nright));
                        return Ok(Some(out));
                    }
                    JoinKind::Anti => return Ok(Some(l)),
                    _ => {}
                }
            }
        }
    }
}

/// Hash Right Semi / Right Anti Join: the left rows hashed, the right rows probing. A
/// semi join puts a left row out at its first match, as the probes find it; an anti join
/// puts out the left rows nothing matched once the probes are done, bucket by bucket.
struct RightSemiNode {
    left: Arc<Plan>,
    right: Arc<Plan>,
    lkeys: Vec<Expr>,
    rkeys: Vec<Expr>,
    cond: Option<Expr>,
    out: Option<std::vec::IntoIter<Row>>,
    anti: bool,
    nbuckets: Option<u32>,
}

impl RightSemiNode {
    fn run(&self, ctx: &ExecCtx) -> Result<Vec<Row>> {
        let rows = collect(&self.left, ctx, None)?;
        let built = build_hash(rows, &self.lkeys, self.nbuckets, self.anti, ctx)?;
        let rows = &built.rows;
        let mut emitted = vec![false; rows.len()];
        let mut out = Vec::new();
        let mut right = open(&self.right, ctx)?;
        while let Some(r) = right.next(ctx)? {
            ctx.check()?;
            let k = int_keys(eval_all(&self.rkeys, &r, ctx)?);
            if k.iter().any(Value::is_null) {
                continue;
            }
            let Some(cands) = built.map.get(&k) else {
                continue;
            };
            for &j in cands {
                if emitted[j] {
                    continue;
                }
                if let Some(c) = &self.cond
                    && !eval_bool(c, &concat_rows(&rows[j], &r), ctx)?
                {
                    continue;
                }
                emitted[j] = true;
                if !self.anti {
                    out.push(rows[j].clone());
                }
            }
        }
        if self.anti {
            for j in built.table.scan() {
                if !emitted[j] {
                    out.push(rows[j].clone());
                }
            }
        }
        Ok(out)
    }
}

impl Node for RightSemiNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.out.is_none() {
            self.out = Some(self.run(ctx)?.into_iter());
        }
        Ok(self.out.as_mut().and_then(Iterator::next))
    }
}

/// A CTE's rows, produced on demand and shared by its readers.
pub struct CteState {
    /// Rows not produced yet come from here; `None` once exhausted (or computed up front).
    node: Option<BoxNode>,
    rows: Vec<Row>,
    /// A data-modifying CTE not started yet.
    dml: Option<Arc<Plan>>,
}

impl CteState {
    /// Start a data-modifying CTE's statement, if it hasn't been.
    fn start(&mut self, ctx: &ExecCtx) -> Result<()> {
        if let Some(p) = self.dml.take() {
            // (Not a member of an inheritance DML opening now.)
            let deferred = ctx.defer_after.replace(false);
            let capture = ctx.capture.replace(false);
            let node = open(&p, ctx);
            ctx.defer_after.set(deferred);
            ctx.capture.set(capture);
            self.node = Some(Box::new(Uncounted { input: node? }));
        }
        Ok(())
    }
}

/// A data-modifying CTE's statement: its rows don't count toward the statement's tag.
struct Uncounted {
    input: BoxNode,
}

impl Node for Uncounted {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let processed = ctx.processed.get();
        let r = self.input.next(ctx);
        ctx.processed.set(processed);
        r
    }
}

/// A statement with data-modifying CTEs: after its main query, those not run to their end
/// are (ExecPostprocessPlan).
struct WithNode {
    body: BoxNode,
    dml: Vec<Rc<RefCell<CteState>>>,
    /// It holds the statement's AFTER trigger events until its end.
    queues: bool,
}

impl WithNode {
    fn run(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if let Some(r) = self.body.next(ctx)? {
            return Ok(Some(r));
        }
        for st in std::mem::take(&mut self.dml).into_iter().rev() {
            let mut st = st.borrow_mut();
            st.start(ctx)?;
            if let Some(mut node) = st.node.take() {
                while node.next(ctx)?.is_some() {
                    ctx.check()?;
                }
            }
        }
        Ok(None)
    }
}

impl Node for WithNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let r = self.run(ctx);
        if self.queues && !matches!(r, Ok(Some(_))) {
            self.queues = false;
            ctx.after_running.set(false);
            if r.is_err() {
                ctx.after_queue.borrow_mut().clear();
                return r;
            }
            super::trigger::run_queue(ctx)?;
        }
        r
    }

    fn end(&mut self, ctx: &ExecCtx, stmt: bool) -> Result<()> {
        self.body.end(ctx, stmt)
    }

    fn dml_table(&self) -> Option<Arc<crate::catalog::Table>> {
        self.body.dml_table()
    }

    fn captures(&self) -> bool {
        self.body.captures()
    }

    fn take_captured(&mut self) -> Vec<super::trigger::AfterRow> {
        self.body.take_captured()
    }

    fn add_captured(&mut self, rows: Vec<super::trigger::AfterRow>) {
        self.body.add_captured(rows)
    }
}

struct CteScanNode {
    state: Rc<RefCell<CteState>>,
    i: usize,
}

impl Node for CteScanNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let mut st = self.state.borrow_mut();
        if self.i >= st.rows.len() {
            st.start(ctx)?;
            let Some(node) = st.node.as_mut() else {
                return Ok(None);
            };
            match node.next(ctx)? {
                Some(r) => st.rows.push(r),
                None => {
                    st.node = None;
                    return Ok(None);
                }
            }
        }
        self.i += 1;
        Ok(Some(st.rows[self.i - 1].clone()))
    }
}

/// WITH RECURSIVE: the base rows, then one iteration of the recursive term at a time over the
/// previous iteration's rows, until an iteration adds nothing.
struct RecursiveNode {
    id: usize,
    base: Option<BoxNode>,
    recursive: Arc<Plan>,
    all: bool,
    /// UNION (not ALL): rows already returned.
    seen: HashMap<Row, ()>,
    /// The next iteration's working table.
    working: Vec<Row>,
    /// The current iteration's recursive term, read a row at a time (so a LIMIT above stops
    /// even an iteration that never ends).
    produced: Option<BoxNode>,
}

impl RecursiveNode {
    /// Keep a new row (UNION drops duplicates); it joins the next working table.
    fn keep(&mut self, r: Row) -> Option<Row> {
        if !self.all {
            if self.seen.contains_key(&r) {
                return None;
            }
            self.seen.insert(r.clone(), ());
        }
        self.working.push(r.clone());
        Some(r)
    }
}

impl Node for RecursiveNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            if let Some(base) = self.base.as_mut() {
                match base.next(ctx)? {
                    Some(r) => match self.keep(r) {
                        Some(r) => return Ok(Some(r)),
                        None => continue,
                    },
                    None => self.base = None,
                }
                continue;
            }
            if let Some(p) = self.produced.as_mut() {
                match p.next(ctx)? {
                    Some(r) => match self.keep(r) {
                        Some(r) => return Ok(Some(r)),
                        None => continue,
                    },
                    None => self.produced = None,
                }
            }
            if self.working.is_empty() {
                ctx.work.borrow_mut().remove(&self.id);
                return Ok(None);
            }
            ctx.txn.borrow().check_cancel()?;
            if let Some(d) = ctx.deadline
                && std::time::Instant::now() > d
            {
                return Err(Error::new(
                    code::QUERY_CANCELED,
                    "canceling statement due to statement timeout",
                ));
            }
            ctx.work
                .borrow_mut()
                .insert(self.id, Rc::new(std::mem::take(&mut self.working)));
            self.produced = Some(open(&self.recursive, ctx)?);
        }
    }
}

/// Runs its input inside a frame (`Plan::Context`).
struct ContextNode {
    input: BoxNode,
    frame: crate::error::Frame,
    /// The snapshot of a volatile function's statement.
    snap: Option<Rc<crate::txn::Snap>>,
}

impl Node for ContextNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let input = &mut self.input;
        let frame = &self.frame;
        let mut run = || ctx.sess.framed(frame.clone(), |_| input.next(ctx));
        // The rows a SQL function's statement writes aren't the calling statement's.
        let processed = ctx.processed.get();
        let r = match &self.snap {
            Some(s) => ctx.with_snapshot(s.clone(), run),
            None => run(),
        };
        ctx.processed.set(processed);
        r
    }
}
