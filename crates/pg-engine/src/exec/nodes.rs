//! Plan nodes other than scans, aggregation, windows and DML.

use super::{BoxNode, ExecCtx, Node, Row, collect, eval_all};
use crate::error::{Error, Result, code};
use crate::sql::expr::{Expr, eval, eval_bool};
use crate::sql::func::{FnCtx, SrfIter, call2};
use crate::sql::plan::{JoinKind, Plan, SetOpKind, SortKey, SrfCall};
use crate::types::Value;
use std::cell::RefCell;
use std::cmp::Ordering;
use super::fasthash::HashMap;
use std::rc::Rc;
use std::sync::Arc;

/// A scan of a system catalog (not a system view) the statement runs: the catalog's
/// sequential scan or its index's scan in pg_stat_sys_tables / _indexes, as a scan of the
/// relation counts. (The engine's own catalog lookups read no relation: not counted.)
fn count_catalog_scan(table: crate::catalog::system::SysTable, path: Option<&crate::sql::plan::SysPath>, ctx: &ExecCtx) {
    use crate::sql::plan::Access;
    if table.is_view() {
        return;
    }
    match path.and_then(|p| p.scan.index.as_ref().filter(|_| !matches!(p.scan.access, Access::Seq))) {
        Some((ix, _)) => super::pgstat::count_index_scan(ctx, ix.oid),
        None => super::pgstat::count_seqscan(ctx, table.oid()),
    }
}

/// Start running `plan`.
pub fn open(plan: &Arc<Plan>, ctx: &ExecCtx) -> Result<BoxNode> {
    // (Taken first: a mask is for this node alone, whatever happens next.)
    let mut need = ctx.need.take();
    // (Under EXPLAIN ANALYZE a join or a window is told what its parent reads all the
    // same: the Sorts below count their tuples by it.)
    if need.is_none() && matches!(**plan, Plan::Join { .. } | Plan::Project { .. } | Plan::Filter { .. } | Plan::Window { .. }) {
        need = ctx.logical_need.take();
    }
    let mut node = crate::stack::guard(|| open_node(plan, need.as_deref(), ctx))?;
    if !ctx.worker_parts.borrow().is_empty() && ctx.worker_parts.borrow().contains(&(Arc::as_ptr(plan) as usize)) {
        node = super::parallel_part::WorkerPart::wrap(node);
    }
    Ok(match &*ctx.instr.borrow() {
        Some(i) => super::InstrNode::wrap(node, Arc::as_ptr(plan) as usize, i.clone()),
        None => node,
    })
}

/// Open `plan`, whose rows' columns outside `need` nothing reads (None: all are read).
pub fn open_need(plan: &Arc<Plan>, need: Option<Vec<bool>>, ctx: &ExecCtx) -> Result<BoxNode> {
    if !ctx.instrumented() {
        ctx.need.set(need.map(Rc::from));
    } else if matches!(**plan, Plan::Sort { .. } | Plan::Join { .. } | Plan::Project { .. } | Plan::Filter { .. } | Plan::Window { .. }) {
        ctx.logical_need.set(need.map(Rc::from));
    }
    open(plan, ctx)
}

/// An UPDATE's node: a postgres_fdw statement sent whole, or ModifyTable's. (Out of
/// `open_node`, whose frame is on every plan level's stack.)
#[inline(never)]
fn open_update(plan: &Arc<Plan>, p: &crate::sql::plan::UpdatePlan, ctx: &ExecCtx) -> Result<BoxNode> {
    match crate::ext::postgres_fdw::direct::open_update(p, ctx)? {
        Some(n) => Ok(n),
        None => Ok(Box::new(super::dml::UpdateNode::new(super::planref::update(plan).expect("an UPDATE"), ctx)?)),
    }
}

/// A DELETE's node: a postgres_fdw statement sent whole, or ModifyTable's.
#[inline(never)]
fn open_delete(plan: &Arc<Plan>, p: &crate::sql::plan::DeletePlan, ctx: &ExecCtx) -> Result<BoxNode> {
    match crate::ext::postgres_fdw::direct::open_delete(p, ctx)? {
        Some(n) => Ok(n),
        None => Ok(Box::new(super::dml::DeleteNode::new(super::planref::delete(plan).expect("a DELETE"), ctx)?)),
    }
}

/// An Append's node. (Out of `open_node`, whose frame is on every plan level's stack.)
#[inline(never)]
fn open_append(inputs: &[Arc<Plan>], inh: bool, ctx: &ExecCtx) -> BoxNode {
    let dml = inh && inputs.first().is_some_and(|p| matches!(&**p, Plan::Update(_) | Plan::Delete(_)));
    // (A UNION ALL's members that are partitions are pruned too: make_partition_pruneinfo
    // for an appendrel of several partitioned tables.)
    let inputs = if (inh && !dml) || (!inh && !inputs.iter().any(|p| p.writes())) {
        runtime_pruned(inputs, crate::sql::binder::partition::Known::Exec, ctx)
    } else {
        inputs.to_vec()
    };
    // The statements a rule made of one: separate commands.
    let commands = !inh && inputs.iter().any(|p| p.writes());
    // (Async Foreign Scans: the synchronous members' rows come first.)
    let (inputs, first_async) = if dml || commands { (inputs, None) } else { crate::ext::postgres_fdw::scan::async_order(inputs, ctx) };
    Box::new(AppendNode {
        inputs,
        i: 0,
        cur: None,
        first_async,
        pre: Vec::new(),
        dml,
        done: Vec::new(),
        capture: false,
        commands,
    })
}

fn open_node(plan: &Arc<Plan>, need: Option<&[bool]>, ctx: &ExecCtx) -> Result<BoxNode> {
    use super::need as nd;
    Ok(match &**plan {
        Plan::One => Box::new(VecNode::new(vec![Vec::new()])),
        Plan::Empty { .. } => Box::new(VecNode::new(Vec::new())),
        Plan::Values { rows, .. } => Box::new(ValuesNode {
            rows: rows.clone(),
            i: 0,
        }),
        Plan::Scan(s) => super::scan::open_scan(s, Some(plan), need, ctx)?,
        Plan::SysScan { mark: Some(_), .. } => open_locked_sys_scan(plan, need, ctx)?,
        Plan::SysScan { table, filter, path, .. } => {
            count_catalog_scan(*table, path.as_deref(), ctx);
            if let Some(rows) = sys_rescan(*table, filter.as_ref(), path.as_deref(), need, ctx)? {
                return Ok(Box::new(VecNode::new(rows)));
            }
            if let Some(node) = sys_shared_scan(*table, filter.as_ref(), path.as_deref(), need, ctx)? {
                return Ok(node);
            }
            // (A statistics view makes only the columns read above or by the filter:
            // `count(*)` of one fetched every relation's statistics, `stat_views::rows_of`.)
            let reads = stat_reads(*table, filter.as_ref(), need);
            let mut rows = match is_named_rows(*table, filter.as_ref(), reads.as_deref(), ctx)? {
                Some(r) => r,
                None if let Some(r) = sys_obj_rows(*table, filter.as_ref(), path.as_deref(), ctx)? => r,
                None if reads.is_some() => {
                    let sel = crate::catalog::stat_views::StatSel { reads, ..Default::default() };
                    crate::catalog::stat_views::rows_of(*table, ctx, &sel)?
                }
                None => crate::catalog::system::rows(*table, ctx)?,
            };
            // An index scan reads only the rows its conditions select, and checks the filter
            // on those alone (a LIKE's exact prefix as `relname = 'a'`: the pattern `a\` isn't
            // matched against other names; pgjdbc's getTables).
            if let Some(sp) = path.as_deref() {
                rows = sys_index_prefilter(rows, sp, ctx)?;
            }
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
            scan,
        } => {
            let vals = eval_all(args, &[], ctx)?;
            let _frame = scan.then(|| crate::stack::FnFrame::hold(crate::stack::PG_FUNCSCAN_FRAME));
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
        // (A subquery read by one of its sorted paths: its rows in that path's order, as
        // its sorted grouping or Unique returns them.)
        Plan::Barrier { input, path: Some(sp), .. } => Box::new(SortNode {
            input: Some(open(input, ctx)?),
            keys: sp.keys.clone(),
            out: Default::default(),
            types: crate::sql::paths::analyzejoins::col_types(input),
            mask: None,
            bound: None,
            presorted: 0,
            key: 0,
            need: None,
            narrow: None,
        }),
        Plan::Barrier { input, .. } => return open(input, ctx),
        Plan::Context {
            input,
            frame,
            perms,
            volatile,
            scan,
            custom,
        } => {
            // A volatile SQL function's statement runs as a command of its own.
            let snap = volatile.then(|| ctx.fresh_snapshot());
            let pg_frame = sql_fn_frame(*scan);
            // auto_explain.log_nested_statements: the statement is an executor run of its
            // own in PostgreSQL (fmgr_sql's), its plan logged when it ends.
            let mut explain = if crate::log::logs_nested() {
                crate::ext::auto_explain::start(&ctx.sess.settings.borrow(), true).map(|run| {
                    let instr = run.opts.analyze.then(|| Rc::new(super::Instr::default()));
                    (run, instr)
                })
            } else {
                None
            };
            let outer_instr = explain.as_ref().map(|(_, i)| ctx.instr.replace(i.clone()));
            let mut ran = None;
            let mut run = || {
                ctx.sess.framed(frame.clone(), |_| {
                    let _frame = crate::stack::FnFrame::enter(pg_frame)?;
                    // (Planned per call: a custom plan, or the generic one bound here.)
                    let planned = match custom {
                        Some(c) => crate::plpgsql::sql_fn_context_plan(ctx, c, input)?,
                        None => None,
                    };
                    if let Some(p) = perms {
                        crate::sql::binder::perms::check(p, ctx)?;
                        crate::sql::binder::perms::check_row_security(p, ctx)?;
                    }
                    let plan = planned.unwrap_or_else(|| input.clone());
                    let node = open(&plan, ctx);
                    ran = Some(plan);
                    node
                })
            };
            let opened = match &snap {
                Some(s) => ctx.with_snapshot(s.clone(), run),
                None => run(),
            };
            if let Some(outer) = outer_instr {
                *ctx.instr.borrow_mut() = outer;
            }
            let input = opened?;
            let explain = match (explain.take(), ran) {
                (Some((run, instr)), Some(plan)) => {
                    let params = custom.as_ref().map_or_else(Vec::new, |c| {
                        (0..c.arg_types.len())
                            .map(|i| match ctx.outer_value(1, i) {
                                Ok(Value::Null) | Err(_) => None,
                                Ok(v) => Some(crate::types::text::format_value(&v, &c.arg_types[i], ctx.fmt())),
                            })
                            .collect()
                    });
                    let query = match frame {
                        crate::error::Frame::SqlFn { src, .. } => src.clone(),
                        _ => Arc::from(""),
                    };
                    let scalar = custom.as_ref().is_some_and(|c| matches!(c.place, crate::sql::plan::SqlFnPlace::Scalar));
                    let started = std::time::Instant::now();
                    Some(Box::new(NestedExplain { run, instr, plan, params, query, scalar, started }))
                }
                _ => None,
            };
            Box::new(ContextNode {
                input,
                frame: frame.clone(),
                snap,
                pg_frame,
                explain,
            })
        }
        // create_gating_plan: the quals that read no column of the row (pseudoconstant) run
        // once, before the input does; when one fails, the input is never run (a recursive
        // function's `WHERE n > 0` ends its recursion).
        Plan::Filter { input, pred } if gating(pred) && !ctx.instrumented() => Box::new(GateNode {
            plan: input.clone(),
            need: nd::add(need.map(<[bool]>::to_vec), [pred]),
            gates: gates_of(pred),
            filter: rest_of(pred),
            input: None,
        }),
        // (An inner join's: the join runs with the others. Not under EXPLAIN ANALYZE, which
        // counts the join's rows by its plan node.)
        Plan::Join {
            kind: JoinKind::Inner,
            cond: Some(c),
            ..
        } if gating(c) && !ctx.instrumented() => {
            let gates = gates_of(c);
            let mut join = (**plan).clone();
            if let Plan::Join { cond, .. } = &mut join {
                *cond = rest_of(c);
            }
            Box::new(GateNode {
                plan: Arc::new(join),
                need: need.map(<[bool]>::to_vec),
                gates,
                filter: None,
                input: None,
            })
        }
        Plan::Filter { input, pred } => Box::new(FilterNode {
            input: open_need(input, nd::add(need.map(<[bool]>::to_vec), [pred]), ctx)?,
            pred: pred.clone(),
        }),
        Plan::Project { input, exprs } => Box::new(ProjectNode {
            // (An output nothing reads that is a plain column needs no input column: it
            // evaluates to whatever is there, without effects. Every other is evaluated as
            // ever, its columns read.)
            input: open_need(
                input,
                match need {
                    Some(m) => nd::of(
                        exprs
                            .iter()
                            .enumerate()
                            .filter(|(i, e)| m.get(*i).copied().unwrap_or(false) || !matches!(e, Expr::Col(..)))
                            .map(|(_, e)| e),
                    ),
                    None => nd::of(exprs),
                },
                ctx,
            )?,
            exprs: exprs.clone(),
            computed: None,
            tried: false,
            topk: None,
            moves: None,
            piped: false,
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
            super::agg::AggNode::new(
                {
                    let need = (|| {
                        let mut ex: Vec<&Expr> = groups.iter().collect();
                        for a in aggs {
                            ex.extend(nd::agg_exprs(a)?);
                        }
                        nd::of(ex)
                    })();
                    open_need(input, need, ctx)?
                },
                groups.clone(),
                sets.clone(),
                aggs.clone(),
            )
                .planned(plan.clone()),
        ),
        Plan::Window { input, calls } => {
            // (Its rows are the input's, then the calls' results.)
            let need = (|| {
                let mut ex: Vec<&Expr> = Vec::new();
                for c in calls {
                    ex.extend(c.args.iter().chain(&c.partition_by).chain(nd::keys(&c.order_by)));
                    if let crate::sql::plan::WinFunc::Agg(a) = &c.func {
                        ex.extend(nd::agg_exprs(a)?);
                    }
                }
                nd::add(need.map(<[bool]>::to_vec), ex)
            })();
            // (Over a table scan, the Sort PostgreSQL has below its WindowAgg runs as one,
            // within work_mem, and the window reads it a partition at a time.)
            let over_scan = {
                let mut p: &Plan = input;
                loop {
                    match p {
                        Plan::Project { input, .. } | Plan::Filter { input, .. } => p = input,
                        Plan::Scan(s) => break s.index.is_none() && !matches!(s.access, crate::sql::plan::Access::Index { .. }),
                        _ => break false,
                    }
                }
            };
            if over_scan && let Some(keys) = super::window::sort_keys_for(calls, ctx) {
                let sort = Arc::new(Plan::Sort { input: input.clone(), keys: keys.clone() });
                let node = super::window::WindowNode::new(open_need(&sort, need.clone(), ctx)?, calls.clone());
                return Ok(Box::new(node.presorted(&keys).holding(sort).counted(plan, input, need, ctx)));
            }
            let node = super::window::WindowNode::new(open_need(input, need.clone(), ctx)?, calls.clone()).counted(plan, input, need, ctx);
            Box::new(match &**input {
                Plan::Sort { keys, .. } => node.presorted(keys),
                _ => node,
            })
        }
        Plan::Sort { input, keys } => {
            let bound = ctx.sort_bound.take();
            if let Some(d) = distinct_presort(plan, ctx) {
                return Ok(Box::new(SortNode {
                    presorted: 0,
                    key: Arc::as_ptr(plan) as usize,
                    input: Some(d),
                    keys: keys.clone(),
                    out: Default::default(),
                    types: crate::sql::paths::analyzejoins::col_types(input),
                    mask: None,
                    bound,
                    need: None,
                    narrow: None,
                }));
            }
            let need = nd::add(need.map(<[bool]>::to_vec), nd::keys(keys));
            Box::new(SortNode {
                presorted: super::pghash::presorted_keys(plan, ctx).min(keys.len()),
                key: Arc::as_ptr(plan) as usize,
                input: Some(open_need(input, need.clone(), ctx)?),
                keys: keys.clone(),
                out: Default::default(),
                types: crate::sql::paths::analyzejoins::col_types(input),
                // (Under EXPLAIN ANALYZE, which reads whole rows, the mask the parent meant.)
                mask: {
                    let logical = ctx.logical_need.take();
                    need.clone().or_else(|| nd::add(logical.as_deref().map(<[bool]>::to_vec), nd::keys(keys)))
                },
                bound,
                need: need.map(Box::from),
                narrow: None,
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
                let need = match with_ties {
                    Some(k) => nd::add(need.map(<[bool]>::to_vec), nd::keys(k)),
                    None => need.map(<[bool]>::to_vec),
                };
                let n = open_need(input, need, ctx);
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
        Plan::Distinct { input, on, hashed, .. } => Box::new(DistinctNode {
            input: open(input, ctx)?,
            on: on.clone(),
            seen: HashMap::default(),
            prev: None,
            hashed: (on.is_none() || *hashed).then(|| plan.clone()),
            out: None,
            presort: None,
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
        Plan::Append { inputs, inh, .. } => open_append(inputs, inh.is_some(), ctx),
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
                    est: meta.hash_inner,
                })
            } else if let Some((lk, rk)) = hash
                && params.is_empty()
            {
                Box::new(HashJoinNode::new(
                    *kind,
                    open_need(left, nd::join_left(need, *nleft, lk, cond.as_ref()), ctx)?,
                    left.clone(),
                    right.clone(),
                    lk.clone(),
                    rk.clone(),
                    cond.clone(),
                    *nleft,
                    *nright,
                    meta,
                    need,
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
                let memo_keys = meta.memoize.as_ref().map(|m| &m.keys[..]).unwrap_or(&[]);
                let lneed = nd::join_left(need, *nleft, params.iter().chain(memo_keys), cond.as_ref());
                let mut nl = NestLoopNode::new(
                    *kind,
                    open_need(left, lneed, ctx)?,
                    right.clone(),
                    cond.clone(),
                    params.clone(),
                    *nleft,
                    *nright,
                );
                nl.key = Arc::as_ptr(plan) as usize;
                nl.keep = join_keep(need, cond.as_ref(), *nleft + *nright);
                if let Some(m) = meta.memoize.as_ref().filter(|_| !params.is_empty()) {
                    nl.memo = Some(Memoize {
                        keys: m.keys.clone(),
                        key: Arc::as_ptr(plan) as usize + 1,
                        cache: HashMap::default(),
                        binary: m.binary,
                        lru: Default::default(),
                        stamp: 0,
                        mem: 0,
                        limit: super::pghash::hash_mem(ctx) as u64,
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
                        store: CteState::store(p, ctx),
                        dml: Some(p.clone()),
                    }));
                    dml.push(st.clone());
                    st
                } else {
                    Rc::new(RefCell::new(CteState {
                        node: Some(open(p, ctx)?),
                        store: CteState::store(p, ctx),
                        dml: None,
                    }))
                };
                ctx.temp_cleanup.register_cte(&state);
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
            let instr = ctx.instr.borrow().clone();
            let slot = state.borrow_mut().store.reader();
            Box::new(CteScanNode { state, slot, instr, id: *id })
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
            seen: HashMap::default(),
            working: Vec::new(),
            produced: None,
            acct: ctx.instr.borrow().clone().map(|instr| {
                let allowed = super::workmem::work_mem(ctx);
                Box::new(RecursiveAcct {
                    key: Arc::as_ptr(plan) as usize,
                    instr,
                    types: crate::sql::paths::analyzejoins::col_types(base),
                    tables: [super::workmem::StoreMem::new(allowed, false), super::workmem::StoreMem::new(allowed, false)],
                    held: [Vec::new(), Vec::new()],
                    started: false,
                })
            }),
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
        Plan::LockRows {
            input,
            marks,
            width,
        } => {
            // (Row marks: ExecutorStart takes es_output_cid.)
            ctx.txn.borrow_mut().output_cid();
            Box::new(LockRowsNode {
                node: open(input, ctx)?,
                plan: input.clone(),
                marks: marks.clone(),
                width: *width,
            })
        }
        Plan::Insert(_) => Box::new(super::dml::InsertNode::new(super::planref::insert(plan).expect("an INSERT"), ctx)?),
        Plan::Update(p) => open_update(plan, p, ctx)?,
        Plan::Delete(p) => open_delete(plan, p, ctx)?,
        Plan::Merge(p) => Box::new(super::merge::MergeNode::new(p.clone(), ctx)?),
        Plan::Foreign(f) => crate::ext::fdw::open(f, ctx)?,
    })
}

/// ExecLockRows: each row the level returns has its locked relations' rows locked (in row
/// mark order); a row whose lock is skipped (SKIP LOCKED, deleted concurrently) goes, and
/// one a concurrent update changed is rechecked (EvalPlanQual): the level's plan run again
/// over the newest version of the locked rows and the same rows of the others, giving the
/// row to return, or none when it no longer qualifies.
struct LockRowsNode {
    node: BoxNode,
    plan: Arc<Plan>,
    marks: Arc<Vec<crate::sql::plan::LockMark>>,
    width: usize,
}

/// A row mark's test row for EvalPlanQual: its key, and its newest version when the lock
/// found one (else the version the statement read).
type EpqTest = Option<(Vec<u8>, Option<bytes::Bytes>)>;

fn key_table(key: &[u8]) -> u32 {
    key.get(..4).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

impl LockRowsNode {
    fn recheck(&self, tests: Vec<EpqTest>, ctx: &ExecCtx) -> Result<Option<Row>> {
        let groups = crate::sql::paths::level_scans(&self.plan);
        if groups.len() != tests.len() {
            return Err(crate::error::internal("LockRows: row marks and scans differ"));
        }
        let snap = ctx.snapshot();
        let mut rows = super::EpqRows::default();
        for (scans, test) in groups.iter().zip(tests) {
            let test = match test {
                None => None,
                Some((k, Some(b))) => Some((k, b)),
                // (A foreign table's mark is a copy of its row, ROW_MARK_COPY: the key holds
                // it, `scan::copy_key`; nothing to read.)
                Some((k, None)) if scans.iter().any(|s| s.table.foreign.is_some() && s.table.oid == key_table(&k)) => {
                    Some((k, bytes::Bytes::new()))
                }
                Some((k, None)) => {
                    let b = match snap.as_deref() {
                        Some(s) => ctx.txn.borrow().get_snap(&k, s)?,
                        None => ctx.txn.borrow().get(&k)?,
                    };
                    b.map(|b| (k, b))
                }
            };
            for s in scans {
                let mine = test.clone().filter(|(k, _)| key_table(k) == s.table.oid);
                rows.insert(*s as *const crate::sql::plan::ScanPlan as usize, mine);
            }
        }
        let prev = ctx.epq.replace(Some(Rc::new(rows)));
        // (Not counted in EXPLAIN ANALYZE's numbers.)
        let instr = ctx.instr.borrow_mut().take();
        let r = open(&self.plan, ctx).and_then(|mut n| n.next(ctx));
        *ctx.instr.borrow_mut() = instr;
        *ctx.epq.borrow_mut() = prev;
        r
    }
}

impl Node for LockRowsNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        use super::scan::{Locked, Xltw, lock_row};
        'rows: loop {
            let Some(row) = self.node.next(ctx)? else {
                return Ok(None);
            };
            let mut tests: Vec<EpqTest> = Vec::with_capacity(self.marks.len());
            let mut changed = false;
            for m in self.marks.iter() {
                let key = match row.get(m.col) {
                    Some(Value::Bytea(k)) => k.to_vec(),
                    // (The nullable side of an outer join: no row to lock.)
                    _ => {
                        tests.push(None);
                        continue;
                    }
                };
                let Some((mark, wait)) = m.lock else {
                    tests.push(Some((key, None)));
                    continue;
                };
                let oid = key_table(&key);
                let Some(t) = m.tables.iter().find(|t| t.oid == oid).or(m.tables.first()) else {
                    tests.push(Some((key, None)));
                    continue;
                };
                // (A foreign table's row: locked by the scan's remote query.)
                if t.foreign.is_some() {
                    tests.push(Some((key, None)));
                    continue;
                }
                match lock_row(t, &key, mark, wait, Xltw::Lock, ctx)? {
                    Locked::Skip => continue 'rows,
                    Locked::Same => tests.push(Some((key, None))),
                    Locked::Newer(b) => {
                        changed = true;
                        tests.push(Some((key, Some(b))));
                    }
                }
            }
            let mut row = if changed {
                match self.recheck(tests, ctx)? {
                    Some(r) => r,
                    None => continue,
                }
            } else {
                row
            };
            row.truncate(self.width);
            return Ok(Some(row));
        }
    }
}

/// SELECT ... FOR UPDATE / SHARE over a catalog: its scan under a `CatalogLockNode`. (Out of
/// line: `open_node` recurses once per plan node, and an unoptimized build gives every
/// arm's locals room in its frame.)
#[inline(never)]
fn open_locked_sys_scan(plan: &Arc<Plan>, need: Option<&[bool]>, ctx: &ExecCtx) -> Result<BoxNode> {
    let Plan::SysScan { table, filter, path, alias, mark: Some((m, w)) } = &**plan else {
        return open_node(plan, need, ctx);
    };
    let unmarked = Arc::new(Plan::SysScan {
        table: *table,
        filter: filter.clone(),
        path: path.clone(),
        alias: alias.clone(),
        mark: None,
    });
    let col = table.columns().iter().position(|(n, _)| n == "oid");
    // (The lock reads each row's oid, whatever is read above.)
    let need: Option<Vec<bool>> = need.map(|n| {
        let mut n = n.to_vec();
        if let Some(c) = col {
            if n.len() <= c {
                n.resize(c + 1, false);
            }
            n[c] = true;
        }
        n
    });
    let input = open_node(&unmarked, need.as_deref(), ctx)?;
    Ok(match col {
        Some(col) => Box::new(CatalogLockNode {
            input,
            class: table.oid(),
            col,
            mode: super::scan::lock_mode(*m),
            wait: *w,
        }),
        None => input,
    })
}

pub struct VecNode {
    rows: std::vec::IntoIter<Row>,
}

/// SELECT ... FOR UPDATE / SHARE over a system catalog: each row the scan returns is locked
/// (`locks::catalog_row_key` by its oid) as heap_lock_tuple locks a catalog tuple, so DDL
/// updating it (GRANT, an in-place update) waits, and waits for it.
struct CatalogLockNode {
    input: BoxNode,
    class: u32,
    col: usize,
    mode: crate::txn::locks::RowLockMode,
    wait: crate::sql::plan::Wait,
}

impl Node for CatalogLockNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        use crate::sql::plan::Wait;
        use crate::txn::locks::WaitPolicy;
        loop {
            let Some(row) = self.input.next(ctx)? else {
                return Ok(None);
            };
            let Some(Value::Oid(oid)) = row.get(self.col) else {
                return Ok(Some(row));
            };
            let key = crate::txn::locks::catalog_row_key(self.class, *oid);
            let policy = match self.wait {
                Wait::Block => WaitPolicy::Block,
                Wait::NoWait => WaitPolicy::NoWait,
                Wait::SkipLocked => WaitPolicy::SkipLocked,
            };
            let name = crate::catalog::system::SysTable::by_oid(self.class)
                .map_or_else(String::new, |t| t.name().to_string());
            let got = ctx
                .txn
                .borrow_mut()
                .lock_row(self.class, &key, self.mode, policy, &|| name.clone(), &|| None)?;
            if got {
                return Ok(Some(row));
            }
        }
    }
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

    fn count_rest(&mut self, _: &ExecCtx) -> Result<Option<u64>> {
        let n = self.rows.len() as u64;
        self.rows = Vec::new().into_iter();
        Ok(Some(n))
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

/// A catalog's rows as one statement scans it again and again (`sys_rescan`).
pub struct SysRescan {
    /// The catalog and the role they were generated for.
    cat: Arc<crate::catalog::Catalog>,
    role: u32,
    /// None: the catalog has errors deferred until a column is read (it is scanned whole
    /// each time, as before).
    rows: Option<Arc<crate::catalog::system::SysRows>>,
}

/// The `column = value` among `f`'s conjuncts (only the first conjunct with `first_only`)
/// that a hash on the column can serve and picks the fewest rows (pg_depend's
/// `refclassid = 1259 AND refobjid = x` picked every relation's rows by its first conjunct,
/// and the rest was evaluated on each: Django's sequences query, 150 ms at 10,000 tables): a built-in `=` over an integer, oid or
/// "char" column, or a name / text column under a deterministic collation (whose equality is
/// the bytes'), with a value (constant, or of outer references) of the column's own type.
/// Its column, and the rows whose value equals it, in row order (None: the value is NULL, and
/// the strict `=` is true for no row).
fn sys_equality(
    f: &Expr,
    rows: &crate::catalog::system::SysRows,
    first_only: bool,
    ctx: &ExecCtx,
) -> Result<Option<(usize, Option<Vec<u32>>)>> {
    fn split<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
        match e {
            Expr::And(items) => items.iter().for_each(|i| split(i, out)),
            e => out.push(e),
        }
    }
    let mut conj = Vec::new();
    split(f, &mut conj);
    if first_only {
        // (A later conjunct too when none before it can raise anything: for the rows it
        // leaves out the conjunction is false, whatever the others give.)
        let quiet = conj.iter().take_while(|c| quiet_filter(c)).count();
        conj.truncate((quiet + 1).min(conj.len()));
    }
    let mut best: Option<(usize, Vec<u32>)> = None;
    for c in conj {
        let Some((k, vals)) = sys_eq_values(c, ctx)? else { continue };
        // (`=` NULL: true for no row.)
        if matches!(c, Expr::Call(..)) && vals.first().is_some_and(Value::is_null) {
            return Ok(Some((k, None)));
        }
        let mut picked: Vec<u32> = Vec::new();
        let mut served = true;
        for v in vals.iter().filter(|v| !v.is_null()) {
            let same = |x: &Value| {
                matches!(
                    (x, v),
                    (Value::Oid(_), Value::Oid(_))
                        | (Value::Int2(_), Value::Int2(_))
                        | (Value::Int4(_), Value::Int4(_))
                        | (Value::Int8(_), Value::Int8(_))
                        | (Value::Char(_), Value::Char(_))
                        | (Value::Text(_), Value::Text(_))
                )
            };
            match rows.lookup(k, v, &same) {
                Some(ix) => picked.extend(ix),
                None => {
                    served = false;
                    break;
                }
            }
        }
        if !served {
            continue;
        }
        if vals.len() > 1 {
            picked.sort_unstable();
            picked.dedup();
        }
        if best.as_ref().is_none_or(|b| picked.len() < b.1.len()) {
            best = Some((k, picked));
        }
    }
    Ok(best.map(|(k, ix)| (k, Some(ix))))
}

/// The column and the values conjunct `c` compares it with for equality: a built-in `=`
/// (one value), `= ANY (array)` or `IN (...)` (ORMs' `relname IN ('t')`, which read every
/// row), deterministic collations only, values that read no column of the row and run no
/// subquery. None: another kind of conjunct.
fn sys_eq_values(c: &Expr, ctx: &ExecCtx) -> Result<Option<(usize, Vec<Value>)>> {
    let plain = |e: &Expr| {
        let mut sub = false;
        e.walk(&mut |x| sub |= matches!(x, Expr::Subquery(_)));
        !sub && e.columns().is_empty() && !e.is_volatile()
    };
    let builtin_eq = |d: &crate::sql::func::FuncDef| {
        d.name == "=" && d.oid < crate::catalog::FIRST_USER_OID && d.coll.as_ref().is_none_or(|c| c.deterministic)
    };
    Ok(match c {
        Expr::Call(d, args, _) if args.len() == 2 && builtin_eq(d) => {
            let (k, other) = match (&args[0], &args[1]) {
                (Expr::Col(k, _), o) | (o, Expr::Col(k, _)) => (*k, o),
                _ => return Ok(None),
            };
            if !plain(other) {
                return Ok(None);
            }
            Some((k, vec![crate::sql::expr::eval(other, &[], ctx)?]))
        }
        Expr::ArrayOp { left, right, op, all: false } if builtin_eq(op) && plain(right) => {
            let Expr::Col(k, _) = **left else { return Ok(None) };
            match crate::sql::expr::eval(right, &[], ctx)? {
                Value::Array(a) => Some((k, a.items.clone())),
                _ => None,
            }
        }
        Expr::InList(x, items, false) if items.iter().all(|(e, d)| builtin_eq(d) && plain(e)) => {
            let Expr::Col(k, _) = **x else { return Ok(None) };
            let vals = items.iter().map(|(e, _)| crate::sql::expr::eval(e, &[], ctx)).collect::<Result<Vec<_>>>()?;
            Some((k, vals))
        }
        _ => None,
    })
}

/// A catalog generated per scan whose index conditions or filter's first conjunct fix its
/// object column (`system::obj_column`: pg_description's `objoid = x`): that object's rows
/// alone (`system::rows_of_object`).
fn sys_obj_rows(
    table: crate::catalog::system::SysTable,
    filter: Option<&Expr>,
    path: Option<&crate::sql::plan::SysPath>,
    ctx: &ExecCtx,
) -> Result<Option<Vec<Row>>> {
    let Some(col) = crate::catalog::system::obj_column(table) else { return Ok(None) };
    let mut conds: Vec<&Expr> = Vec::new();
    if let Some(sp) = path
        && let crate::sql::plan::Access::Index { quals, .. } = &sp.scan.access
    {
        conds.extend(quals.iter().map(|q| &q.expr));
    }
    if let Some(f) = filter {
        let mut first = f;
        while let Expr::And(items) = first {
            match items.first() {
                Some(i) => first = i,
                None => break,
            }
        }
        conds.push(first);
    }
    for c in conds {
        match col_pin(col, c, ctx)? {
            Some(Some(o)) => return crate::catalog::system::rows_of_object(table, ctx, o),
            Some(None) => return Ok(Some(Vec::new())),
            None => {}
        }
    }
    Ok(None)
}

/// How many rescans of a catalog per statement read one relation's rows alone
/// (`sys_rel_rows`) before the whole catalog is generated, at least: an eighth of the
/// relations otherwise (`rel_rescans`; a nested loop over a schema's 1,000 relations into
/// pg_attribute generated its 150,000 rows of 11,000 tables, 140 ms against 5).
const REL_RESCANS: u32 = 16;

fn rel_rescans(cat: &crate::catalog::Catalog) -> u32 {
    let rels = cat.tables.len() + cat.views.len() + cat.indexes.len() + cat.sequences.len();
    REL_RESCANS.max((rels / 8) as u32)
}

/// How many inner rows a hash join asks its outer side to restrict itself to at most
/// (`Node::restrict_keys`; the outer side restricts itself only when the keys are few
/// against its rows: a schema's 1,500 relations against pg_attribute's 150,000 rows).
const RESTRICT_KEYS: usize = 1 << 16;

/// The value conjunct `c` sets `table`'s relation column to (`system::rel_column`), as
/// `sys_equality` reads such a conjunct: a built-in `=` between the column and a value (a
/// constant, or of outer references) of its type. Some(None): the value is NULL.
fn rel_pin(
    table: crate::catalog::system::SysTable,
    c: &Expr,
    ctx: &ExecCtx,
) -> Result<Option<Option<u32>>> {
    let Some(col) = crate::catalog::system::rel_column(table) else {
        return Ok(None);
    };
    col_pin(col, c, ctx)
}

/// The oid conjunct `c` sets column `col` to (`rel_pin`'s reading).
fn col_pin(col: usize, c: &Expr, ctx: &ExecCtx) -> Result<Option<Option<u32>>> {
    let Expr::Call(d, args, _) = c else { return Ok(None) };
    if d.name != "=" || args.len() != 2 || d.oid >= crate::catalog::FIRST_USER_OID {
        return Ok(None);
    }
    let other = match (&args[0], &args[1]) {
        (Expr::Col(k, _), o) | (o, Expr::Col(k, _)) if *k == col => o,
        _ => return Ok(None),
    };
    let mut sub = false;
    other.walk(&mut |e| sub |= matches!(e, Expr::Subquery(_)));
    if sub || !other.columns().is_empty() || other.is_volatile() {
        return Ok(None);
    }
    Ok(match crate::sql::expr::eval(other, &[], ctx)? {
        Value::Null => Some(None),
        Value::Oid(o) => Some(Some(o)),
        _ => None,
    })
}

/// The rows of the one relation a scan of a cacheable catalog reads, generated alone
/// (`system::rows_of_rel`), when its rows aren't generated already and its index path's
/// conditions or its filter's first conjunct pin the catalog's relation column: the rows
/// those read of the whole catalog are among them, in the same order, so the scan reads the
/// same rows (a catalog query right after DDL, psql's `\d`, would otherwise generate the
/// whole catalog for one relation).
fn sys_rel_rows(
    table: crate::catalog::system::SysTable,
    filter: Option<&Expr>,
    path: Option<&crate::sql::plan::SysPath>,
    ctx: &ExecCtx,
) -> Result<Option<Arc<crate::catalog::system::SysRows>>> {
    if crate::catalog::system::rel_column(table).is_none() {
        return Ok(None);
    }
    let mut conds: Vec<&Expr> = Vec::new();
    if let Some(sp) = path
        && let crate::sql::plan::Access::Index { quals, .. } = &sp.scan.access
    {
        conds.extend(quals.iter().map(|q| &q.expr));
    }
    if let Some(f) = filter {
        // (The first conjunct, as `sys_equality` splits the filter.)
        let mut first = f;
        while let Expr::And(items) = first {
            match items.first() {
                Some(i) => first = i,
                None => break,
            }
        }
        conds.push(first);
    }
    for c in conds {
        if let Some(pin) = rel_pin(table, c, ctx)? {
            if crate::catalog::system::rows_cached(table, ctx).is_some() {
                return Ok(None);
            }
            let rows = match pin {
                Some(rel) => match crate::catalog::system::rows_of_rel(table, ctx, rel)? {
                    Some(r) => r,
                    None => return Ok(None),
                },
                None => Vec::new(),
            };
            return Ok(Some(Arc::new(crate::catalog::system::SysRows::new(rows))));
        }
    }
    // pg_class read through its name index: the relations of those names alone.
    if let Some(bounds) = class_name_bounds(table, path, ctx)?
        && crate::catalog::system::rows_cached(table, ctx).is_none()
        && let Some(rows) = crate::catalog::system::class_rows_named(ctx, &bounds)?
    {
        return Ok(Some(Arc::new(crate::catalog::system::SysRows::new(rows))));
    }
    Ok(None)
}

/// The bounds an index scan of pg_class puts on `relname` (`pg_class_relname_nsp_index`'s
/// first column: `=`, or a pattern's prefix range), with built-in operators comparing bytes.
/// Postgres's scan reads only the index entries within them.
fn class_name_bounds(
    table: crate::catalog::system::SysTable,
    path: Option<&crate::sql::plan::SysPath>,
    ctx: &ExecCtx,
) -> Result<Option<crate::catalog::system::NameBounds>> {
    if table != crate::catalog::system::SysTable::PgClass {
        return Ok(None);
    }
    let Some(sp) = path else { return Ok(None) };
    let crate::sql::plan::Access::Index { quals, .. } = &sp.scan.access else {
        return Ok(None);
    };
    let mut bounds = crate::catalog::system::NameBounds::default();
    let mut any = false;
    for q in quals {
        let Expr::Call(d, args, _) = &q.expr else { continue };
        if args.len() != 2 || d.oid >= crate::catalog::FIRST_USER_OID {
            continue;
        }
        // (An equality under any deterministic collation is the bytes'; a range needs byte
        // order.)
        let equal = d.name == "=";
        if d.coll.as_ref().is_some_and(|c| if equal { !c.deterministic } else { !c.bytewise() }) {
            continue;
        }
        // (The key as the index applies it: of pg_class's indexes only the name index's first
        // column takes a string, whatever form the key has there. psql's `relname ~ '^(t)$'
        // COLLATE default` gives `relname = 't'::text` with relname read as text, and it
        // generated every pg_class row for one: 30-80 ms at 10,000 tables.)
        if q.col != 0 {
            continue;
        }
        let other = &args[1];
        let mut sub = false;
        other.walk(&mut |e| sub |= matches!(e, Expr::Subquery(_)));
        if sub || !other.columns().is_empty() || other.is_volatile() {
            continue;
        }
        let v = crate::sql::expr::eval(other, &[], ctx)?;
        let Some(s) = v.as_str() else { continue };
        let s = s.to_string();
        let tighter_lo = |b: &Option<(String, bool)>, n: &(String, bool)| {
            b.as_ref().is_none_or(|(x, inc)| n.0 > *x || (n.0 == *x && *inc && !n.1))
        };
        let tighter_hi = |b: &Option<(String, bool)>, n: &(String, bool)| {
            b.as_ref().is_none_or(|(x, inc)| n.0 < *x || (n.0 == *x && *inc && !n.1))
        };
        let (lo, hi) = match d.name {
            "=" => (Some((s.clone(), true)), Some((s, true))),
            ">=" | "~>=~" => (Some((s, true)), None),
            ">" | "~>~" => (Some((s, false)), None),
            "<=" | "~<=~" => (None, Some((s, true))),
            "<" | "~<~" => (None, Some((s, false))),
            _ => continue,
        };
        if let Some(l) = lo
            && tighter_lo(&bounds.lo, &l)
        {
            bounds.lo = Some(l);
        }
        if let Some(h) = hi
            && tighter_hi(&bounds.hi, &h)
        {
            bounds.hi = Some(h);
        }
        any = true;
    }
    Ok(any.then_some(bounds))
}

/// The rows of a catalog scan run once per outer row (a nested loop's parameterized inner
/// side, a correlated sub-select): None when the scan reads no outer value, or reads a
/// system view.
///
/// Postgres reads such a scan through the catalog's index (an index scan per outer row, or a
/// syscache lookup), never the whole catalog each time; the engine generates a catalog's rows
/// whole, which per outer row made a join over a full run's catalogs quadratic
/// (information_schema.column_domain_usage, oidjoins' checks: minutes). So a catalog's rows
/// are generated once per statement (for its catalog and role: the rows as of the statement's
/// start, which is what its snapshot gives Postgres's scans; a cacheable catalog's are its
/// database's shared ones, `system::rows_shared`), and an equality between a column and an
/// outer value (`pk.oid = fk.atttypid`) finds the rows it matches through a hash on that
/// column, as the index would; the whole filter is still checked on each.
fn sys_rescan(
    table: crate::catalog::system::SysTable,
    filter: Option<&Expr>,
    path: Option<&crate::sql::plan::SysPath>,
    need: Option<&[bool]>,
    ctx: &ExecCtx,
) -> Result<Option<Vec<Row>>> {
    // (A view's rows may change within a statement, as pg_settings after set_config:
    // Postgres runs its function again on a rescan with new parameters.)
    let Some(f) = filter else { return Ok(None) };
    if table.is_view() || !f.has_outer_refs() {
        return Ok(None);
    }
    let cat = ctx.catalog();
    let role = ctx.current_user();
    let cached = ctx
        .sys_rescans
        .borrow()
        .get(&table.oid())
        .filter(|e| Arc::ptr_eq(&e.cat, &cat) && e.role == role)
        .cloned();
    // (The first few rescans of a catalog not generated yet read one relation's rows alone;
    // past them, the whole catalog is generated, once.)
    let rel_rows = match &cached {
        None if crate::catalog::system::rows_cacheable(table)
            && ctx.sys_rel_scans.borrow().get(&table.oid()).copied().unwrap_or(0) < rel_rescans(&cat) =>
        {
            sys_rel_rows(table, Some(f), path, ctx)?
        }
        None if crate::catalog::system::obj_column(table).is_some() => {
            sys_obj_rows(table, Some(f), path, ctx)?.map(|r| Arc::new(crate::catalog::system::SysRows::new(r)))
        }
        _ => None,
    };
    let all = match rel_rows {
        Some(r) => {
            *ctx.sys_rel_scans.borrow_mut().entry(table.oid()).or_default() += 1;
            r
        }
        None => {
            let entry = match cached {
                Some(e) => e,
                None => {
                    let rows = if crate::catalog::system::rows_cacheable(table) {
                        Some(crate::catalog::system::rows_shared(table, ctx)?)
                    } else {
                        let rows = crate::catalog::system::rows(table, ctx)?;
                        let deferred = crate::catalog::system::deferred_errors(table, ctx, &rows);
                        deferred
                            .is_empty()
                            .then(|| Arc::new(crate::catalog::system::SysRows::new(rows)))
                    };
                    let e = Rc::new(SysRescan { cat, role, rows });
                    ctx.sys_rescans.borrow_mut().insert(table.oid(), e.clone());
                    e
                }
            };
            let Some(all) = &entry.rows else { return Ok(None) };
            all.clone()
        }
    };
    let all = &all;
    let found = match sys_index_equality(path, all, ctx)? {
        Some(e) => Some(e),
        None => sys_equality(f, all, false, ctx)?,
    };
    let picked = match found {
        Some((_, None)) => return Ok(Some(Vec::new())),
        Some((_, Some(ix))) => Some(ix),
        None => class_name_bounds(table, path, ctx)?.map(|b| sys_named(all, &b)),
    };
    // (Only the columns read above are copied, `RowChunk::project`; an index order a sort
    // relies on is found on whole rows first.)
    let ordered = path.filter(|sp| sp.ordered);
    let copy = if ordered.is_some() { None } else { need };
    let mut out = Vec::new();
    // (The filter is checked on a row holding the columns it reads.)
    let reads = filter_reads(f);
    let mut scratch: Row = Vec::new();
    let mut check = |c: &crate::catalog::system::RowChunk, i: usize| -> Result<()> {
        ctx.check()?;
        c.fill(i, reads.as_deref(), &mut scratch);
        if eval_bool(f, &scratch, ctx)? {
            out.push(c.project(i, copy));
        }
        Ok(())
    };
    match &picked {
        Some(ix) => ix.iter().try_for_each(|&i| {
            let (c, k) = all.locate(i as usize);
            check(c, k)
        })?,
        None => all.places().try_for_each(|(_, c, i)| check(c, i))?,
    }
    // An index path a sort relies on: the rows in that index's order.
    if let Some(sp) = ordered {
        out = crate::sql::paths::sys_index_order(out, &sp.scan, ctx)?.0;
    }
    Ok(Some(out))
}

/// The rows an index scan's equality conditions (`Access::Index` quals: the index's key
/// column `=` a value) select, as `sys_equality`'s: Postgres's index scan reads only those
/// (and checks the filter on them alone).
fn sys_index_equality(
    path: Option<&crate::sql::plan::SysPath>,
    rows: &crate::catalog::system::SysRows,
    ctx: &ExecCtx,
) -> Result<Option<(usize, Option<Vec<u32>>)>> {
    let Some(sp) = path else { return Ok(None) };
    let crate::sql::plan::Access::Index { quals, .. } = &sp.scan.access else {
        return Ok(None);
    };
    // (Each as the index applies it, and as the filter wrote it.)
    let conds: Vec<Expr> = (quals.iter())
        .flat_map(|q| [Some(q.expr.clone()), (!q.lossy).then(|| q.orig.clone())])
        .flatten()
        .collect();
    if conds.is_empty() {
        return Ok(None);
    }
    sys_equality(&Expr::And(conds), rows, false, ctx)
}

/// A scan of a cacheable catalog (`system::rows_cacheable`): its database's shared rows,
/// each copied only once it is a candidate, read a row at a time as the filter over the
/// whole rows was. An index scan's equality conditions (`sys_index_equality`) give the
/// candidates, as Postgres's index scan reads only those; otherwise, when the filter's first
/// conjunct is an equality a hash on the column serves, only the rows it is true or NULL for
/// are checked (for the others the conjunction is false before anything else in it is
/// evaluated, as it was). In an index's order when a sort relies on it.
fn sys_shared_scan(
    table: crate::catalog::system::SysTable,
    filter: Option<&Expr>,
    path: Option<&crate::sql::plan::SysPath>,
    need: Option<&[bool]>,
    ctx: &ExecCtx,
) -> Result<Option<BoxNode>> {
    if !crate::catalog::system::rows_cacheable(table) {
        return Ok(None);
    }
    // The index conditions first: an index scan checks the filter only on the rows they
    // select (a LIKE's exact prefix as `relname = 'a'`: the pattern `a\` isn't matched
    // against other names; pgjdbc's getTables).
    let quals: Vec<Expr> = match path.map(|sp| &sp.scan.access) {
        Some(crate::sql::plan::Access::Index { quals, .. }) => quals.iter().map(|q| q.expr.clone()).collect(),
        _ => Vec::new(),
    };
    // (A pg_class scan planned as the index scan its EXPLAIN shows: a LIKE on relname reads
    // only the names its fixed prefix selects, as the derived index condition does.)
    let quals = match (quals.is_empty(), path, filter) {
        (true, None, Some(f)) if table == crate::catalog::system::SysTable::PgClass => like_prefix_quals(f),
        _ => quals,
    };
    let checked = match (quals.is_empty(), filter) {
        (true, _) => filter.cloned(),
        (false, Some(f)) => Some(Expr::And(quals.into_iter().chain([f.clone()]).collect())),
        (false, None) => None,
    };
    let filter = checked.as_ref();
    // A whole catalog read in no order the scan gives: `count(*)` of it counts what the
    // relation maps keep (`system::row_count`) without generating any row, and the rows are
    // made only if something reads them (a session's first `count(*)` of pg_class after
    // connecting generated the whole catalog).
    let plain = path.is_none_or(|p| {
        matches!(p.scan.access, crate::sql::plan::Access::Seq | crate::sql::plan::Access::Unplanned) && !p.ordered
    });
    if filter.is_none() && plain && {
        use crate::catalog::system::SysTable::*;
        matches!(table, PgClass | PgAttribute | PgAttrdef | PgIndex)
    } {
        return Ok(Some(Box::new(LazyRowsNode {
            table,
            catalog: ctx.catalog(),
            filter: None,
            need: need.map(Rc::from),
            rows: None,
            peeked: None,
        })));
    }
    let rows = match sys_rel_rows(table, filter, path, ctx)? {
        Some(r) => r,
        // pg_class's first read in this catalog version: of the relations its filter fixes
        // alone, or (a filter that can't raise anything) made once a hash join above has
        // said which relations or schemas it can match (`LazyRowsNode`).
        None if table == crate::catalog::system::SysTable::PgClass
            && plain
            && crate::catalog::system::rows_cached(table, ctx).is_none() =>
        {
            let w = match filter {
                Some(f) => class_where(f, ctx)?,
                None => Default::default(),
            };
            match crate::catalog::system::class_rows_where(ctx, &w)? {
                Some(r) => Arc::new(crate::catalog::system::SysRows::new(r)),
                None if filter.is_none_or(quiet_filter) => {
                    return Ok(Some(Box::new(LazyRowsNode {
                        table,
                        catalog: ctx.catalog(),
                        filter: filter.cloned(),
                        need: need.map(Rc::from),
                        rows: None,
                        peeked: None,
                    })));
                }
                None => crate::catalog::system::rows_shared(table, ctx)?,
            }
        }
        // (pg_attribute's: of the relations a hash join above can match.)
        None if table == crate::catalog::system::SysTable::PgAttribute
            && plain
            && filter.is_none_or(quiet_filter)
            && crate::catalog::system::rows_cached(table, ctx).is_none() =>
        {
            return Ok(Some(Box::new(LazyRowsNode {
                table,
                catalog: ctx.catalog(),
                filter: filter.cloned(),
                need: need.map(Rc::from),
                rows: None,
                peeked: None,
            })));
        }
        None => crate::catalog::system::rows_shared(table, ctx)?,
    };
    // (The names' bounds only when no index equality picks the rows: they are checked
    // against every row.)
    let equal = sys_index_equality(path, &rows, ctx)?;
    let named = match equal {
        None => class_name_bounds(table, path, ctx)?.map(|b| sys_named(&rows, &b)),
        Some(_) => None,
    };
    let picked = match equal {
        Some((_, ix)) => Some(ix.unwrap_or_default()),
        None if named.is_some() => named,
        None => match filter {
            Some(f) => equality_picked(f, &rows, ctx)?,
            None => None,
        },
    };
    // An index path a sort relies on: the candidates in that index's order.
    if let Some(sp) = path.filter(|sp| sp.ordered) {
        let cand: Vec<Row> = match &picked {
            Some(ix) => ix.iter().map(|&i| rows.row(i as usize)).collect(),
            None => rows.iter().collect(),
        };
        let sorted = crate::sql::paths::sys_index_order(cand, &sp.scan, ctx)?.0;
        let node: BoxNode = Box::new(VecNode::new(sorted));
        return Ok(Some(match filter {
            Some(f) => Box::new(FilterNode {
                input: node,
                pred: f.clone(),
            }),
            None => node,
        }));
    }
    Ok(Some(Box::new(SharedRowsNode::new(rows, picked, filter, need.map(Rc::from)))))
}

/// The rows an index path's conditions select (all of them for another path).
#[inline(never)]
fn sys_index_prefilter(rows: Vec<Row>, sp: &crate::sql::plan::SysPath, ctx: &ExecCtx) -> Result<Vec<Row>> {
    let crate::sql::plan::Access::Index { quals, .. } = &sp.scan.access else { return Ok(rows) };
    if quals.is_empty() {
        return Ok(rows);
    }
    let mut kept = Vec::with_capacity(rows.len());
    'rows: for r in rows {
        for q in quals {
            if !eval_bool(&q.expr, &r, ctx)? {
                continue 'rows;
            }
        }
        kept.push(r);
    }
    Ok(kept)
}

/// A filter's `relname LIKE 'const'` conjuncts as the index conditions their fixed prefix
/// derives (like_support.c: `relname = 'prefix'` for an exact prefix, `relname ^@ 'prefix'`
/// for a partial one), to be checked before the filter.
fn like_prefix_quals(f: &Expr) -> Vec<Expr> {
    let conj: Vec<&Expr> = match f {
        Expr::And(items) => items.iter().collect(),
        e => vec![e],
    };
    let mut out = Vec::new();
    for c in conj {
        let Expr::Call(d, args, _) = c else { continue };
        if d.name != "~~" || args.len() != 2 || d.coll.as_ref().is_some_and(|c| !c.bytewise()) {
            continue;
        }
        let (Expr::Col(1, _), Expr::Const(Value::Text(p), _)) = (&args[0], &args[1]) else { continue };
        let (status, prefix, _) = crate::sql::costs::pattern_prefix(p, crate::sql::costs::Ptype::Like);
        let pre = Expr::Const(Value::Text(prefix.into()), crate::types::DataType::Text);
        let col = args[0].clone();
        // (texteq, text_starts_with.)
        let oid = match status {
            crate::sql::costs::PrefixStatus::Exact => 67,
            crate::sql::costs::PrefixStatus::Partial => 3696,
            crate::sql::costs::PrefixStatus::None => continue,
        };
        if let Some(def) = crate::sql::func::def_by_oid(oid) {
            out.push(Expr::Call(def, vec![col, pre], crate::types::DataType::Bool));
        }
    }
    out
}

/// The rows of pg_class's `rows` whose `relname` `bounds` admits, in row order.
fn sys_named(rows: &crate::catalog::system::SysRows, bounds: &crate::catalog::system::NameBounds) -> Vec<u32> {
    (rows.places())
        .filter(|&(_, c, i)| c.with_value(i, 1, |v| v.as_str().is_some_and(|n| bounds.admits(n))))
        .map(|(n, ..)| n as u32)
        .collect()
}

/// The columns of statistics view `table` (`stat_views::generated_here`) a scan's parent reads
/// (`need`) and its filter reads: the ones `stat_views::rows_of` makes. None: all of them, or
/// not such a view.
fn stat_reads(table: crate::catalog::system::SysTable, filter: Option<&Expr>, need: Option<&[bool]>) -> Option<Vec<bool>> {
    if !crate::catalog::stat_views::generated_here(table) {
        return None;
    }
    let mut reads = need?.to_vec();
    if let Some(f) = filter {
        // (A mask is as long as its last column read: `exec::need`.)
        let fr = filter_reads(f)?;
        if reads.len() < fr.len() {
            reads.resize(fr.len(), false);
        }
        for (i, r) in fr.iter().enumerate() {
            reads[i] |= *r;
        }
    }
    Some(reads)
}

/// The rows of an information_schema view generated per relation
/// (`system::is_rel_columns`) whose filter starts with equalities fixing `table_schema`
/// and/or `table_name` (`WHERE table_schema = 's' AND table_name = 't'`): those relations'
/// alone (`system::is_rows_named`). Only leading conjuncts are read, so the rows left out
/// are those for which the filter stops at one of them, false, as it did over every row.
/// `reads`: a statistics view's columns read (`stat_reads`).
fn is_named_rows(
    table: crate::catalog::system::SysTable,
    filter: Option<&Expr>,
    reads: Option<&[bool]>,
    ctx: &ExecCtx,
) -> Result<Option<Vec<Row>>> {
    let Some((sk, nk)) = crate::catalog::system::is_rel_columns(table) else {
        return Ok(None);
    };
    let Some(f) = filter else { return Ok(None) };
    fn split<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
        match e {
            Expr::And(items) => items.iter().for_each(|i| split(i, out)),
            e => out.push(e),
        }
    }
    let mut conj = Vec::new();
    split(f, &mut conj);
    // (A statistics view's `relid` / `indexrelid`, pinned to an oid as `relname` to a name.)
    let (rk, xk) = match crate::catalog::stat_views::oid_columns(table) {
        Some((r, x)) => (Some(r), x),
        None => (None, None),
    };
    let is_key = |k: usize| k == sk || k == nk || Some(k) == rk || Some(k) == xk;
    let mut sel = crate::catalog::stat_views::StatSel { reads: reads.map(<[bool]>::to_vec), ..Default::default() };
    for c in conj {
        // (`col = v`, or `col = ANY (array of one v)`: Prisma's `table_schema = ANY ($1)`.)
        let (d, k, other, one) = match c {
            Expr::Call(d, args, _) if args.len() == 2 => match (&args[0], &args[1]) {
                (Expr::Col(k, _), o) | (o, Expr::Col(k, _)) if is_key(*k) => (d, *k, o, false),
                _ => break,
            },
            Expr::ArrayOp { left, right, op, all: false } => match &**left {
                Expr::Col(k, _) if is_key(*k) => (op, *k, &**right, true),
                _ => break,
            },
            _ => break,
        };
        if d.name != "=" || d.oid >= crate::catalog::FIRST_USER_OID {
            break;
        }
        if d.coll.as_ref().is_some_and(|c| !c.deterministic) {
            break;
        }
        let mut sub = false;
        other.walk(&mut |e| sub |= matches!(e, Expr::Subquery(_)));
        if sub || !other.columns().is_empty() || other.is_volatile() {
            break;
        }
        let v = crate::sql::expr::eval(other, &[], ctx)?;
        let v = match (one, &v) {
            (false, _) => v,
            (true, Value::Array(a)) if a.items.len() == 1 => a.items[0].clone(),
            _ => break,
        };
        if Some(k) == rk || Some(k) == xk {
            let Value::Oid(o) = v else { break };
            let slot = if Some(k) == rk { &mut sel.relid } else { &mut sel.indexrelid };
            if slot.is_some_and(|x| x != o) {
                return Ok(Some(Vec::new()));
            }
            *slot = Some(o);
            continue;
        }
        let Some(s) = v.as_str() else { break };
        let slot = if k == sk { &mut sel.schema } else { &mut sel.name };
        if slot.as_deref().is_some_and(|x| x != s) {
            // (Two different values: no row has both.)
            return Ok(Some(Vec::new()));
        }
        *slot = Some(s.to_string());
    }
    if sel.schema.is_none() && sel.name.is_none() && sel.relid.is_none() && sel.indexrelid.is_none() {
        return Ok(None);
    }
    crate::catalog::system::is_rows_named(table, ctx, &sel)
}

/// `sys_shared_scan`'s node.
struct SharedRowsNode {
    rows: Arc<crate::catalog::system::SysRows>,
    /// The rows to check, in order (None: all).
    picked: Option<Vec<u32>>,
    i: usize,
    /// Reading all of them: the chunk and the row in it next.
    chunk: (usize, usize),
    filter: Option<Rc<Expr>>,
    /// The filter compiled (`exec::kernel`): what most catalog filters are, evaluated without
    /// `eval`'s calls (`relkind IN (...)` over every pg_class row was most of psql's `\dt`).
    kernel: Option<SysKernel>,
    /// The columns read above (`exec::need`): only those are copied out of the shared rows.
    need: Option<Rc<[bool]>>,
    /// The columns the filter reads (None: all), and the row they are set in to check it.
    reads: Option<Box<[bool]>>,
    scratch: Row,
}

/// The columns filter `f` reads (None: all of them), which a catalog scan sets in the row
/// it checks `f` on (the shared rows are kept by column, `catalog::sysrows`).
fn filter_reads(f: &Expr) -> Option<Box<[bool]>> {
    let mut sub = false;
    f.walk(&mut |e| sub |= matches!(e, Expr::Subquery(_)));
    if sub { None } else { super::need::of([f]).map(Vec::into_boxed_slice) }
}

/// A catalog scan's filter compiled (`exec::kernel`): all of it, or its leading conjuncts
/// that compile and the rest (`relkind IN (...) AND pg_table_is_visible(oid)`, Django's
/// table list, evaluated `relkind`'s test through `eval` on every pg_class row).
struct SysKernel {
    kernel: super::kernel::K,
    rest: Option<Expr>,
}

fn sys_kernel(f: &Expr) -> Option<SysKernel> {
    if let Some(kernel) = super::kernel::compile(f) {
        return Some(SysKernel { kernel, rest: None });
    }
    let Expr::And(items) = f else { return None };
    (1..items.len()).rev().find_map(|n| {
        let kernel = super::kernel::compile(&Expr::And(items[..n].to_vec()))?;
        let rest = match &items[n..] {
            [one] => one.clone(),
            more => Expr::And(more.to_vec()),
        };
        Some(SysKernel { kernel, rest: Some(rest) })
    })
}

/// Whether row `r` passes `f`, the filter (through `kernel`, its compiled form, as far as
/// it tells: the rest is evaluated after a compiled part that isn't false, as `eval` of the
/// conjunction would).
fn sys_passes(kernel: Option<&SysKernel>, f: &Expr, r: &Row, ctx: &ExecCtx) -> Result<bool> {
    let Some(k) = kernel else { return eval_bool(f, r, ctx) };
    match (k.kernel.truth(r), &k.rest) {
        (None, _) => eval_bool(f, r, ctx),
        (Some(Some(false)), _) => Ok(false),
        (Some(t), None) => Ok(t == Some(true)),
        (Some(t), Some(rest)) => Ok(eval_bool(rest, r, ctx)? && t == Some(true)),
    }
}

impl SharedRowsNode {
    fn new(
        rows: Arc<crate::catalog::system::SysRows>,
        picked: Option<Vec<u32>>,
        filter: Option<&Expr>,
        need: Option<Rc<[bool]>>,
    ) -> SharedRowsNode {
        SharedRowsNode {
            rows,
            picked,
            i: 0,
            chunk: (0, 0),
            kernel: filter.and_then(sys_kernel),
            reads: filter.and_then(filter_reads),
            filter: filter.cloned().map(Rc::new),
            need,
            scratch: Vec::new(),
        }
    }

    /// Whether the row at (`c`, `k`) (`advance`'s) passes the filter `f`.
    fn passes(&mut self, f: &Expr, c: usize, k: usize, ctx: &ExecCtx) -> Result<bool> {
        let (chunk, i) = shared_place(&self.rows, c, k);
        chunk.fill(i, self.reads.as_deref(), &mut self.scratch);
        sys_passes(self.kernel.as_ref(), f, &self.scratch, ctx)
    }

    /// The next row to check, if any.
    fn advance(&mut self) -> Option<(usize, usize)> {
        match &self.picked {
            Some(ix) => {
                let r = *ix.get(self.i)? as usize;
                self.i += 1;
                Some((usize::MAX, r))
            }
            None => {
                let chunks = self.rows.chunks();
                while self.chunk.0 < chunks.len() && self.chunk.1 >= chunks[self.chunk.0].len() {
                    self.chunk = (self.chunk.0 + 1, 0);
                }
                let (c, k) = self.chunk;
                if c >= chunks.len() {
                    return None;
                }
                self.chunk.1 += 1;
                Some((c, k))
            }
        }
    }
}

/// Row `k` of chunk `c` of `rows` (`c` usize::MAX: row `k` of all): its chunk, and its place there.
fn shared_place(rows: &crate::catalog::system::SysRows, c: usize, k: usize) -> (&crate::catalog::system::RowChunk, usize) {
    if c == usize::MAX { rows.locate(k) } else { (&rows.chunks()[c], k) }
}

impl Node for SharedRowsNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let filter = self.filter.clone();
        while let Some((c, k)) = self.advance() {
            if let Some(f) = &filter {
                ctx.check()?;
                if !self.passes(f, c, k, ctx)? {
                    continue;
                }
            }
            let (chunk, i) = shared_place(&self.rows, c, k);
            return Ok(Some(chunk.project(i, self.need.as_deref())));
        }
        Ok(None)
    }

    fn count_rest(&mut self, ctx: &ExecCtx) -> Result<Option<u64>> {
        let mut n = 0;
        let Some(f) = self.filter.clone() else {
            // (Nothing to evaluate: what is left.)
            n = match &self.picked {
                Some(ix) => ix.len().saturating_sub(self.i),
                None => {
                    let chunks = self.rows.chunks();
                    (chunks.iter().skip(self.chunk.0).map(|c| c.len()).sum::<usize>()).saturating_sub(self.chunk.1)
                }
            } as u64;
            self.picked = Some(Vec::new());
            self.i = 0;
            return Ok(Some(n));
        };
        while let Some((c, k)) = self.advance() {
            ctx.check()?;
            if self.passes(&f, c, k, ctx)? {
                n += 1;
            }
        }
        Ok(Some(n))
    }

    fn restrict_keys(&mut self, col: usize, keys: &[u32], _ctx: &ExecCtx) -> Result<()> {
        // (Only when the filter can't raise anything or call anything for the rows left out:
        // a Seq Scan's filter runs on every row. The rows already read stay read: a hash join
        // that read its first outer row before building, as ExecHashJoin does when the outer
        // side starts cheaper, restricts the rest.)
        if !self.filter.as_ref().is_none_or(|f| quiet_filter(f)) {
            return Ok(());
        }
        // The rows not read yet, by their number in row order.
        let rest: Option<&[u32]> = self.picked.as_ref().map(|p| &p[self.i.min(p.len())..]);
        let next = match rest {
            Some(_) => 0,
            None => {
                let chunks = self.rows.chunks();
                chunks.iter().take(self.chunk.0).map(|c| c.len()).sum::<usize>() + self.chunk.1
            }
        };
        let left = rest.map_or_else(|| self.rows.len().saturating_sub(next), |r| r.len());
        // (Past a quarter of the rows, reading them all costs about as much.)
        if keys.len().saturating_mul(4) > left {
            return Ok(());
        }
        let same = |x: &Value| matches!(x, Value::Oid(_));
        let mut all: Vec<u32> = Vec::new();
        for k in keys {
            match self.rows.lookup(col, &Value::Oid(*k), &same) {
                Some(ix) => all.extend(ix),
                None => return Ok(()),
            }
        }
        all.sort_unstable();
        all.dedup();
        match rest {
            Some(r) => {
                let had: std::collections::HashSet<u32> = r.iter().copied().collect();
                all.retain(|i| had.contains(i));
            }
            None => all.retain(|&i| i as usize >= next),
        }
        self.picked = Some(all);
        self.i = 0;
        Ok(())
    }
}

/// `sys_shared_scan`'s node for a catalog whose rows no statement has made yet: its shared
/// rows are taken (generated if no statement has yet) only when a row is asked for. Without a
/// filter, `count(*)` counts the relation maps' rows (`system::row_count`); a hash join above
/// that restricts pg_class to some relations or schemas first (`restrict_keys`: psql's `\dt`
/// joins pg_namespace) gets only those relations' rows made (`system::class_rows_where`).
struct LazyRowsNode {
    table: crate::catalog::system::SysTable,
    catalog: Arc<crate::catalog::Catalog>,
    /// A filter proven quiet before generation was deferred (`quiet_filter`).
    filter: Option<Expr>,
    need: Option<Rc<[bool]>>,
    rows: Option<SharedRowsNode>,
    /// pg_class's first row, made alone before its rows were (`peek`): its oid, read already.
    peeked: Option<u32>,
}

impl LazyRowsNode {
    fn rows(&mut self, ctx: &ExecCtx) -> Result<&mut SharedRowsNode> {
        if self.rows.is_none() {
            let held = (!Arc::ptr_eq(&self.catalog, &ctx.catalog()))
                .then(|| ctx.reading_catalog(self.catalog.clone()));
            let generation = held.as_ref().unwrap_or(ctx);
            let rows = crate::catalog::system::rows_shared(self.table, generation)?;
            let mut picked = match &self.filter {
                Some(f) => equality_picked(f, &rows, generation)?,
                None => None,
            };
            // (Past the row `peek` read: the rows up to it were checked then.)
            let mut start = (0, 0);
            if let Some(o) = self.peeked
                && let Some(ix) = rows.lookup(0, &Value::Oid(o), &|x| matches!(x, Value::Oid(_)))
                && let Some(&n) = ix.first()
            {
                match &mut picked {
                    Some(p) => p.retain(|&i| i > n),
                    None => {
                        let mut at = n as usize + 1;
                        let chunks = rows.chunks();
                        while start.0 < chunks.len() && at >= chunks[start.0].len() {
                            at -= chunks[start.0].len();
                            start.0 += 1;
                        }
                        start.1 = at;
                    }
                }
            }
            let mut node = SharedRowsNode::new(rows, picked, self.filter.as_ref(), self.need.clone());
            node.chunk = start;
            self.rows = Some(node);
        }
        Ok(self.rows.as_mut().unwrap())
    }

    /// pg_class's first row that passes the filter among Postgres's own relations' (which
    /// lead its rows), made alone: a hash join reads its outer side's first row before it
    /// builds (ExecHashJoin, when the outer side starts cheaper), and making every row for it
    /// came before the join's keys could restrict them (`restrict_keys`: psql's `\dt`, whose
    /// first run after a start made pg_class's 40,000 rows at 10,000 tables for 1,000).
    fn peek(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        use crate::catalog::system::SysTable;
        if self.table != SysTable::PgClass || self.peeked.is_some() || !self.catalog.rel_oid_edits.is_empty() {
            return Ok(None);
        }
        let held = (!Arc::ptr_eq(&self.catalog, &ctx.catalog()))
            .then(|| ctx.reading_catalog(self.catalog.clone()));
        let generation = held.as_ref().unwrap_or(ctx);
        let mut at = 0;
        while let Some(row) = crate::catalog::system::class_builtin_row(generation, at)? {
            at += 1;
            let Some(row) = row else { continue };
            if self.filter.as_ref().map_or(Ok(true), |f| eval_bool(f, &row, generation))? {
                let Value::Oid(o) = row[0] else { return Ok(None) };
                self.peeked = Some(o);
                return Ok(Some(row));
            }
        }
        Ok(None)
    }
}

impl Node for LazyRowsNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.rows.is_none()
            && self.peeked.is_none()
            && let Some(r) = self.peek(ctx)?
        {
            return Ok(Some(r));
        }
        self.rows(ctx)?.next(ctx)
    }

    fn count_rest(&mut self, ctx: &ExecCtx) -> Result<Option<u64>> {
        match &mut self.rows {
            Some(r) => r.count_rest(ctx),
            None if self.filter.is_none() && self.peeked.is_none() => {
                let held = (!Arc::ptr_eq(&self.catalog, &ctx.catalog()))
                    .then(|| ctx.reading_catalog(self.catalog.clone()));
                let n = crate::catalog::system::row_count(self.table, held.as_ref().unwrap_or(ctx));
                // (Nothing is left once counted.)
                let none = Arc::new(crate::catalog::system::SysRows::new(Vec::new()));
                self.rows = Some(SharedRowsNode::new(none, None, None, None));
                Ok(n.map(|n| n as u64))
            }
            None => self.rows(ctx)?.count_rest(ctx),
        }
    }

    fn restrict_keys(&mut self, col: usize, keys: &[u32], ctx: &ExecCtx) -> Result<()> {
        use crate::catalog::system::SysTable;
        let held = (!Arc::ptr_eq(&self.catalog, &ctx.catalog()))
            .then(|| ctx.reading_catalog(self.catalog.clone()));
        let generation = held.as_ref().unwrap_or(ctx);
        let quiet = self.filter.as_ref().is_none_or(quiet_filter);
        let subset = match (self.table, col) {
            _ if self.rows.is_some() || !quiet => None,
            (SysTable::PgClass, 0 | 2) => {
                let mut w = match &self.filter {
                    Some(f) => class_where(f, generation)?,
                    None => Default::default(),
                };
                let slot = if col == 0 { &mut w.oids } else { &mut w.schemas };
                let mut keys = keys.to_vec();
                if let Some(had) = slot.take() {
                    keys.retain(|k| had.contains(k));
                }
                *slot = Some(keys);
                crate::catalog::system::class_rows_where(generation, &w)?
            }
            (SysTable::PgAttribute, 0) => crate::catalog::system::attribute_rows_of(generation, keys)?,
            _ => None,
        };
        if let Some(mut rows) = subset {
            // (`peek`'s row was read already.)
            if let Some(o) = self.peeked {
                rows.retain(|r| r[0] != Value::Oid(o));
            }
            let rows = Arc::new(crate::catalog::system::SysRows::new(rows));
            self.rows = Some(SharedRowsNode::new(rows, None, self.filter.as_ref(), self.need.clone()));
            return Ok(());
        }
        self.rows(ctx)?.restrict_keys(col, keys, ctx)
    }
}

/// The rows of `rows` a filter's first conjunct's equality picks through a hash on its column
/// (`sys_equality`), with the rows NULL there, in order (None: all of them): for the others the
/// conjunction is false before anything else in it is evaluated.
fn equality_picked(f: &Expr, rows: &crate::catalog::system::SysRows, ctx: &ExecCtx) -> Result<Option<Vec<u32>>> {
    Ok(match sys_equality(f, rows, true, ctx)? {
        Some((k, ix)) => {
            let nulls = rows.nulls(k);
            let mut all: Vec<u32> = ix.unwrap_or_default();
            if !nulls.is_empty() {
                all.extend(nulls.iter().copied());
                all.sort_unstable();
            }
            Some(all)
        }
        None => None,
    })
}

/// What a pg_class filter's conjuncts fix of `oid`, `relnamespace` and `relkind` (`= v`, `=
/// ANY (v)`, `IN (...)` with values that read no column; `system::class_rows_where`): only
/// conjuncts after conjuncts that can't raise or show anything (`quiet_filter`), so the rows
/// left out are rows the filter is false for, with nothing else having shown.
fn class_where(f: &Expr, ctx: &ExecCtx) -> Result<crate::catalog::system::ClassWhere> {
    fn split<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
        match e {
            Expr::And(items) => items.iter().for_each(|i| split(i, out)),
            e => out.push(e),
        }
    }
    let mut conj = Vec::new();
    split(f, &mut conj);
    let mut w = crate::catalog::system::ClassWhere::default();
    for c in conj {
        let Some((col, vals)) = class_conjunct(c, ctx)? else {
            if quiet_filter(c) {
                continue;
            }
            break;
        };
        let oids = || vals.iter().filter_map(|v| if let Value::Oid(o) = v { Some(*o) } else { None }).collect::<Vec<u32>>();
        let both = |had: Option<Vec<u32>>, new: Vec<u32>| match had {
            Some(h) => new.into_iter().filter(|x| h.contains(x)).collect(),
            None => new,
        };
        match col {
            0 => w.oids = Some(both(w.oids.take(), oids())),
            2 => w.schemas = Some(both(w.schemas.take(), oids())),
            _ => {
                let kinds: Vec<u8> = vals.iter().filter_map(|v| if let Value::Char(k) = v { Some(*k) } else { None }).collect();
                w.kinds = Some(match w.kinds.take() {
                    Some(h) => kinds.into_iter().filter(|x| h.contains(x)).collect(),
                    None => kinds,
                });
            }
        }
    }
    Ok(w)
}

/// A conjunct comparing pg_class's `oid` (0), `relnamespace` (2) or `relkind` (17) with
/// values (`class_where`): the column and the values (oids, "char"s). None: not one, or a
/// value that can't be computed here.
fn class_conjunct(c: &Expr, ctx: &ExecCtx) -> Result<Option<(usize, Vec<Value>)>> {
    let col = |e: &Expr| match e {
        Expr::Col(k, _) if matches!(*k, 0 | 2 | 17) => Some(*k),
        _ => None,
    };
    let fixed = |e: &Expr| {
        let mut sub = false;
        e.walk(&mut |x| sub |= matches!(x, Expr::Subquery(_)));
        !sub && e.columns().is_empty() && !e.is_volatile()
    };
    let eq = |d: &crate::sql::func::FuncDef| d.name == "=" && d.oid < crate::catalog::FIRST_USER_OID;
    // (A value that fails to compute is left to the filter: it raises the error only for the
    // rows it reaches.)
    let value = |e: &Expr| crate::sql::expr::eval(e, &[], ctx).ok();
    let typed = |k: usize, v: &Value| match v {
        Value::Oid(_) => k != 17,
        Value::Char(_) => k == 17,
        _ => v.is_null(),
    };
    let (k, vals) = match c {
        Expr::Call(d, args, _) if args.len() == 2 && eq(d) => {
            let (k, v) = match (col(&args[0]), col(&args[1])) {
                (Some(k), _) if fixed(&args[1]) => (k, &args[1]),
                (_, Some(k)) if fixed(&args[0]) => (k, &args[0]),
                _ => return Ok(None),
            };
            let Some(v) = value(v) else { return Ok(None) };
            (k, vec![v])
        }
        Expr::ArrayOp { left, right, op, all: false } if eq(op) && fixed(right) => {
            let Some(k) = col(left) else { return Ok(None) };
            match value(right) {
                Some(Value::Array(a)) => (k, a.items.to_vec()),
                Some(Value::Null) => (k, Vec::new()),
                _ => return Ok(None),
            }
        }
        Expr::InList(x, items, false) if items.iter().all(|(e, d)| eq(d) && fixed(e)) => {
            let Some(k) = col(x) else { return Ok(None) };
            let mut vals = Vec::with_capacity(items.len());
            for (e, _) in items {
                let Some(v) = value(e) else { return Ok(None) };
                vals.push(v);
            }
            (k, vals)
        }
        _ => return Ok(None),
    };
    Ok(vals.iter().all(|v| typed(k, v)).then_some((k, vals)))
}

/// Whether evaluating `e` on a row can't raise an error, a notice or any other effect:
/// columns and constants compared by built-in operators, with AND / OR / NOT and IS [NOT]
/// NULL.
fn quiet_filter(e: &Expr) -> bool {
    match e {
        Expr::Col(..) | Expr::Const(..) => true,
        Expr::And(v) | Expr::Or(v) => v.iter().all(quiet_filter),
        Expr::Not(x) | Expr::IsNull(x, _) => quiet_filter(x),
        Expr::Call(d, args, _) => {
            d.oid < crate::catalog::FIRST_USER_OID
                && matches!(d.name, "=" | "<>" | "<" | "<=" | ">" | ">=")
                && args.iter().all(|a| matches!(a, Expr::Col(..) | Expr::Const(..)))
        }
        // (`relkind IN ('r', 'p')`, `contype = ANY ('{p,u}')`.)
        Expr::ArrayOp { left, right, op, .. } => {
            op.oid < crate::catalog::FIRST_USER_OID
                && matches!(op.name, "=" | "<>")
                && matches!(**left, Expr::Col(..) | Expr::Const(..))
                && matches!(**right, Expr::Const(..))
        }
        Expr::InList(x, items, _) => {
            matches!(**x, Expr::Col(..) | Expr::Const(..))
                && (items.iter()).all(|(e, d)| {
                    d.oid < crate::catalog::FIRST_USER_OID && matches!(d.name, "=" | "<>") && matches!(e, Expr::Const(..))
                })
        }
        _ => false,
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
    /// The expressions a parallel scan below computes in its workers (`compute_in_workers`,
    /// tried at the first row): the scan row's length without them, and each expression's
    /// place among them. None: all evaluated here.
    computed: Option<(usize, Vec<Option<usize>>)>,
    tried: bool,
    /// A bounded sort above's wish (`topk_below`), for the scan when the key is computed there.
    topk: Option<super::TopK>,
    /// Per expression, the input column it is when no other expression reads that column:
    /// moved out of the input row rather than copied (`NOT_MOVED`: evaluated). None: every
    /// expression is evaluated (decided at the first row, `project_moves`).
    moves: Option<Box<[u32]>>,
    /// The input runs a pipeline for an aggregation above (`pipeline_projected`): its
    /// parts pass through, its rows are projected.
    piped: bool,
}

const NOT_MOVED: u32 = u32::MAX;

/// A piped projection's next item: a part as it comes, a row projected.
#[inline(never)]
fn projected_item(p: &mut ProjectNode, ctx: &ExecCtx) -> Result<Option<super::pipe::Item>> {
    use super::pipe::Item;
    Ok(match p.input.next_item(ctx)? {
        None => None,
        Some(Item::Part(x)) => Some(Item::Part(x)),
        Some(Item::Row(r, l)) => Some(Item::Row(
            match &p.moves {
                Some(m) => project_moving(&p.exprs, m, r, ctx)?,
                None => eval_all(&p.exprs, &r, ctx)?,
            },
            l,
        )),
    })
}

/// `ProjectNode::moves` of `exprs`: None when no column can move.
#[inline(never)]
fn project_moves(exprs: &[Expr]) -> Option<Box<[u32]>> {
    let mut reads: Vec<u32> = Vec::new();
    for e in exprs {
        for (i, r) in super::need::of([e])?.into_iter().enumerate() {
            if r {
                if reads.len() <= i {
                    reads.resize(i + 1, 0);
                }
                reads[i] += 1;
            }
        }
    }
    let moves: Box<[u32]> = exprs
        .iter()
        .map(|e| match e {
            Expr::Col(i, _) if reads.get(*i) == Some(&1) => *i as u32,
            _ => NOT_MOVED,
        })
        .collect();
    moves.iter().any(|&m| m != NOT_MOVED).then_some(moves)
}

/// The projection of `r` with `moves`: the evaluated expressions first, in order (they read
/// the row whole), then the moved columns taken out of it.
#[inline(never)]
fn project_moving(exprs: &[Expr], moves: &[u32], mut r: Row, ctx: &ExecCtx) -> Result<Row> {
    let mut out = Vec::with_capacity(exprs.len());
    for (e, &m) in exprs.iter().zip(moves) {
        out.push(if m == NOT_MOVED || m as usize >= r.len() { eval(e, &r, ctx)? } else { Value::Null });
    }
    for (o, &m) in out.iter_mut().zip(moves) {
        if m != NOT_MOVED && (m as usize) < r.len() {
            *o = std::mem::replace(&mut r[m as usize], Value::Null);
        }
    }
    Ok(out)
}

/// A direct column whose built-in ordering can be used by a worker's first-key heap.
fn direct_numeric_col(e: &Expr) -> bool {
    use crate::types::DataType::{Float4, Float8, Int2, Int4, Int8, Numeric};
    matches!(e, Expr::Col(_, Int2 | Int4 | Int8 | Float4 | Float8 | Numeric(_)))
}

impl ProjectNode {
    /// Hand pgvector distances, or a bounded sort's whole numeric column projection, to
    /// a parallel scan below, when there is one.
    fn compute_below(&mut self, ctx: &ExecCtx) -> Result<()> {
        self.tried = true;
        self.moves = project_moves(&self.exprs);
        // Large bounds build worker heaps without discarding rows. Their shallow slots
        // alone must fit work_mem before enabling numeric column computation.
        let numeric = self.topk.is_some_and(|t| super::parscan::topk_slots_fit(t.k, super::workmem::work_mem(ctx) as usize))
            && self.exprs.iter().all(direct_numeric_col);
        let mut kernels = Vec::new();
        let places: Vec<Option<usize>> = self
            .exprs
            .iter()
            .map(|e| match super::kernel::compile(e) {
                Some(k @ super::kernel::K::Dist(..)) => {
                    kernels.push(k);
                    Some(kernels.len() - 1)
                }
                Some(k @ super::kernel::K::Col(_)) if numeric => {
                    kernels.push(k);
                    Some(kernels.len() - 1)
                }
                _ => None,
            })
            .collect();
        if kernels.is_empty() {
            return Ok(());
        }
        // The columns only the computed expressions read: the workers drop them from the
        // rows they computed them for (rows a kernel left to `eval` keep them).
        let reads = |computed: bool| super::need::of(self.exprs.iter().zip(&places).filter(|(_, p)| p.is_some() == computed).map(|(e, _)| e));
        let drop: Vec<usize> = match (reads(true), reads(false)) {
            (Some(c), Some(o)) => (0..c.len()).filter(|&i| c[i] && !o.get(i).copied().unwrap_or(false)).collect(),
            _ => Vec::new(),
        };
        // Of those, the ones read only as a distance's column argument: not decoded.
        let raw = drop
            .iter()
            .copied()
            .filter(|&c| {
                self.exprs.iter().zip(&places).filter(|(_, p)| p.is_some()).all(|(e, p)| {
                    let k = &kernels[p.unwrap()];
                    k.dist_col().is_some_and(|d| d.0 == c) || super::need::of([e]).is_some_and(|n| !n.get(c).copied().unwrap_or(false))
                })
            })
            .collect();
        // (The sort's key among the computed values.)
        let topk = self.topk.and_then(|t| Some(super::TopK { col: (*places.get(t.col)?)?, ..t }));
        if let Some(base) = self.input.compute_in_workers(super::Computed { extra: kernels, drop, raw, topk }, ctx)? {
            self.computed = Some((base, places));
        }
        Ok(())
    }
}
/// Whether a filter or an inner join's condition has quals that gate it (`GateNode`).
fn gating(pred: &Expr) -> bool {
    let gate = crate::sql::binder::optimize::pseudo_constant;
    match pred {
        Expr::And(list) => list.iter().any(gate),
        e => gate(e),
    }
}

/// The gating quals of `pred` (`gating`).
fn gates_of(pred: &Expr) -> Vec<Expr> {
    match pred {
        Expr::And(list) => list.iter().filter(|e| gating(e)).cloned().collect(),
        e => vec![e.clone()],
    }
}

/// The quals of `pred` that don't gate it (`gating`), which still run per row.
fn rest_of(pred: &Expr) -> Option<Expr> {
    let mut rest: Vec<Expr> = match pred {
        Expr::And(list) => list.iter().filter(|e| !gating(e)).cloned().collect(),
        _ => Vec::new(),
    };
    match rest.len() {
        0 => None,
        1 => rest.pop(),
        _ => Some(Expr::And(rest)),
    }
}

/// A node gated by pseudoconstant quals (a Result's One-Time Filter): they are evaluated at
/// the first row asked for, and the input is opened (and run) only when they all hold.
/// A filter's other quals (`filter`) then filter the input's rows.
struct GateNode {
    plan: Arc<Plan>,
    need: Option<Vec<bool>>,
    gates: Vec<Expr>,
    filter: Option<Expr>,
    input: Option<BoxNode>,
}

impl Node for GateNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.input.is_none() {
            for g in &self.gates {
                if !eval_bool(g, &[], ctx)? {
                    self.input = Some(Box::new(VecNode::new(Vec::new())));
                    return Ok(None);
                }
            }
            self.input = Some(open_need(&self.plan, self.need.take(), ctx)?);
        }
        let input = self.input.as_mut().unwrap();
        while let Some(r) = input.next(ctx)? {
            match &self.filter {
                Some(p) => {
                    ctx.check()?;
                    if eval_bool(p, &r, ctx)? {
                        return Ok(Some(r));
                    }
                }
                None => return Ok(Some(r)),
            }
        }
        Ok(None)
    }
}


impl Node for ProjectNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if !self.tried {
            self.compute_below(ctx)?;
        }
        let Some(mut r) = self.input.next(ctx)? else {
            return Ok(None);
        };
        // A row with the values computed has them after its columns (none of them where a
        // kernel left one to `eval`: all evaluated here, as without the workers).
        if let Some((base, places)) = &self.computed
            && r.len() > *base
        {
            let extra = r.split_off(*base);
            let mut out = Vec::with_capacity(self.exprs.len());
            for (e, p) in self.exprs.iter().zip(places) {
                out.push(match p {
                    Some(j) => extra[*j].clone(),
                    None => eval(e, &r, ctx)?,
                });
            }
            return Ok(Some(out));
        }
        if let Some(m) = &self.moves {
            return project_moving(&self.exprs, m, r, ctx).map(Some);
        }
        Ok(Some(eval_all(&self.exprs, &r, ctx)?))
    }

    fn projection(&self) -> Option<&[Expr]> {
        (!self.tried).then_some(&self.exprs[..])
    }

    fn pipeline_projected(&mut self, spec: Arc<super::partial::Spec>, ctx: &ExecCtx) -> Result<Option<u8>> {
        if self.tried {
            return Ok(None);
        }
        let r = self.input.pipeline(Vec::new(), Some(spec), ctx)?;
        if r.is_some() {
            self.tried = true;
            self.piped = true;
            self.moves = project_moves(&self.exprs);
        }
        Ok(r)
    }

    fn next_item(&mut self, ctx: &ExecCtx) -> Result<Option<super::pipe::Item>> {
        if !self.piped {
            return Ok(self.next(ctx)?.map(|r| super::pipe::Item::Row(r, super::pipe::ALL)));
        }
        projected_item(self, ctx)
    }

    fn topk_below(&mut self, t: super::TopK) -> bool {
        if self.tried || (t.keep_ties && !self.exprs.iter().all(direct_numeric_col)) {
            return false;
        }
        self.topk = Some(t);
        true
    }

    fn take_skipped(&mut self) -> usize {
        self.input.take_skipped()
    }
}

/// Start a set-returning call (or a plain function, as one row): in FROM (`scan`, a function
/// scan) or in a select list (ProjectSet).
pub fn start_srf(call: &SrfCall, row: &[Value], scan: bool, ctx: &ExecCtx) -> Result<SrfIter> {
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
        // (Postgres's frames between a function's call and its body's: a function scan's or
        // ProjectSet's executor, ExecMakeTableFunctionResult / ExecMakeFunctionResultSet.)
        let _frame = crate::stack::FnFrame::hold(if scan {
            crate::stack::PG_FUNCSCAN_FRAME
        } else {
            crate::stack::PG_PROJECTSET_FRAME
        });
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
                    .map(|s| start_srf(s, &row, false, ctx).map(Some))
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
    /// A call returning a one-field composite whose field isn't composite: a row value it
    /// returns is unwrapped to the field (unnest of an array of it; Hibernate).
    one_field: Vec<bool>,
    ordinality: bool,
    n: i64,
    /// Per call, its column definition list's domains (`SrfCall::domains`), checked on
    /// every row it returns.
    domains: Vec<Vec<Option<Arc<crate::sql::expr::DomainCoerce>>>>,
    /// Per call, whether its rows are all made at the first fetch, as ExecMakeTableFunctionResult
    /// fills its tuplestore: a function failing on a later row fails before any row goes
    /// up (no row inserted, no row sent). Series and unnest, which can't fail midway, stream.
    materialize: Vec<bool>,
    started: bool,
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
                    return start_srf(c, &[], true, ctx).map(Some);
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
                        let rows: Vec<Row> = start_srf(c, &[], true, ctx)?.collect::<Result<_>>()?;
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
            one_field: calls
                .iter()
                .map(|c| {
                    matches!(&c.ty, crate::types::DataType::Row(r) if r.fields.len() == 1
                        && !matches!(r.fields[0].1, crate::types::DataType::Row(_) | crate::types::DataType::Record))
                })
                .collect(),
            ordinality,
            n: 0,
            materialize: calls
                .iter()
                .map(|c| c.body.is_some() || !matches!(c.func.name, "generate_series" | "unnest" | "generate_subscripts"))
                .collect(),
            started: false,
            domains: calls
                .iter()
                .map(|c| {
                    let mut b = crate::sql::binder::Binder::new(ctx, Vec::new());
                    c.domains.iter().map(|d| d.map_or(Ok(None), |d| b.domain_coerce(d))).collect()
                })
                .collect::<Result<_>>()?,
        })
    }
}

impl Node for FunctionScanNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        ctx.check()?;
        if !self.started {
            self.started = true;
            for ((((it, m), n), one), doms) in
                self.iters.iter_mut().zip(&self.materialize).zip(&self.ncols).zip(&self.one_field).zip(&self.domains)
            {
                if *m && let Some(i) = it.take() {
                    // (Shaped and checked against the domains here: a later row's failure
                    // comes before the first row goes up.)
                    let rows = i
                        .map(|r| {
                            let r = srf_columns(r?, *n, *one);
                            check_domains(&r, doms, ctx)?;
                            Ok(r)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    *it = Some(Box::new(rows.into_iter().map(Ok)));
                }
            }
        }
        let mut any = false;
        let mut row = Vec::new();
        for (((it, n), one), doms) in self.iters.iter_mut().zip(&self.ncols).zip(&self.one_field).zip(&self.domains) {
            match it.as_mut().and_then(|i| i.next()) {
                Some(r) => {
                    any = true;
                    let r = srf_columns(r?, *n, *one);
                    check_domains(&r, doms, ctx)?;
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

/// A function scan call's row as its `n` columns: a function returning a composite value
/// (unnest of a row array) has its fields as the columns.
fn srf_columns(mut r: Row, n: usize, one: bool) -> Row {
    if (n > 1 || one)
        && r.len() == 1
        && let Value::Record(f) = &r[0]
    {
        r = f.to_vec();
    }
    r.resize(n, Value::Null);
    r
}

/// A column definition list's domains checked over a call's row (`SrfCall::domains`).
fn check_domains(r: &[Value], doms: &[Option<Arc<crate::sql::expr::DomainCoerce>>], ctx: &ExecCtx) -> Result<()> {
    for (v, d) in r.iter().zip(doms) {
        if let Some(d) = d {
            d.check(v, ctx)?;
        }
    }
    Ok(())
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
    // Keys that are the rows' own columns: the rows compared in place (the same
    // comparisons, so the same order), without a key vector per row.
    let cols: Option<Vec<usize>> = keys
        .iter()
        .map(|k| match k.expr {
            Expr::Col(i, _) => Some(i),
            _ => None,
        })
        .collect();
    if let Some(cols) = cols
        && rows.iter().all(|r| cols.iter().all(|&c| c < r.len()))
    {
        let mut f = |a: &Row, b: &Row| -> Result<Ordering> {
            for (k, &c) in keys.iter().zip(&cols) {
                match compare_values(&a[c], &b[c], k, ctx)? {
                    Ordering::Equal => {}
                    o => return Ok(o),
                }
            }
            Ok(Ordering::Equal)
        };
        if let Some(order) = super::fastsort::order(rows.len(), keys, |i, k| &rows[i][cols[k]], |i| &rows[i], bound) {
            return Ok(super::fastsort::permute(rows, &order));
        }
        let mut cmp = super::pgsort::Cmp::new(&mut f);
        let sorted = super::pgsort::tuplesort(rows, bound, &mut cmp);
        if let Some(e) = cmp.err {
            return Err(e);
        }
        return Ok(sorted);
    }
    let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
    for r in rows {
        let k = keys
            .iter()
            .map(|k| eval(&k.expr, &r, ctx))
            .collect::<Result<Vec<_>>>()?;
        keyed.push((k, r));
    }
    if let Some(order) = super::fastsort::order(keyed.len(), keys, |i, k| &keyed[i].0[k], |i| &keyed[i].1, bound) {
        let rows: Vec<Row> = keyed.into_iter().map(|(_, r)| r).collect();
        return Ok(super::fastsort::permute(rows, &order));
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

/// `sort_rows_bounded` of the input's rows (`bound` > 0), read a row at a time into
/// tuplesort's bounded sort (`pgsort::Bounded`), so that only the rows that may still be
/// among the first `bound` are kept: the same rows in the same order.
/// (Also the rows read, those a parallel scan's heap skipped included.)
fn sort_input_bounded(input: &mut BoxNode, keys: &[SortKey], bound: usize, ctx: &ExecCtx) -> Result<(Vec<Row>, usize)> {
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
    let mut sort = super::pgsort::Bounded::new(bound);
    // (The key vector of a row discarded at once serves the next.)
    let mut spare = Vec::with_capacity(keys.len());
    let mut n = 0;
    while let Some(r) = input.next(ctx)? {
        ctx.check()?;
        n += 1;
        let mut key: Vec<Value> = std::mem::take(&mut spare);
        key.clear();
        for k in keys {
            key.push(eval(&k.expr, &r, ctx)?);
        }
        if let Some((k, _)) = sort.put((key, r), &mut cmp) {
            spare = k;
        }
    }
    let skipped = input.take_skipped();
    sort.skip(skipped);
    let sorted = sort.finish(&mut cmp);
    if let Some(e) = cmp.err {
        return Err(e);
    }
    Ok((sorted.into_iter().map(|(_, r)| r).collect(), n + skipped))
}

/// An Incremental Sort's rows: the input sorted on the first `presorted` keys already, read
/// only as far as the sort needs (a LIMIT above may stop it early). `key`: the node's, for
/// EXPLAIN ANALYZE's counts of its sorts.
fn incremental_sort_rows(
    input: &mut BoxNode,
    keys: &[SortKey],
    presorted: usize,
    bound: Option<usize>,
    key: usize,
    ctx: &ExecCtx,
) -> Result<Vec<Row>> {
    let mut read_err: Option<crate::error::Error> = None;
    let keyed = std::iter::from_fn(|| {
        if read_err.is_some() {
            return None;
        }
        let r = (|| -> Result<Option<(Vec<Value>, Row)>> {
            let Some(r) = input.next(ctx)? else { return Ok(None) };
            ctx.check()?;
            let k = keys
                .iter()
                .map(|k| eval(&k.expr, &r, ctx))
                .collect::<Result<Vec<_>>>()?;
            Ok(Some((k, r)))
        })();
        match r {
            Ok(x) => x,
            Err(e) => {
                read_err = Some(e);
                None
            }
        }
    });
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
    let mut stats = super::pgsort::IncrStats::default();
    let out = super::pgsort::incremental_sort(keyed, bound, &mut cf, &mut cs, &mut same, &mut stats);
    if let Some(e) = read_err.or(err).or(cf.err).or(cs.err) {
        return Err(e);
    }
    ctx.count(key, |st| {
        for g in 0..2 {
            st.incr.groups[g] += stats.groups[g];
            st.incr.methods[g] |= stats.methods[g];
            st.incr.kept_total[g] += stats.kept_total[g];
            st.incr.kept_max[g] = st.incr.kept_max[g].max(stats.kept_max[g]);
        }
    });
    Ok(out.into_iter().map(|(_, r)| r).collect())
}

/// FinishSortSupportFunction's error for a sort key of made type `ty` whose ordering
/// operator's btree family has no comparison function (FUNCTION 1) for it.
fn missing_sort_support(ty: &crate::types::DataType, ctx: &ExecCtx) -> Option<Error> {
    let crate::types::DataType::Base(b) = ty else {
        return None;
    };
    let fam = ctx.catalog().sort_support_missing(b.oid)?;
    Some(crate::error::internal(format!(
        "missing support function 1({},{}) in opfamily {fam}",
        b.oid, b.oid
    )))
}

struct SortNode {
    input: Option<BoxNode>,
    keys: Vec<SortKey>,
    out: super::extsort::Sorted,
    /// The input's column types (what tuplesort counts its tuples by), and the columns
    /// read above (PostgreSQL's plan has the others as NULLs).
    types: Vec<crate::types::DataType>,
    mask: Option<Vec<bool>>,
    /// tuplesort_set_bound: the rows a LIMIT above wants.
    bound: Option<usize>,
    /// An Incremental Sort: the leading keys the input is sorted on already.
    presorted: usize,
    /// The plan node's address (EXPLAIN ANALYZE's key).
    key: usize,
    /// The input columns read above the sort and by its keys (None: all).
    need: Option<Box<[bool]>>,
    /// The full sort held its rows narrowed to `need` (`Narrow`; its keys over them): they
    /// come out widened.
    narrow: Option<(Narrow, Vec<SortKey>)>,
}

/// Rows held with only the columns read of them: a full sort's (the columns read above it
/// and its keys) and a hash join's inner rows (its output's, condition's and keys'). The
/// others are NULL in what the node gives out anyway (`need`); a row of a wide join (q10's
/// sort holds 37 columns, 9 of them read) took 32 bytes a column.
pub(super) struct Narrow {
    /// The columns kept, in order.
    cols: Box<[u32]>,
    width: usize,
}

impl Narrow {
    /// For rows of `width` columns: None when too few columns would go (the copies cost
    /// more than they save).
    pub(super) fn of(need: &[bool], width: usize) -> Option<Narrow> {
        let cols: Box<[u32]> = (0..width).filter(|&i| need.get(i).copied().unwrap_or(false)).map(|i| i as u32).collect();
        (width - cols.len() >= 4).then_some(Narrow { cols, width })
    }

    /// The columns a narrowed row keeps.
    pub(super) fn kept(&self) -> usize {
        self.cols.len()
    }

    /// Column `i`'s place in a narrowed row.
    pub(super) fn place(&self, i: usize) -> Option<usize> {
        self.cols.binary_search(&(i as u32)).ok()
    }

    /// Each column's place in a narrowed row (`u32::MAX`: not kept).
    pub(super) fn places(&self) -> Box<[u32]> {
        let mut m = vec![u32::MAX; self.width];
        for (p, &c) in self.cols.iter().enumerate() {
            m[c as usize] = p as u32;
        }
        m.into()
    }

    /// `e` over the narrowed rows, when every column it reads is kept.
    pub(super) fn expr(&self, e: &Expr) -> Option<Expr> {
        if let Expr::Col(i, t) = e {
            return Some(Expr::Col(self.place(*i)?, t.clone()));
        }
        let mut read = Vec::new();
        if !super::need::mark(e, &mut read) || read.iter().enumerate().any(|(i, &r)| r && self.place(i).is_none()) {
            return None;
        }
        Some(e.clone().remap_cols(&|i| self.place(i).unwrap_or(usize::MAX)))
    }

    pub(super) fn shrink(&self, mut r: Row) -> Row {
        debug_assert_eq!(r.len(), self.width);
        self.cols.iter().map(|&c| std::mem::replace(&mut r[c as usize], Value::Null)).collect()
    }

    fn widen(&self, r: Row) -> Row {
        let mut out = vec![Value::Null; self.width];
        for (v, &c) in r.into_iter().zip(&*self.cols) {
            out[c as usize] = v;
        }
        out
    }
}

/// How a full sort holds rows of `width` columns: narrowed (`Narrow`, its keys over the
/// narrowed rows) when `need` leaves enough columns out.
fn narrow_sort(need: Option<&[bool]>, width: usize, keys: &[SortKey]) -> Option<(Narrow, Vec<SortKey>)> {
    need.and_then(|n| Narrow::of(n, width)).and_then(|n| {
        let keys = keys
            .iter()
            .map(|k| Some(SortKey { expr: n.expr(&k.expr)?, ..k.clone() }))
            .collect::<Option<Vec<_>>>()?;
        Some((n, keys))
    })
}

impl SortNode {
    /// The next sorted row, widened back when held narrowed.
    fn next_out(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        Ok(match (self.out.next(ctx)?, &self.narrow) {
            (Some(r), Some((n, _))) => Some(n.widen(r)),
            (r, _) => r,
        })
    }
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
            // PrepareSortSupportFromOrderingOp: a made type's key needs its family's
            // comparison function, looked up as the sort starts.
            if let Some(e) = self.keys.iter().find_map(|k| missing_sort_support(&k.expr.ty(), ctx)) {
                return Err(e);
            }
            self.out = super::extsort::Sorted::Mem(if self.presorted > 0 {
                incremental_sort_rows(&mut input, &self.keys, self.presorted, self.bound, self.key, ctx)?
            } else {
                match self.bound.filter(|&b| b > 0) {
                    Some(b) => {
                        // A numeric multikey sort can prune by its first key only when
                        // ties still reach the full sort for the remaining comparisons.
                        let keep_ties = self.keys.len() > 1;
                        if let Some(k) = self.keys.first()
                            && k.using.is_none()
                            && let Expr::Col(col, _) = k.expr
                            && (!keep_ties || self.keys.iter().all(|k| k.using.is_none() && direct_numeric_col(&k.expr)))
                        {
                            input.topk_below(super::TopK { col, desc: k.desc, nulls_first: k.nulls_first, k: b, keep_ties });
                        }
                        let (rows, n) = sort_input_bounded(&mut input, &self.keys, b, ctx)?;
                        if let Some(instr) = ctx.instr.borrow().as_ref() {
                            let st = super::extsort::bounded_stats(&rows, n, b, &self.keys, &self.types, ctx);
                            instr.update(self.key, |s| s.sort = Some(st));
                        }
                        rows
                    }
                    None => {
                        // tuplesort: in memory while work_mem holds the rows, else runs on
                        // disk merged as they're read; the rows held narrowed to the columns
                        // read (`Narrow`), decided at the first.
                        let mut sort: Option<super::extsort::ExtSort> = None;
                        while let Some(r) = input.next(ctx)? {
                            ctx.check()?;
                            let s = match &mut sort {
                                Some(s) => s,
                                None => {
                                    self.narrow = narrow_sort(self.need.as_deref(), r.len(), &self.keys);
                                    sort.insert(super::extsort::ExtSort::new(
                                        &self.keys,
                                        self.types.clone(),
                                        self.mask.clone(),
                                        self.narrow.as_ref(),
                                        false,
                                        ctx,
                                    ))
                                }
                            };
                            s.put(r, self.narrow.as_ref().map(|(n, _)| n), ctx)?;
                        }
                        let sort = sort.unwrap_or_else(|| {
                            super::extsort::ExtSort::new(&self.keys, self.types.clone(), None, None, false, ctx)
                        });
                        let (out, method, kb) = sort.finish(ctx)?;
                        if let Some(instr) = ctx.instr.borrow().as_ref() {
                            instr.update(self.key, |s| s.sort = Some((method, kb)));
                        }
                        self.out = out;
                        return self.next_out(ctx);
                    }
                }
            }
            .into_iter());
        }
        self.next_out(ctx)
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

/// A Sort over a plain DISTINCT that EXPLAIN shows as a Unique over a Sort (the ORDER BY's
/// keys, then the other output columns): when equal values may differ, the DISTINCT keeps
/// the first of each group in that sort's order, not the first it reads, so it sorts its
/// input so first. (The Sort above then finds its rows already in order.)
fn distinct_presort(plan: &Arc<Plan>, ctx: &ExecCtx) -> Option<BoxNode> {
    let Plan::Sort { input, keys } = &**plan else { return None };
    let Plan::Distinct { input: below, on: None, .. } = &**input else { return None };
    let types = row_types(below);
    if types.iter().all(equal_is_identical) || !crate::sql::explain::sort_is_unique(plan, ctx) {
        return None;
    }
    let mut all = keys.clone();
    for (i, t) in types.iter().enumerate() {
        if !keys.iter().any(|k| matches!(k.expr, Expr::Col(c, _) if c == i)) {
            all.push(SortKey { expr: Expr::Col(i, t.clone()), desc: false, nulls_first: false, using: None });
        }
    }
    let mut d = match &**input {
        Plan::Distinct { input: below, on, hashed, .. } => DistinctNode {
            input: open(below, ctx).ok()?,
            on: on.clone(),
            seen: HashMap::default(),
            prev: None,
            hashed: (on.is_none() || *hashed).then(|| input.clone()),
            out: None,
            presort: None,
        },
        _ => return None,
    };
    d.presort = Some(all);
    Some(Box::new(d))
}

/// Whether two values of a type that compare equal are always the same value (so which of
/// them a DISTINCT keeps can't be seen).
fn equal_is_identical(t: &crate::types::DataType) -> bool {
    use crate::types::DataType as T;
    match t {
        T::Bool | T::Int2 | T::Int4 | T::Int8 | T::Text | T::Varchar(_) | T::Name | T::Char | T::Bytea | T::Date
        | T::Time(_) | T::Timetz(_) | T::Timestamp(_) | T::Timestamptz(_) | T::Uuid | T::Oid => true,
        t => t.elem().is_some_and(|e| !matches!(e, T::Unknown) && e != t && equal_is_identical(e)),
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
    /// Under a Sort EXPLAIN shows it as a Unique over (`distinct_presort`): all rows sorted
    /// on these keys first, the first of each group kept in that order.
    presort: Option<Vec<SortKey>>,
}

/// A plan's column types as far as its nodes say (`col_types`, through a UNION ALL's first
/// arm too), for comparing its rows.
fn row_types(p: &Plan) -> Vec<crate::types::DataType> {
    match p {
        Plan::Append { inputs, .. } if !inputs.is_empty() => row_types(&inputs[0]),
        Plan::Filter { input, .. } | Plan::Sort { input, .. } | Plan::Limit { input, .. } | Plan::Barrier { input, .. } => {
            row_types(input)
        }
        _ => crate::sql::paths::analyzejoins::col_types(p),
    }
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
    hashed_distinct_keyed(rows.into_iter().map(|r| (key(&r), r)), types, plan, ctx)
}

/// `hashed_distinct` over rows paired with their keys.
fn hashed_distinct_keyed(
    rows: impl IntoIterator<Item = (Row, Row)>,
    types: Vec<crate::types::DataType>,
    plan: &Arc<Plan>,
    ctx: &ExecCtx,
) -> Result<Vec<Row>> {
    let mut index: HashMap<Row, usize> = HashMap::default();
    let mut firsts: Vec<Row> = Vec::new();
    let mut order = super::pghash::GroupOrder::new(types, ctx);
    // (Each group's hash taken at its first row: equal keys hash alike.)
    let mut hashes: Vec<u32> = Vec::new();
    for (k, r) in rows {
        ctx.check()?;
        let g = match index.get(&k) {
            Some(g) => *g,
            None => {
                firsts.push(r);
                hashes.push(order.hash(&k));
                index.insert(k, firsts.len() - 1);
                firsts.len() - 1
            }
        };
        order.lookup(hashes[g], g);
    }
    // (A HashAggregate without aggregates.)
    let e = super::pghash::agg_estimate(plan, ctx);
    let (est, width) = (e.rows, e.input_width);
    let sizing = super::pghash::AggSizing {
        input_width: width,
        num_trans: 0,
        trans_space: 0.0,
        num_hashes: 1,
        total_groups: est,
    };
    let mut slots: Vec<Option<Row>> = firsts.into_iter().map(Some).collect();
    Ok(order
        .order(sizing.nbuckets(est, super::pghash::hash_mem(ctx)))
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
    // (Postgres's quicksort: rows with equal keys in the order its Sort leaves them.)
    let mut f = |a: &Row, b: &Row| -> Result<Ordering> {
        for &c in cols {
            match compare_values(&a[c], &b[c], &key, ctx)? {
                Ordering::Equal => {}
                o => return Ok(o),
            }
        }
        Ok(Ordering::Equal)
    };
    let mut cmp = super::pgsort::Cmp::new(&mut f);
    super::pgsort::pg_qsort(rows, &mut cmp);
    match cmp.err {
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
            // (Whole rows compare as their columns do: bpchar without trailing spaces.)
            let norms: Vec<Option<super::agg::KeyNorm>> = row_types(&input)
                .iter()
                .map(|t| matches!(t, crate::types::DataType::Bpchar(_)).then_some(super::agg::KeyNorm::Bpchar(None)))
                .collect();
            let whole = |r: &Row| super::agg::group_key(r, &norms);
            // (Grouped on keys, the rest passed through: each group's first row whole.)
            let out = if let Some(keys) = self.presort.take() {
                let mut seen: HashMap<Row, ()> = HashMap::default();
                let mut out: Vec<Row> = Vec::new();
                for r in sort_rows(rows, &keys, ctx)? {
                    if seen.insert(whole(&r), ()).is_none() {
                        out.push(r);
                    }
                }
                out
            } else if let Some(on) = &self.on {
                let colls = super::agg::nondeterministic_keys(on, ctx);
                let mut keyed = Vec::with_capacity(rows.len());
                for r in rows {
                    let k = super::agg::group_key(&eval_all(on, &r, ctx)?, &colls);
                    keyed.push((k, r));
                }
                let types: Vec<crate::types::DataType> = on.iter().map(Expr::ty).collect();
                hashed_distinct_keyed(keyed, types, &plan, ctx)?
            // (Shown as a Unique over its input sorted: the rows in that order. With values
            // that compare equal but differ (`'1 mon'` / `'30 days'`, `1.0` / `1.00`), the
            // one kept is the first of its run in that order, so all rows are sorted first.)
            } else if let Some(cols) = crate::sql::explain::distinct_order(&plan, ctx) {
                let ties = row_types(&input).iter().any(|t| !equal_is_identical(t));
                let mut seen: HashMap<Row, ()> = HashMap::default();
                let mut out: Vec<Row> = Vec::new();
                if ties {
                    let mut all = rows;
                    sort_rows_by(&mut all, &cols, ctx)?;
                    for r in all {
                        if seen.insert(whole(&r), ()).is_none() {
                            out.push(r);
                        }
                    }
                } else {
                    for r in rows {
                        if seen.insert(whole(&r), ()).is_none() {
                            out.push(r);
                        }
                    }
                    sort_rows_by(&mut out, &cols, ctx)?;
                }
                out
            } else {
                let types = output_types(&input, rows.first());
                hashed_distinct(rows, whole, types, &plan, ctx)?
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
                    Plan::Append { inputs, inh: None, .. } => {
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
        } else if matches!(self.kind, SetOpKind::Intersect | SetOpKind::Except) {
            // nodeSetOp: the first input read (EXCEPT's left; INTERSECT's with fewer groups,
            // generate_nonunion_paths) being empty, the other one is never read (nor what
            // evaluating it would raise).
            let swapped = self.kind == SetOpKind::Intersect
                && crate::sql::explain::setop_shape(&self.left, &self.right, true, ctx).0;
            let (first, second) = if swapped { (&self.right, &self.left) } else { (&self.left, &self.right) };
            let a = collect(first, ctx, None)?;
            if a.is_empty() {
                return Ok(Vec::new());
            }
            let b = collect(second, ctx, None)?;
            if swapped { (b, a) } else { (a, b) }
        } else {
            (collect(&self.left, ctx, None)?, collect(&self.right, ctx, None)?)
        };
        // Rows compare by their keys: strings under a nondeterministic collation by what it
        // finds equal, bpchar without trailing spaces.
        let types = row_types(&self.left);
        let width = types.len().max(self.nondet.len());
        let cat = (!self.nondet.is_empty() || types.iter().any(|t| t.holds_composite())).then(|| ctx.catalog());
        let colls: Vec<Option<super::agg::KeyNorm>> = (0..width)
            .map(|i| {
                let c = match (&cat, self.nondet.get(i), types.get(i)) {
                    (Some(cat), Some(Some(n)), _) => crate::types::collation::by_name(cat, n),
                    // (Composites with fields under nondeterministic collations.)
                    (Some(cat), _, Some(t)) if t.holds_composite() => {
                        crate::types::collation::for_type(cat, t).filter(|c| !c.deterministic)
                    }
                    _ => None,
                };
                if matches!(types.get(i), Some(crate::types::DataType::Bpchar(_))) {
                    Some(super::agg::KeyNorm::Bpchar(c))
                } else {
                    super::agg::KeyNorm::of(c)
                }
            })
            .collect();
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
                    let mut seen: HashMap<Row, ()> = HashMap::default();
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
                        let mut seen: HashMap<Row, ()> = HashMap::default();
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
                let mut counts: HashMap<Row, usize> = HashMap::default();
                for r in right {
                    *counts.entry(key(&r)).or_insert(0) += 1;
                }
                let mut emitted: HashMap<Row, ()> = HashMap::default();
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
        let mut index: HashMap<Row, usize> = HashMap::default();
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
        let mut out = Vec::new();
        for g in order.order(super::pghash::setop_nbuckets(est, super::pghash::hash_mem(ctx))) {
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
    /// The first Async Foreign Scan member (the members' requests sent when the Append
    /// starts) and the members' nodes opened then, by place.
    first_async: Option<usize>,
    pre: Vec<(usize, BoxNode)>,
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
                // (ExecAppendAsyncBegin: each async member's request goes out now, one per
                // connection: a member sharing one sends its own when its turn comes.)
                if self.i == 0
                    && let Some(k) = self.first_async
                {
                    let mut used = Vec::new();
                    for j in k..self.inputs.len() {
                        let q = self.inputs[j].clone();
                        let Some(key) = crate::ext::postgres_fdw::scan::async_conn(&q, ctx) else { continue };
                        if used.contains(&key) {
                            continue;
                        }
                        used.push(key);
                        let mut n = open(&q, ctx)?;
                        n.prefetch(ctx)?;
                        self.pre.push((j, n));
                    }
                }
                ctx.defer_after.set(self.dml);
                ctx.capture.set(self.capture && self.i > 0);
                let pre = self.pre.iter().position(|(j, _)| *j == self.i).map(|x| self.pre.remove(x).1);
                let opened = match pre {
                    Some(n) => Ok(n),
                    None => open(p, ctx),
                };
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
            // (Its index conditions too: a parameterized scan's join clauses.)
            Plan::Scan(s) => {
                let mut quals: Vec<Expr> = s.filter.iter().cloned().collect();
                if let crate::sql::plan::Access::Index { quals: iq, .. } = &s.access {
                    for q in iq {
                        if !quals.iter().any(|x| x.same_as(&q.orig)) {
                            quals.push(q.orig.clone());
                        }
                    }
                }
                Some((s, quals))
            }
            // (A UNION ALL arm's projection: whatever it computes, the scan's quals are its
            // own.)
            Plan::Project { input, .. } if matches!(&**input, Plan::Scan(_)) => member(input),
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
    // (An Append of one member is no Append at all: set_append_references removes it, and
    // its run-time pruning with it.)
    if inputs.len() == 1 {
        return inputs.to_vec();
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

/// A join condition's kernel, boxed and compiled apart: `open_node`, on the stack of every
/// plan level (and of every level of a recursive SQL function), stays its size.
#[inline(never)]
fn compile_boxed(cond: Option<&Expr>) -> Option<Box<super::kernel::K>> {
    cond.and_then(super::kernel::compile).map(Box::new)
}

/// An outer and an inner row read as their concatenation (`NestLoopNode::cond_kernel`).
struct Pair<'a>(&'a [Value], &'a [Value]);

impl super::kernel::Cols for Pair<'_> {
    #[inline]
    fn col(&self, i: usize) -> Option<&Value> {
        if i < self.0.len() { self.0.get(i) } else { self.1.get(i - self.0.len()) }
    }
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
    /// Per key: its rows, its place in the LRU order, its bytes.
    cache: HashMap<Vec<Value>, (Rc<Vec<Row>>, u64, u64)>,
    /// Cache Mode: binary: keys equal only bit for bit (-0.0 isn't 0.0).
    binary: bool,
    /// The entries by last use (oldest first), the bytes they take and hash_mem's limit:
    /// past it the least recently used go (cache_reduce_memory).
    lru: std::collections::BTreeMap<u64, Vec<Value>>,
    stamp: u64,
    mem: u64,
    limit: u64,
}

/// Nested loop: the right side is materialised once, or re-run per left row for LATERAL.
struct NestLoopNode {
    kind: JoinKind,
    left: BoxNode,
    right_plan: Arc<Plan>,
    cond: Option<Expr>,
    /// The condition compiled (`exec::kernel`), checked on the outer and inner row in place:
    /// a pair is copied into one row only once it passes (a catalog join's filter rejecting
    /// thousands of pairs of wide rows copied each).
    cond_kernel: Option<Box<super::kernel::K>>,
    params: Vec<Expr>,
    nleft: usize,
    nright: usize,
    right: Option<Rc<Vec<Row>>>,
    right_matched: Vec<bool>,
    /// The outer row, its inner rows (with the next one's place) and whether one matched.
    cur: Option<(Row, InnerRows, bool)>,
    /// A parameterized inner side kept open across outer rows and rescanned for each
    /// (`streams`), once opened.
    inner: Option<BoxNode>,
    unmatched_right: Option<usize>,
    memo: Option<Memoize>,
    /// A RIGHT / FULL join whose right side reads only an enclosing query's values: run
    /// once, its rows' matches kept.
    fixed: bool,
    /// Its plan's instrumentation key (EXPLAIN ANALYZE's rows the join filter removed).
    key: usize,
    /// The joined rows' columns anything reads (None: all).
    keep: Option<Box<[bool]>>,
    /// Its inner scans run in a parallel scan's workers below (`lookup`): the level of its
    /// rows (`pipe::ALL`: not), and the stage (its statistics are counted as it ends).
    level: u8,
    tried: bool,
    stage: Option<Arc<super::pipe::Stage>>,
}

/// A nested loop's inner rows for one outer row: collected (a Material's, a memo's, a
/// fixed or unparameterized side's), or read from the inner node as the join asks, under
/// the outer row's parameters.
enum InnerRows {
    Rows(Rc<Vec<Row>>, usize),
    Stream(Rc<Row>),
}

impl NestLoopNode {
    /// Whether the inner side is read as ExecNestLoop reads it: rescanned per outer row
    /// (ExecReScan with the new parameters) and its rows taken as the join asks, not
    /// collected first. A parameterized side without a memo (Memoize collects) whose plan
    /// can rescan; not under EXPLAIN ANALYZE (each open counts a loop) or outside a
    /// running statement (collecting runs it as one).
    fn streams(&self, ctx: &ExecCtx) -> bool {
        !self.params.is_empty()
            && self.memo.is_none()
            && !self.fixed
            && !ctx.instrumented()
            && ctx.snapshot().is_some()
            && ctx.temp_cleanup.active()
            && super::scan::rescans(&self.right_plan)
    }

    /// The inner node read again for the outer row whose parameters are `p`: rescanned,
    /// or opened (the first time, or when it can't rescan).
    fn open_inner(&mut self, p: &Rc<Row>, ctx: &ExecCtx) -> Result<()> {
        if let Some(n) = self.inner.as_mut()
            && ctx.with_outer(p.clone(), || n.rescan(ctx))?
        {
            return Ok(());
        }
        // (The last one goes first: its scan's marks are made as it ends.)
        self.inner = None;
        let need = self.keep.as_ref().map(|k| (self.nleft..self.nleft + self.nright).map(|i| k.get(i).copied().unwrap_or(false)).collect());
        self.inner = Some(ctx.with_outer(p.clone(), || open_need(&self.right_plan, need, ctx))?);
        Ok(())
    }

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
            cond_kernel: compile_boxed(cond.as_ref()),
            cond,
            params,
            nleft,
            nright,
            right: None,
            right_matched: Vec::new(),
            cur: None,
            inner: None,
            unmatched_right: None,
            memo: None,
            fixed,
            key: 0,
            keep: None,
            level: super::pipe::ALL,
            tried: false,
            stage: None,
        }
    }

    /// Its inner scans as a stage a parallel scan's workers run (`lookup`), when they can
    /// be one.
    fn lookup_stage(&mut self, ctx: &ExecCtx) -> Result<Option<Arc<super::pipe::Stage>>> {
        // (A memoized inner side scans once per distinct key: its scans' counts differ.)
        if self.memo.is_some() || self.fixed || self.right.is_some() {
            return Ok(None);
        }
        let lk = super::lookup::Lookup::new(
            self.kind,
            &self.right_plan,
            &self.params,
            self.cond.as_ref(),
            self.nleft,
            self.nright,
            self.keep.clone(),
            ctx,
        )?;
        let st = lk.map(|l| Arc::new(super::pipe::Stage::Lookup(Box::new(l))));
        self.stage = st.clone();
        Ok(st)
    }

    /// The stage's statistics so far (`Lookup::flush`).
    fn flush(&self, ctx: &ExecCtx) {
        if let Some(st) = &self.stage
            && let super::pipe::Stage::Lookup(l) = &**st
        {
            l.flush(ctx);
        }
    }

    /// The inner side's rows, read with only the columns the join's output and condition
    /// read (the others NULL: a scan doesn't decode them).
    fn collect_right(&self, ctx: &ExecCtx) -> Result<Vec<Row>> {
        let need = self.keep.as_ref().map(|k| (self.nleft..self.nleft + self.nright).map(|i| k.get(i).copied().unwrap_or(false)).collect());
        super::collect_need(&self.right_plan, need, ctx, |r| r)
    }

    fn right_rows(&mut self, left: &[Value], ctx: &ExecCtx) -> Result<Rc<Vec<Row>>> {
        if self.fixed {
            if self.right.is_none() {
                let p = eval_all(&self.params, &[], ctx)?;
                let rows = ctx.with_outer(Rc::new(p), || self.collect_right(ctx))?;
                self.right_matched = vec![false; rows.len()];
                self.right = Some(Rc::new(rows));
            }
            return Ok(self.right.clone().unwrap());
        }
        if !self.params.is_empty() {
            let memo_key = match &self.memo {
                Some(m) => {
                    let mut k = eval_all(&m.keys, left, ctx)?;
                    if m.binary {
                        for v in k.iter_mut() {
                            match v {
                                Value::Float8(f) => *v = Value::Int8(f.to_bits() as i64),
                                Value::Float4(f) => *v = Value::Int4(f.to_bits() as i32),
                                _ => {}
                            }
                        }
                    }
                    Some(k)
                }
                None => None,
            };
            if let (Some(m), Some(k)) = (&mut self.memo, &memo_key)
                && let Some((rows, at, _)) = m.cache.get_mut(k)
            {
                // (Now the most recently used.)
                m.stamp += 1;
                let old = std::mem::replace(at, m.stamp);
                if let Some(kk) = m.lru.remove(&old) {
                    m.lru.insert(m.stamp, kk);
                }
                let rows = rows.clone();
                let (n, key) = (rows.len() as u64, m.key);
                ctx.count(key, |st| {
                    st.loops += 1;
                    st.rows += n;
                    st.cache_hits += 1;
                });
                return Ok(rows.clone());
            }
            let p = eval_all(&self.params, left, ctx)?;
            let rows = ctx.with_outer(Rc::new(p), || self.collect_right(ctx))?;
            let rows = Rc::new(rows);
            if let (Some(m), Some(k)) = (&mut self.memo, memo_key) {
                // EMPTY_ENTRY_MEMORY_BYTES and CACHE_TUPLE_BYTES.
                let mut bytes = (24 + 24 + minimal_len(&k, ctx)) as u64;
                for r in rows.iter() {
                    bytes += (16 + minimal_len(r, ctx)) as u64;
                }
                // (Over the limit: the least recently used entries go first.)
                m.mem += bytes;
                let mut evicted = 0u64;
                while m.mem > m.limit
                    && let Some((_, old)) = m.lru.pop_first()
                {
                    if let Some((_, _, b)) = m.cache.remove(&old) {
                        m.mem -= b;
                        evicted += 1;
                    }
                }
                let mem = m.mem;
                let (n, key) = (rows.len() as u64, m.key);
                ctx.count(key, |st| {
                    st.loops += 1;
                    st.rows += n;
                    st.cache_misses += 1;
                    st.cache_evictions += evicted;
                    st.cache_bytes = st.cache_bytes.max(mem);
                });
                m.stamp += 1;
                m.lru.insert(m.stamp, k.clone());
                m.cache.insert(k, (rows.clone(), m.stamp, bytes));
            }
            return Ok(rows);
        }
        if self.right.is_none() {
            let rows = self.collect_right(ctx)?;
            self.material_stats(&rows, ctx);
            self.right_matched = vec![false; rows.len()];
            self.right = Some(Rc::new(rows));
        }
        Ok(self.right.clone().unwrap())
    }

    /// EXPLAIN ANALYZE: what a Materialize over the inner side would have held, its
    /// tuplestore's tuples of the columns the join reads (`workmem::StoreMem`).
    fn material_stats(&self, rows: &[Row], ctx: &ExecCtx) {
        let Some(instr) = ctx.instr.borrow().clone() else { return };
        let cols: Option<Vec<bool>> =
            self.keep.as_ref().map(|k| (self.nleft..self.nleft + self.nright).map(|i| k.get(i).copied().unwrap_or(false)).collect());
        let types = crate::sql::paths::analyzejoins::col_types(&self.right_plan);
        let mut m = super::workmem::StoreMem::new(super::workmem::work_mem(ctx), false);
        let mut held: Vec<i64> = Vec::new();
        for r in rows {
            let len = super::workmem::minimal_tuple_len_cols(r, &types, cols.as_deref(), ctx);
            if m.disk.is_none() {
                held.push(len);
            }
            if m.put(len) {
                for l in held.drain(..) {
                    m.dumped(l);
                }
            }
        }
        let (disk, bytes) = m.stats();
        instr.update(Arc::as_ptr(&self.right_plan) as usize, |s| {
            let (d0, b0) = s.material.unwrap_or_default();
            s.material = Some((d0 || disk, b0.max(bytes as u64)));
        });
    }
}

impl NestLoopNode {
    /// The next row (with its level) or part.
    fn step(&mut self, ctx: &ExecCtx) -> Result<Option<super::pipe::Item>> {
        use super::pipe::Item;
        if !self.tried {
            self.tried = true;
            if let Some(st) = self.lookup_stage(ctx)?
                && let Some(l) = self.left.pipeline(vec![st], None, ctx)?
            {
                self.level = l + 1;
                super::lookup::note_started();
            } else {
                self.stage = None;
            }
        }
        let level = self.level;
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
                        return Ok(Some(Item::Row(out, level)));
                    }
                }
                return Ok(None);
            }
            if self.cur.is_none() {
                let next = match level {
                    super::pipe::ALL => self.left.next(ctx)?,
                    _ => match self.left.next_item(ctx)? {
                        None => None,
                        Some(Item::Part(p)) => return Ok(Some(Item::Part(p))),
                        // (Joined in the workers already.)
                        Some(Item::Row(r, l)) if l >= level => return Ok(Some(Item::Row(r, l))),
                        Some(Item::Row(r, _)) => Some(r),
                    },
                };
                match next {
                    Some(l) if self.streams(ctx) => {
                        let p = Rc::new(eval_all(&self.params, &l, ctx)?);
                        self.open_inner(&p, ctx)?;
                        self.cur = Some((l, InnerRows::Stream(p), false));
                    }
                    Some(l) => {
                        let rows = self.right_rows(&l, ctx)?;
                        self.cur = Some((l, InnerRows::Rows(rows, 0), false));
                    }
                    None => {
                        self.flush(ctx);
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
            let (l, rows, matched) = self.cur.as_mut().unwrap();
            loop {
                ctx.check()?;
                let streamed;
                let (j, r) = match rows {
                    InnerRows::Rows(rows, i) => {
                        let Some(r) = rows.get(*i) else { break };
                        *i += 1;
                        (*i - 1, r)
                    }
                    InnerRows::Stream(p) => {
                        let n = self.inner.as_mut().expect("an inner side open");
                        match ctx.with_outer(p.clone(), || n.next(ctx))? {
                            Some(r) => {
                                streamed = r;
                                (usize::MAX, &streamed)
                            }
                            None => break,
                        }
                    }
                };
                let pair = Pair(l, r);
                let early = self.cond_kernel.as_ref().and_then(|k| k.passes(&pair));
                let combined = match early {
                    Some(false) => None,
                    _ => Some(concat_rows(l, r)),
                };
                let ok = match (&self.cond, early, &combined) {
                    (_, Some(b), _) => b,
                    (Some(c), None, Some(combined)) => eval_bool(c, combined, ctx)?,
                    _ => true,
                };
                let Some(combined) = combined.filter(|_| ok) else {
                    if self.key != 0 {
                        ctx.filtered(self.key);
                    }
                    continue;
                };
                *matched = true;
                if (self.params.is_empty() || self.fixed) && j < self.right_matched.len() {
                    self.right_matched[j] = true;
                }
                match self.kind {
                    JoinKind::Semi => {
                        let out = l.clone();
                        self.cur = None;
                        return Ok(Some(Item::Row(out, level)));
                    }
                    JoinKind::Anti => {
                        self.cur = None;
                        break;
                    }
                    _ => return Ok(Some(Item::Row(combined, level))),
                }
            }
            let Some((l, _, matched)) = self.cur.take() else {
                continue;
            };
            if !matched {
                match self.kind {
                    JoinKind::Left | JoinKind::Full => {
                        let mut out = l;
                        out.extend(std::iter::repeat_n(Value::Null, self.nright));
                        return Ok(Some(Item::Row(out, level)));
                    }
                    JoinKind::Anti => return Ok(Some(Item::Row(l, level))),
                    _ => {}
                }
            }
        }
    }
}

impl Node for NestLoopNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        match self.step(ctx)? {
            None => Ok(None),
            Some(super::pipe::Item::Row(r, _)) => Ok(Some(r)),
            Some(super::pipe::Item::Part(_)) => Err(crate::error::internal("parts from a nested loop")),
        }
    }

    fn next_item(&mut self, ctx: &ExecCtx) -> Result<Option<super::pipe::Item>> {
        self.step(ctx)
    }

    fn pipeline(
        &mut self,
        above: Vec<Arc<super::pipe::Stage>>,
        spec: Option<Arc<super::partial::Spec>>,
        ctx: &ExecCtx,
    ) -> Result<Option<u8>> {
        if self.tried || self.cur.is_some() {
            return Ok(None);
        }
        let with_spec = spec.is_some();
        let Some(st) = self.lookup_stage(ctx)? else {
            self.tried = !with_spec;
            return Ok(None);
        };
        let mut stages = vec![st];
        stages.extend(above);
        match self.left.pipeline(stages, spec, ctx)? {
            Some(l) => {
                self.tried = true;
                self.level = l + 1;
                super::lookup::note_started();
                Ok(Some(l + 1))
            }
            None => {
                self.stage = None;
                self.tried = !with_spec;
                Ok(None)
            }
        }
    }
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
/// rows, at least 1024). `norms`: how each key's values are compared (`pipe::norms`).
fn build_hash(
    rows: Vec<Row>,
    keys: &[Expr],
    norms: &[super::pipe::Norm],
    nbuckets: Option<u32>,
    keep_nulls: bool,
    merge: bool,
    ctx: &ExecCtx,
) -> Result<super::pipe::Built> {
    let types = key_types(keys);
    let cat = ctx.catalog();
    let mut tuples = Vec::with_capacity(rows.len());
    let mut by_key: Vec<(Vec<Value>, usize)> = Vec::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        let k = eval_all(keys, r, ctx)?;
        let null = k.iter().any(Value::is_null);
        if null && !keep_nulls {
            continue;
        }
        let h = super::pghash::combine(&k, &types, Some(&cat));
        tuples.push((i, h, 0));
        if !null {
            by_key.push((normed(k, norms), i));
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
    Ok(super::pipe::Built::new(rows, by_key, table, merge))
}

/// Key values as the join compares them (`pipe::norm`).
fn normed(k: Vec<Value>, norms: &[super::pipe::Norm]) -> Vec<Value> {
    k.into_iter()
        .zip(norms.iter().copied().chain(std::iter::repeat(super::pipe::Norm::Int)))
        .map(|(v, n)| super::pipe::norm(v, n))
        .collect()
}

/// EXPLAIN ANALYZE's view of a hash join's inner rows: how they bunch on their keys (which
/// batching can't split) and each column's values as a tuple stores them (what the ones the
/// table keeps take decides a batch's size), for the model of a Hash it didn't run.
struct InnerStats {
    counts: HashMap<String, u64>,
    bytes: [u64; 32],
    rows: u64,
    types: Vec<crate::types::DataType>,
}

impl InnerStats {
    fn new(plan: &Plan) -> InnerStats {
        let types = crate::sql::paths::analyzejoins::col_types(plan);
        InnerStats { counts: HashMap::default(), bytes: [0; 32], rows: 0, types }
    }

    fn add(&mut self, r: &[Value], keys: &[Expr], ctx: &ExecCtx) -> Result<()> {
        let k = eval_all(keys, r, ctx)?;
        *self.counts.entry(format!("{k:?}")).or_default() += 1;
        let text = |v: &Value, ty: &crate::types::DataType| crate::types::text::format_value(v, ty, ctx.fmt());
        for ((v, ty), b) in r.iter().zip(&self.types).zip(self.bytes.iter_mut()) {
            if !v.is_null() {
                *b += crate::catalog::relsize::datum_bytes(0, v, ty, &text) as u64;
            }
        }
        self.rows += 1;
        Ok(())
    }

    fn record(self, plan: &Arc<Plan>, ctx: &ExecCtx) {
        let Some(instr) = ctx.instr.borrow().clone() else { return };
        let max = self.counts.values().copied().max().unwrap_or(0);
        // (The bunching and the values' sizes under the inner relation's own nodes too, below a merge
        // join's Sort and the projection over its scan: EXPLAIN's plan may hash that
        // relation where this one merges, its node keyed by the scan.)
        let mut keys = vec![Arc::as_ptr(plan) as usize];
        let mut p: &Plan = plan;
        while let Plan::Sort { input, .. } | Plan::Project { input, .. } = p {
            keys.push(Arc::as_ptr(input) as usize);
            p = input;
        }
        for k in keys {
            instr.update(k, |s| {
                s.hash_max_group = s.hash_max_group.max(max);
                s.hash_rows += self.rows;
                for (a, b) in s.hash_col_bytes.iter_mut().zip(self.bytes) {
                    *a += b;
                }
            });
        }
    }
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
    table: Option<Arc<super::pipe::Built>>,
    matched: Vec<bool>,
    /// The outer row being joined, its candidates, the next one, and whether one matched.
    cur: Option<(Row, (u32, u32), u32, bool)>,
    unmatched_right: Option<std::vec::IntoIter<usize>>,
    nbuckets: Option<u32>,
    merge: bool,
    /// A merge join's key directions (`JoinMeta::merge_dirs`).
    merge_dirs: Vec<(bool, bool)>,
    /// A merge right / full join's rows.
    merged: std::vec::IntoIter<Row>,
    /// A merge join's first outer row, read before the inner side runs.
    first: Option<Row>,
    /// The probe key's vector, reused.
    kbuf: Vec<Value>,
    /// How the outer and inner keys' values are compared (`pipe::norms`).
    lnorm: Vec<super::pipe::Norm>,
    rnorm: Vec<super::pipe::Norm>,
    /// The joined rows' columns anything reads, the condition's too (None: all).
    keep: Option<Box<[bool]>>,
    /// Its probes run in a parallel scan's workers below (`pipe`): the level of its rows
    /// (`pipe::ALL`: not).
    level: u8,
    /// Whether that was tried.
    tried: bool,
    /// A right or full join's inner rows the workers matched (`pipe::Probe::matched`).
    shared_matched: Option<Arc<[std::sync::atomic::AtomicBool]>>,
    /// Whether its outer side was asked to leave out rows of keys the inner side lacks
    /// (`Node::restrict_keys`).
    restricted: bool,
    /// ExecHashJoin reads an outer row before building (`JoinMeta::outer_first`).
    outer_first: bool,
    /// An inner or semi merge join's end (ExecMergeJoin's MJ_ENDINNER): the merge order's
    /// keys and the last inner row's keys that can match (None: not known yet, or not
    /// such a join); `merge_ended` once an outer row passes them.
    merge_end: Option<Option<(Vec<SortKey>, Vec<Value>)>>,
    merge_ended: bool,
    /// The planner's estimate of the inner side (rows, width), which sizes PostgreSQL's
    /// table: a planned hash join's table is modelled (`hashjoin`), and batched past
    /// hash_mem.
    inner_est: Option<(f64, u32)>,
    /// The inner columns read above the join (its output), apart from its keys and
    /// condition: they come first in the Hash's tuple (the relation's reltarget lists the
    /// target list's columns before the join clauses').
    above: Option<Box<[bool]>>,
    left_plan: Arc<Plan>,
    batching: Option<Box<Batching>>,
}

/// The column a hash join's one outer key is, if any (create_hashjoin_plan's skew column:
/// a Var, under a RelabelType or not).
fn skew_column(keys: &[Expr]) -> Option<usize> {
    match keys {
        [Expr::Col(i, _)] => Some(*i),
        [Expr::Relabel(x, _)] => match &**x {
            Expr::Col(i, _) => Some(*i),
            _ => None,
        },
        _ => None,
    }
}

/// The skew column's MCVs as ExecHashBuildSkewHash reads them from pg_statistic: (hash
/// value, frequency), most common first; none when it isn't a table's column.
fn skew_mcvs(outer: &Plan, keys: &[Expr], ctx: &ExecCtx) -> Vec<(u32, f32)> {
    let Some(col) = skew_column(keys) else { return Vec::new() };
    let Some((scan, c, _)) = crate::sql::explain::pwagg::origin(outer, col) else { return Vec::new() };
    let cat = ctx.catalog();
    let Some(st) = crate::sql::costs::col_stats(&cat, scan.table.oid, c) else { return Vec::new() };
    let Some(sl) = st.slots.iter().find(|s| s.kind == 1) else { return Vec::new() };
    let (Some(nums), Some((ty, vals))) = (&sl.numbers, &sl.values) else { return Vec::new() };
    let Some(values) = crate::sql::costs::slot_values(sl, ty, vals, ctx.fmt()) else { return Vec::new() };
    let kty = keys[0].ty();
    values.iter().zip(nums).map(|(v, &f)| (super::pghash::hash32(v, &kty, Some(&cat)), f)).collect()
}

/// A hash join's table as PostgreSQL's (`hashjoin::HashTable`) and what its batches need.
struct Batching {
    hj: super::hashjoin::HashTable,
    /// The inner rows' keys (over narrowed rows) and how a batch's rows are kept
    /// (`Built::narrowed` places, or the columns `Built::compact` keeps).
    rkeys: Vec<Expr>,
    places: Option<Box<[u32]>>,
    compact: Option<Vec<bool>>,
    /// The batch's outer file being probed.
    outer: Option<super::hashjoin::BatchReader>,
    /// The outer columns the join reads (the others go to batch files as NULL), the outer
    /// row's types, and the outer keys' types.
    ocols: Option<Vec<bool>>,
    otypes: Vec<crate::types::DataType>,
    oktypes: Vec<crate::types::DataType>,
    cat: Arc<crate::catalog::Catalog>,
}

/// What a join's output needs copied: `need` and the columns its condition reads.
fn join_keep(need: Option<&[bool]>, cond: Option<&Expr>, width: usize) -> Option<Box<[bool]>> {
    let mut k: Vec<bool> = need?.to_vec();
    if let Some(c) = cond
        && !super::need::mark(c, &mut k)
    {
        return None;
    }
    k.resize(width.max(k.len()), false);
    Some(k.into_boxed_slice())
}

impl HashJoinNode {
    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: JoinKind,
        left: BoxNode,
        left_plan: Arc<Plan>,
        right_plan: Arc<Plan>,
        lkeys: Vec<Expr>,
        rkeys: Vec<Expr>,
        cond: Option<Expr>,
        nleft: usize,
        nright: usize,
        meta: &crate::sql::plan::JoinMeta,
        need: Option<&[bool]>,
    ) -> Self {
        let (lnorm, rnorm) = super::pipe::norms(&lkeys, &rkeys);
        HashJoinNode {
            keep: join_keep(need, cond.as_ref(), nleft + nright),
            above: need.map(|n| (nleft..nleft + nright).map(|i| n.get(i).copied().unwrap_or(false)).collect()),
            lnorm,
            rnorm,
            level: super::pipe::ALL,
            tried: false,
            shared_matched: None,
            restricted: false,
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
            kbuf: Vec::new(),
            outer_first: meta.outer_first,
            merge_end: None,
            merge_ended: false,
            inner_est: meta.hash_inner.filter(|_| meta.method != Some(crate::sql::plan::JoinMethod::Merge)),
            left_plan,
            batching: None,
        }
    }

    /// Whether ExecHashJoin would read the outer side's first row before building the hash
    /// table: it fills the outer side, or the outer side starts sooner than the hash is
    /// built, and it doesn't fill the inner side.
    fn reads_outer_first(&self) -> bool {
        !self.merge
            && !matches!(self.kind, JoinKind::Right | JoinKind::Full)
            && (self.outer_first || matches!(self.kind, JoinKind::Left | JoinKind::Anti))
    }

    /// Whether the outer side's first row is read before the build: as ExecHashJoin does
    /// when that shows, under EXPLAIN ANALYZE (the Hash "never executed") or when the build
    /// can be seen by itself (a notice, a volatile call). Otherwise the build comes first,
    /// so the probes can run in a parallel scan's workers.
    fn build_waits(&self, ctx: &ExecCtx) -> bool {
        self.reads_outer_first() && (ctx.instrumented() || crate::sql::explain::plan_observable(&self.right_plan))
    }

    /// The build failed with `e`: as ExecHashJoin, which would have read the outer side
    /// first, nothing when that has no row (the inner side never ran), else the error (the
    /// outer side's own first, if it fails).
    fn outer_empty_or(&mut self, e: crate::error::Error, ctx: &ExecCtx) -> Result<Option<super::pipe::Item>> {
        match self.left.next(ctx)? {
            None => Ok(None),
            Some(_) => Err(e),
        }
    }

    fn build(&mut self, ctx: &ExecCtx) -> Result<()> {
        if self.table.is_some() {
            return Ok(());
        }
        if let Some(est) = self.inner_est {
            return self.build_hashed(est, ctx);
        }
        // (The inner columns the output and condition read, and the keys': the inner rows are
        // read with only those, the keys over them.)
        let narrow = self.inner_narrow(ctx);
        let rows = match &narrow {
            Some((n, need, _)) => super::collect_need(&self.right_plan, Some(need.clone()), ctx, |r| n.shrink(r))?,
            // (Still told the columns read: a Sort below counts its tuples by them.)
            None => super::collect_need(&self.right_plan, self.inner_cols(), ctx, |r| r)?,
        };
        // (EXPLAIN ANALYZE: how the rows bunch on their keys, which batching can't split.)
        if ctx.instr.borrow().is_some() {
            let mut st = InnerStats::new(&self.right_plan);
            for r in &rows {
                st.add(r, &self.rkeys, ctx)?;
            }
            st.record(&self.right_plan, ctx);
        }
        let keep = matches!(self.kind, JoinKind::Right | JoinKind::Full);
        // (A merge join meets equal inner rows in their (sorted) order.)
        let rkeys = narrow.as_ref().map_or(&self.rkeys, |(_, _, k)| k);
        let mut built = build_hash(rows, rkeys, &self.rnorm, self.nbuckets, keep, self.merge, ctx)?;
        // (The inner columns the output and condition read, and the keys' (the outer side's
        // restriction evaluates them again): the table keeps only those.)
        if let Some((n, ..)) = &narrow {
            built.narrowed(n.places());
        } else if let Some(k) = &self.keep {
            let mut inner = k.get(self.nleft..).map(<[bool]>::to_vec).unwrap_or_default();
            inner.resize(self.nright.max(inner.len()), false);
            if self.rkeys.iter().all(|e| super::need::mark(e, &mut inner)) {
                built.compact(&inner);
            }
        }
        self.matched = vec![false; built.rows.len()];
        self.table = Some(Arc::new(built));
        Ok(())
    }
}

impl HashJoinNode {
    /// The inner rows narrowed to the columns the join's output, its condition and its keys
    /// read (`Narrow`), the mask the inner side is opened with, and the keys over the
    /// narrowed rows. None: whole rows (under EXPLAIN ANALYZE, whose sizes read them).
    fn inner_narrow(&self, ctx: &ExecCtx) -> Option<(Narrow, Vec<bool>, Vec<Expr>)> {
        if ctx.instrumented() {
            return None;
        }
        let inner = self.inner_cols()?;
        let n = Narrow::of(&inner, self.nright)?;
        let keys = self.rkeys.iter().map(|e| n.expr(e)).collect::<Option<Vec<_>>>()?;
        Some((n, inner, keys))
    }

    /// The inner columns the join's output, its condition and its keys read (None: all). A
    /// semi or anti join returns its outer rows only: its condition and keys'.
    fn inner_cols(&self) -> Option<Vec<bool>> {
        let mut inner = match &self.keep {
            Some(k) => k.get(self.nleft..).map(<[bool]>::to_vec).unwrap_or_default(),
            None if matches!(self.kind, JoinKind::Semi | JoinKind::Anti) => {
                let mut m = Vec::new();
                if let Some(c) = &self.cond
                    && !super::need::mark(c, &mut m)
                {
                    return None;
                }
                m.get(self.nleft..).map(<[bool]>::to_vec).unwrap_or_default()
            }
            None => return None,
        };
        inner.resize(self.nright.max(inner.len()), false);
        if !self.rkeys.iter().all(|e| super::need::mark(e, &mut inner)) {
            return None;
        }
        Some(inner)
    }

    /// Whether an outer row of keys `k` ends an inner or semi merge join, as ExecMergeJoin
    /// ends it having read that row: a null first key sorting last (MJEVAL_ENDOFJOIN), or
    /// keys past every inner row that can match (the inner side runs out, MJ_ENDINNER).
    /// The outer rows after it are never read.
    fn merge_passed(&mut self, k: &[Value], ctx: &ExecCtx) -> Result<bool> {
        if self.merge_end.is_none() {
            let keys = self.merge_keys();
            let t = self.table.as_ref().unwrap();
            let mut last = None;
            for j in (0..t.rows.len()).rev() {
                let ik = normed(eval_all(&self.rkeys, &t.inner(j).to_row(), ctx)?, &self.rnorm);
                if !ik.iter().any(Value::is_null) {
                    last = Some(ik);
                    break;
                }
            }
            self.merge_end = Some(last.map(|l| (keys, l)));
        }
        let Some(Some((keys, last))) = &self.merge_end else {
            return Ok(false);
        };
        if k.first().is_some_and(Value::is_null) && !keys[0].nulls_first {
            return Ok(true);
        }
        if k.iter().any(Value::is_null) {
            return Ok(false);
        }
        for (i, sk) in keys.iter().enumerate() {
            match compare_values(&k[i], &last[i], sk, ctx)? {
                Ordering::Equal => {}
                o => return Ok(o == Ordering::Greater),
            }
        }
        Ok(false)
    }

    /// A merge join's keys in its inputs' order: the inner input's sort, when it is a Sort
    /// (else ascending).
    fn merge_keys(&self) -> Vec<SortKey> {
        self.rkeys
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
            .collect()
    }

    /// A Merge Right / Full Join (nodeMergejoin.c): both inputs sorted on the keys, walked
    /// together; an inner row nothing matched comes out when the walk passes it.
    fn merge_outer(&mut self, ctx: &ExecCtx) -> Result<Vec<Row>> {
        let mut outer = Vec::new();
        while let Some(r) = self.left.next(ctx)? {
            ctx.check()?;
            outer.push(r);
        }
        let inner = super::collect_need(&self.right_plan, self.inner_cols(), ctx, |r| r)?;
        let sort_keys = self.merge_keys();
        let okeys: Vec<Vec<Value>> = outer
            .iter()
            .map(|r| eval_all(&self.lkeys, r, ctx).map(|k| normed(k, &self.lnorm)))
            .collect::<Result<_>>()?;
        let ikeys: Vec<Vec<Value>> = inner
            .iter()
            .map(|r| eval_all(&self.rkeys, r, ctx).map(|k| normed(k, &self.rnorm)))
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

impl HashJoinNode {
    /// A planned hash join's build: the inner rows go into PostgreSQL's table as its Hash
    /// puts them there (`hashjoin::HashTable`), into batches past hash_mem.
    fn build_hashed(&mut self, est: (f64, u32), ctx: &ExecCtx) -> Result<()> {
        let narrow = self.inner_narrow(ctx);
        let cols = self.inner_cols();
        let types = crate::sql::paths::analyzejoins::col_types(&self.right_plan);
        // (The Hash's tuple has the columns of its target list: a narrowed row's all.)
        let (ttypes, tcols) = match &narrow {
            Some((_, need, _)) => (types.iter().zip(need).filter(|(_, k)| **k).map(|(t, _)| t.clone()).collect(), None),
            None => (types.clone(), cols.clone()),
        };
        // (The tuple's columns in the Hash's target list order, as places in the rows read.)
        let order: Vec<usize> = {
            let inner = |c: usize| cols.as_ref().is_none_or(|m| m.get(c).copied().unwrap_or(false));
            let up = |c: usize| self.above.as_ref().is_none_or(|m| m.get(c).copied().unwrap_or(false));
            let first = (0..types.len()).filter(|&c| inner(c) && up(c));
            let rest = (0..types.len()).filter(|&c| inner(c) && !up(c));
            first
                .chain(rest)
                .map(|c| narrow.as_ref().map_or(Some(c), |(n, ..)| n.place(c)))
                .collect::<Option<Vec<usize>>>()
                .unwrap_or_default()
        };
        let rkeys = narrow.as_ref().map_or_else(|| self.rkeys.clone(), |(_, _, k)| k.clone());
        let ktypes = key_types(&rkeys);
        let keep_nulls = matches!(self.kind, JoinKind::Right | JoinKind::Full);
        // (create_hashjoin_plan's skew column: one clause, over an outer column.)
        let useskew = skew_column(&self.lkeys).is_some_and(|c| crate::sql::explain::pwagg::origin(&self.left_plan, c).is_some());
        let mut hj = super::hashjoin::HashTable::new(est.0, est.1, useskew, ctx);
        if useskew && hj.wants_skew() {
            hj.build_skew(&skew_mcvs(&self.left_plan, &self.lkeys, ctx));
        }
        let mut st = ctx.instr.borrow().is_some().then(|| InnerStats::new(&self.right_plan));
        let cat = ctx.catalog();
        let need = narrow.as_ref().map_or_else(|| tcols.clone(), |(_, n, _)| Some(n.clone()));
        ctx.statement(|| {
            let mut node = open_need(&self.right_plan, need, ctx)?;
            while let Some(r) = node.next(ctx)? {
                let r = match &narrow {
                    Some((n, ..)) => n.shrink(r),
                    None => r,
                };
                if let Some(st) = &mut st {
                    st.add(&r, &self.rkeys, ctx)?;
                }
                let k = eval_all(&rkeys, &r, ctx)?;
                if !keep_nulls && k.iter().any(Value::is_null) {
                    continue;
                }
                let h = super::pghash::combine(&k, &ktypes, Some(&cat));
                let tlen = super::workmem::minimal_tuple_len_order(&r, &ttypes, &order, ctx);
                hj.build_insert(r, h, tlen, ctx)?;
            }
            Ok(())
        })?;
        if let Some(st) = st {
            st.record(&self.right_plan, ctx);
        }
        hj.finish_build();
        let (places, compact) = match &narrow {
            Some((n, ..)) => (Some(n.places()), None),
            None => (None, cols),
        };
        let ocols = self.keep.as_ref().and_then(|k| {
            let mut o = k.get(..self.nleft)?.to_vec();
            self.lkeys.iter().all(|e| super::need::mark(e, &mut o)).then_some(o)
        });
        self.batching = Some(Box::new(Batching {
            hj,
            rkeys,
            places,
            compact,
            outer: None,
            ocols,
            otypes: Vec::new(),
            oktypes: key_types(&self.lkeys),
            cat,
        }));
        self.load_batch(ctx)
    }

    /// The batch's table, of the rows PostgreSQL's table holds now.
    fn load_batch(&mut self, ctx: &ExecCtx) -> Result<()> {
        let b = self.batching.as_mut().unwrap();
        let (rows, table) = b.hj.take_batch();
        let mut by_key = Vec::with_capacity(rows.len());
        for (i, r) in rows.iter().enumerate() {
            let k = eval_all(&b.rkeys, r, ctx)?;
            if !k.iter().any(Value::is_null) {
                by_key.push((normed(k, &self.rnorm), i));
            }
        }
        let mut built = super::pipe::Built::new(rows, by_key, table, false);
        if let Some(p) = &b.places {
            built.narrowed(p.clone());
        } else if let Some(c) = &b.compact {
            built.compact(c);
        }
        self.matched = vec![false; built.rows.len()];
        self.table = Some(Arc::new(built));
        if let Some(instr) = ctx.instr.borrow().clone() {
            let s = b.hj.stats();
            instr.update(Arc::as_ptr(&self.right_plan) as usize, |x| x.hashjoin = Some(super::HashStats::accum(x.hashjoin, s)));
        }
        Ok(())
    }

    /// The next outer row of the batch running (ExecHashJoinOuterGetTuple), those of later
    /// batches put in their outer files on the way.
    fn next_outer(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let Some(b) = self.batching.as_mut().filter(|b| b.hj.batched()) else {
            return match self.first.take() {
                Some(r) => Ok(Some(r)),
                None => self.left.next(ctx),
            };
        };
        let fill_outer = matches!(self.kind, JoinKind::Left | JoinKind::Full | JoinKind::Anti);
        loop {
            let (row, hash, tlen) = if b.hj.curbatch == 0 {
                let r = match self.first.take() {
                    Some(r) => r,
                    None => match self.left.next(ctx)? {
                        Some(r) => r,
                        None => return Ok(None),
                    },
                };
                let k = eval_all(&self.lkeys, &r, ctx)?;
                if !fill_outer && k.iter().any(Value::is_null) {
                    continue;
                }
                (r, super::pghash::combine(&k, &b.oktypes, Some(&b.cat)), None)
            } else {
                let Some(rd) = b.outer.as_mut() else { return Ok(None) };
                match b.hj.read(rd)? {
                    Some((h, len, r)) => (r, h, Some(len as i64)),
                    None => return Ok(None),
                }
            };
            let batch = b.hj.batch_of(hash);
            // (A skew bucket's outer rows are batch 0's, whatever their batch.)
            if batch == b.hj.curbatch || b.hj.skew_bucket(hash).is_some() {
                return Ok(Some(row));
            }
            ctx.check()?;
            let row: Row = match &b.ocols {
                Some(c) => row
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| if c.get(i).copied().unwrap_or(true) { v } else { Value::Null })
                    .collect(),
                None => row,
            };
            let tlen = match tlen {
                Some(l) => l,
                None => {
                    if b.otypes.is_empty() {
                        b.otypes = crate::sql::paths::analyzejoins::col_types(&self.left_plan);
                    }
                    super::workmem::minimal_tuple_len_cols(&row, &b.otypes, b.ocols.as_deref(), ctx)
                }
            };
            b.hj.save_outer(batch, hash, &row, tlen, ctx)?;
        }
    }

    /// The batch's outer rows are done: the next batch with work loaded (ExecHashJoinNewBatch),
    /// false when none is left.
    fn next_batch(&mut self, ctx: &ExecCtx) -> Result<bool> {
        let Some(b) = self.batching.as_mut().filter(|b| b.hj.batched()) else { return Ok(false) };
        let fill_outer = matches!(self.kind, JoinKind::Left | JoinKind::Full | JoinKind::Anti);
        let fill_inner = matches!(self.kind, JoinKind::Right | JoinKind::Full);
        b.outer = None;
        // (The batch's rows go before the next one's come.)
        self.table = Some(Arc::new(super::pipe::Built::new(Vec::new(), Vec::new(), super::pghash::JoinTable::flat(1, std::iter::empty()), false)));
        let Some(outer) = b.hj.new_batch(fill_outer, fill_inner, ctx)? else { return Ok(false) };
        b.outer = outer;
        self.cur = None;
        self.unmatched_right = None;
        self.load_batch(ctx)?;
        Ok(true)
    }

    /// No inner row at all (ExecHashJoin's totalTuples == 0).
    fn inner_empty(&self) -> bool {
        match &self.batching {
            Some(b) => b.hj.total_tuples == 0,
            None => self.table.as_ref().is_some_and(|t| t.rows.is_empty()),
        }
    }
}

impl HashJoinNode {
    /// This join's probe as a stage a parallel scan's workers run (`pipe`), when it can
    /// be one: a hash join (not a merge join's, not one returning unmatched inner rows)
    /// whose outer keys and condition compile to kernels. Builds the table.
    fn probe_stage(&mut self, ctx: &ExecCtx) -> Result<Option<Arc<super::pipe::Stage>>> {
        use super::kernel::compile;
        if self.merge
            || !matches!(
                self.kind,
                JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti | JoinKind::Right | JoinKind::Full
            )
            || ctx.instrumented()
        {
            return Ok(None);
        }
        let Some(keys) = self
            .lkeys
            .iter()
            .zip(&self.lnorm)
            .map(|(e, n)| compile(e).map(|k| (k, *n)))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        // (A condition without a kernel: the rows with candidates are checked here.)
        let (cond, defer) = match &self.cond {
            Some(c) => match compile(c) {
                Some(k) => (Some(k), false),
                None => (None, true),
            },
            None => (None, false),
        };
        // (Unmatched inner rows need every outer row's matches seen.)
        if defer && matches!(self.kind, JoinKind::Right | JoinKind::Full) {
            return Ok(None);
        }
        self.build(ctx)?;
        // (A table in batches is probed here, a batch at a time.)
        if self.batching.as_ref().is_some_and(|b| b.hj.batched()) {
            return Ok(None);
        }
        let built = self.table.clone().unwrap();
        // (An empty table that leaves the outer side unread: `step` returns at once.)
        if built.rows.is_empty() && matches!(self.kind, JoinKind::Inner | JoinKind::Semi | JoinKind::Right) {
            return Ok(None);
        }
        let matched = matches!(self.kind, JoinKind::Right | JoinKind::Full).then(|| {
            (0..built.rows.len()).map(|_| std::sync::atomic::AtomicBool::new(false)).collect::<Arc<[_]>>()
        });
        self.shared_matched = matched.clone();
        Ok(Some(Arc::new(super::pipe::Stage::Hash(super::pipe::Probe {
            built,
            keys,
            cond,
            defer,
            kind: self.kind,
            nright: self.nright,
            keep: self.keep.clone(),
            matched,
        }))))
    }

    /// The next row (with its level) or part.
    fn step(&mut self, ctx: &ExecCtx) -> Result<Option<super::pipe::Item>> {
        use super::pipe::Item;
        if self.merge && matches!(self.kind, JoinKind::Right | JoinKind::Full) {
            if self.unmatched_right.is_none() {
                let rows = self.merge_outer(ctx)?;
                self.merged = rows.into_iter();
                self.unmatched_right = Some(Vec::new().into_iter());
            }
            return Ok(self.merged.next().map(|r| Item::Row(r, self.level)));
        }
        // ExecMergeJoin reads the outer side first: without rows, the inner side never
        // runs.
        if self.merge && self.table.is_none() {
            let Some(l) = self.left.next(ctx)? else {
                return Ok(None);
            };
            self.first = Some(l);
        }
        // ExecHashJoin's first outer row before the build (`build_waits`); otherwise only
        // the build's failure asks whether the outer side has a row.
        let outer_first = self.table.is_none() && self.reads_outer_first();
        if outer_first && self.first.is_none() && self.build_waits(ctx) {
            match self.left.next(ctx)? {
                None => return Ok(None),
                Some(l) => self.first = Some(l),
            }
        }
        // (Its probes in the workers of a parallel scan below, when it has one.)
        if !self.tried && self.table.is_none() {
            self.tried = true;
            let p = match self.probe_stage(ctx) {
                Err(e) if outer_first && self.first.is_none() => return self.outer_empty_or(e, ctx),
                p => p?,
            };
            if let Some(p) = p
                && let Some(l) = self.left.pipeline(vec![p], None, ctx)?
            {
                self.level = l + 1;
            }
        }
        match self.build(ctx) {
            Err(e) if outer_first && self.first.is_none() => return self.outer_empty_or(e, ctx),
            r => r?,
        }
        // An empty hash table: no outer row can match, and one that needs no NULL-extended
        // output isn't read (ExecHashJoin's totalTuples == 0).
        if !self.merge
            && self.cur.is_none()
            && self.unmatched_right.is_none()
            && matches!(self.kind, JoinKind::Inner | JoinKind::Semi | JoinKind::Right)
            && self.inner_empty()
        {
            return Ok(None);
        }
        // An inner or semi join on one oid column: the outer side may leave out the rows of
        // keys the inner side lacks (a catalog scan reads the others through its hash).
        if !self.restricted {
            self.restricted = true;
            if !self.merge
                && matches!(self.kind, JoinKind::Inner | JoinKind::Semi)
                && self.level == super::pipe::ALL
                && !self.batching.as_ref().is_some_and(|b| b.hj.batched())
                && let [Expr::Col(k, _)] = &self.lkeys[..]
                && let [rkey] = &self.rkeys[..]
                && let Some(t) = &self.table
                && t.rows.len() <= RESTRICT_KEYS
            {
                let mut keys = Vec::with_capacity(t.rows.len());
                let mut oids = true;
                for j in 0..t.rows.len() {
                    match eval(rkey, &t.inner(j).to_row(), ctx)? {
                        Value::Oid(o) => keys.push(o),
                        Value::Null => {}
                        _ => oids = false,
                    }
                }
                if oids {
                    let k = *k;
                    self.left.restrict_keys(k, &keys, ctx)?;
                }
            }
        }
        let level = self.level;
        loop {
            if let Some(it) = self.unmatched_right.as_mut() {
                let t = self.table.as_ref().unwrap();
                for j in it.by_ref() {
                    if !self.matched[j] {
                        let mut out = vec![Value::Null; self.nleft];
                        out.extend(t.inner(j).to_row());
                        return Ok(Some(Item::Row(out, level)));
                    }
                }
                if self.next_batch(ctx)? {
                    continue;
                }
                return Ok(None);
            }
            if self.cur.is_none() {
                if self.merge_ended {
                    return Ok(None);
                }
                let batched = self.batching.as_ref().is_some_and(|b| b.hj.batched());
                let next = match self.first.take() {
                    first if batched => {
                        self.first = first;
                        self.next_outer(ctx)?
                    }
                    Some(r) => Some(r),
                    None if level != super::pipe::ALL => match self.left.next_item(ctx)? {
                        None => None,
                        Some(Item::Part(p)) => return Ok(Some(Item::Part(p))),
                        // (Joined in the workers already.)
                        Some(Item::Row(r, l)) if l >= level => return Ok(Some(Item::Row(r, l))),
                        Some(Item::Row(r, _)) => Some(r),
                    },
                    None => self.left.next(ctx)?,
                };
                match next {
                    Some(l) => {
                        ctx.check()?;
                        // (The probe's key in one vector for all rows.)
                        let mut k = std::mem::take(&mut self.kbuf);
                        k.clear();
                        for (e, n) in self.lkeys.iter().zip(&self.lnorm) {
                            k.push(super::pipe::norm(eval(e, &l, ctx)?, *n));
                        }
                        if self.merge
                            && matches!(self.kind, JoinKind::Inner | JoinKind::Semi)
                            && self.merge_passed(&k, ctx)?
                        {
                            self.kbuf = k;
                            self.merge_ended = true;
                            return Ok(None);
                        }
                        let span = self.table.as_ref().unwrap().lookup(&k);
                        self.kbuf = k;
                        self.cur = Some((l, span, 0, false));
                    }
                    None => {
                        if matches!(self.kind, JoinKind::Right | JoinKind::Full) {
                            // (The rows the workers matched, with the ones matched here.)
                            if let Some(m) = self.shared_matched.take() {
                                for (a, b) in self.matched.iter_mut().zip(m.iter()) {
                                    *a |= b.load(std::sync::atomic::Ordering::Relaxed);
                                }
                            }
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
                        if self.next_batch(ctx)? {
                            continue;
                        }
                        return Ok(None);
                    }
                }
            }
            let (l, span, i, matched) = self.cur.as_mut().unwrap();
            let t = self.table.as_ref().unwrap();
            while *i < span.1 {
                let j = t.cand(*span, *i);
                *i += 1;
                let combined = super::pipe::joined(l, t.inner(j), self.keep.as_deref());
                if let Some(c) = &self.cond
                    && !eval_bool(c, &combined, ctx)?
                {
                    continue;
                }
                *matched = true;
                self.matched[j] = true;
                match self.kind {
                    JoinKind::Semi => {
                        let Some((l, ..)) = self.cur.take() else { unreachable!() };
                        return Ok(Some(Item::Row(l, level)));
                    }
                    JoinKind::Anti => {
                        self.cur = None;
                        break;
                    }
                    _ => return Ok(Some(Item::Row(combined, level))),
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
                        return Ok(Some(Item::Row(out, level)));
                    }
                    JoinKind::Anti => return Ok(Some(Item::Row(l, level))),
                    _ => {}
                }
            }
        }
    }
}

impl Node for HashJoinNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        match self.step(ctx)? {
            None => Ok(None),
            Some(super::pipe::Item::Row(r, _)) => Ok(Some(r)),
            Some(super::pipe::Item::Part(_)) => Err(crate::error::internal("parts from a join")),
        }
    }

    fn next_item(&mut self, ctx: &ExecCtx) -> Result<Option<super::pipe::Item>> {
        self.step(ctx)
    }

    fn pipeline(
        &mut self,
        above: Vec<Arc<super::pipe::Stage>>,
        spec: Option<Arc<super::partial::Spec>>,
        ctx: &ExecCtx,
    ) -> Result<Option<u8>> {
        // (A build that waits for the outer side's first row runs in `step`.)
        if self.tried || self.table.is_some() || self.build_waits(ctx) {
            return Ok(None);
        }
        let with_spec = spec.is_some();
        let Some(p) = self.probe_stage(ctx)? else {
            self.tried = !with_spec;
            return Ok(None);
        };
        let mut stages = vec![p];
        stages.extend(above);
        match self.left.pipeline(stages, spec, ctx)? {
            Some(l) => {
                self.tried = true;
                self.level = l + 1;
                Ok(Some(l + 1))
            }
            None => {
                // (Without the aggregation it may still start on its own.)
                self.tried = !with_spec;
                Ok(None)
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
    /// The planner's estimate of the hashed (left) side: PostgreSQL's table, in batches
    /// past hash_mem (`hashjoin`).
    est: Option<(f64, u32)>,
}

impl RightSemiNode {
    /// The join as ExecHashJoin runs it with PostgreSQL's table: the left rows hashed into
    /// batches, the right rows probing batch 0 as they come and the later batches from
    /// their files; a semi join's left row out at its first match, an anti join's
    /// unmatched ones after each batch, bucket by bucket.
    fn run_batched(&self, est: (f64, u32), ctx: &ExecCtx) -> Result<Vec<Row>> {
        let (lnorm, rnorm) = super::pipe::norms(&self.lkeys, &self.rkeys);
        let ltypes = crate::sql::paths::analyzejoins::col_types(&self.left);
        let (lk, rk) = (key_types(&self.lkeys), key_types(&self.rkeys));
        let cat = ctx.catalog();
        let useskew = skew_column(&self.rkeys).is_some_and(|c| crate::sql::explain::pwagg::origin(&self.right, c).is_some());
        let mut hj = super::hashjoin::HashTable::new(est.0, est.1, useskew, ctx);
        if useskew && hj.wants_skew() {
            hj.build_skew(&skew_mcvs(&self.right, &self.rkeys, ctx));
        }
        ctx.statement(|| {
            let mut node = open(&self.left, ctx)?;
            while let Some(r) = node.next(ctx)? {
                let k = eval_all(&self.lkeys, &r, ctx)?;
                if !self.anti && k.iter().any(Value::is_null) {
                    continue;
                }
                let h = super::pghash::combine(&k, &lk, Some(&cat));
                let tlen = super::workmem::minimal_tuple_len_cols(&r, &ltypes, None, ctx);
                hj.build_insert(r, h, tlen, ctx)?;
            }
            Ok(())
        })?;
        hj.finish_build();
        let mut out = Vec::new();
        if hj.total_tuples == 0 {
            return Ok(out);
        }
        let mut right = open(&self.right, ctx)?;
        let mut rtypes = Vec::new();
        let mut probe: Option<super::hashjoin::BatchReader> = None;
        loop {
            let (rows, table) = hj.take_batch();
            let mut by_key = Vec::with_capacity(rows.len());
            for (i, r) in rows.iter().enumerate() {
                let k = eval_all(&self.lkeys, r, ctx)?;
                if !k.iter().any(Value::is_null) {
                    by_key.push((normed(k, &lnorm), i));
                }
            }
            let built = super::pipe::Built::new(rows, by_key, table, false);
            if let Some(instr) = ctx.instr.borrow().clone() {
                let s = hj.stats();
                instr.update(Arc::as_ptr(&self.left) as usize, |x| x.hashjoin = Some(super::HashStats::accum(x.hashjoin, s)));
            }
            let mut emitted = vec![false; built.rows.len()];
            loop {
                let (r, h) = if hj.curbatch == 0 {
                    let Some(r) = right.next(ctx)? else { break };
                    let k = eval_all(&self.rkeys, &r, ctx)?;
                    if k.iter().any(Value::is_null) {
                        continue;
                    }
                    (r, super::pghash::combine(&k, &rk, Some(&cat)))
                } else {
                    let Some(rd) = probe.as_mut() else { break };
                    match hj.read(rd)? {
                        Some((h, _, r)) => (r, h),
                        None => break,
                    }
                };
                ctx.check()?;
                let b = hj.batch_of(h);
                if b != hj.curbatch && hj.skew_bucket(h).is_none() {
                    if rtypes.is_empty() {
                        rtypes = crate::sql::paths::analyzejoins::col_types(&self.right);
                    }
                    let tlen = super::workmem::minimal_tuple_len_cols(&r, &rtypes, None, ctx);
                    hj.save_outer(b, h, &r, tlen, ctx)?;
                    continue;
                }
                self.probe(&built, &mut emitted, &r, &rnorm, &mut out, ctx)?;
            }
            if self.anti {
                for j in built.table.scan() {
                    if !emitted[j] {
                        out.push(built.rows[j].clone());
                    }
                }
            }
            drop(built);
            // ExecHashJoinNewBatch closes the previous outer file before loading another
            // inner batch, so its bytes no longer count against temp_file_limit.
            drop(probe.take());
            match hj.new_batch(false, self.anti, ctx)? {
                Some(p) => probe = p,
                None => return Ok(out),
            }
        }
    }

    /// A right row's matches among the hashed rows (`emitted`: matched already).
    fn probe(
        &self,
        built: &super::pipe::Built,
        emitted: &mut [bool],
        r: &[Value],
        rnorm: &[super::pipe::Norm],
        out: &mut Vec<Row>,
        ctx: &ExecCtx,
    ) -> Result<()> {
        let k = normed(eval_all(&self.rkeys, r, ctx)?, rnorm);
        let span = built.lookup(&k);
        for c in 0..span.1 {
            let j = built.cand(span, c);
            if emitted[j] {
                continue;
            }
            if let Some(c) = &self.cond
                && !eval_bool(c, &concat_rows(&built.rows[j], r), ctx)?
            {
                continue;
            }
            emitted[j] = true;
            if !self.anti {
                out.push(built.rows[j].clone());
            }
        }
        Ok(())
    }
    fn run(&self, ctx: &ExecCtx) -> Result<Vec<Row>> {
        if let Some(est) = self.est {
            return self.run_batched(est, ctx);
        }
        let rows = collect(&self.left, ctx, None)?;
        let (lnorm, rnorm) = super::pipe::norms(&self.lkeys, &self.rkeys);
        let built = build_hash(rows, &self.lkeys, &lnorm, self.nbuckets, self.anti, false, ctx)?;
        let mut emitted = vec![false; built.rows.len()];
        let mut out = Vec::new();
        let mut right = open(&self.right, ctx)?;
        while let Some(r) = right.next(ctx)? {
            ctx.check()?;
            self.probe(&built, &mut emitted, &r, &rnorm, &mut out, ctx)?;
        }
        if self.anti {
            for j in built.table.scan() {
                if !emitted[j] {
                    out.push(built.rows[j].clone());
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
    /// The rows produced so far (the CTE's tuplestore: on disk past work_mem).
    store: super::tuplestore::Tuplestore,
    /// A data-modifying CTE not started yet.
    dml: Option<Arc<Plan>>,
}

impl CteState {
    /// The tuplestore of CTE `p`'s rows (ExecInitCteScan's leader makes it randomAccess,
    /// then sets its scan's flags, without EXEC_FLAG_BACKWARD, before a row is stored).
    fn store(p: &Plan, ctx: &ExecCtx) -> super::tuplestore::Tuplestore {
        super::tuplestore::Tuplestore::new(crate::sql::paths::analyzejoins::col_types(p), false, ctx)
    }

    pub(super) fn release(&mut self) {
        self.node.take();
    }

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
    /// Its read pointer in the CTE's tuplestore.
    slot: usize,
    instr: Option<Rc<super::Instr>>,
    /// The CTE's id: its scans' Storage line is counted under `super::cte_key(id)`.
    id: usize,
}

impl Node for CteScanNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let mut st = self.state.borrow_mut();
        if let Some(r) = st.store.read(self.slot, ctx)? {
            return Ok(Some(r));
        }
        st.start(ctx)?;
        let next = match st.node.as_mut() {
            Some(node) => node.next(ctx)?,
            None => None,
        };
        match next {
            Some(r) => {
                st.store.put(r.clone(), Some(self.slot), ctx)?;
                Ok(Some(r))
            }
            None => {
                st.node = None;
                Ok(None)
            }
        }
    }
}

impl Drop for CteScanNode {
    fn drop(&mut self) {
        let Ok(mut st) = self.state.try_borrow_mut() else { return };
        st.store.close_reader(self.slot);
        // (Every CTE Scan of it shows the store's figures as they are by now.)
        if let Some(instr) = &self.instr {
            let (disk, bytes) = st.store.stats();
            instr.update(super::cte_key(self.id), |s| {
                let (d0, b0) = s.material.unwrap_or_default();
                s.material = Some((d0 || disk, b0.max(bytes)));
            });
        }
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
    /// Under EXPLAIN ANALYZE: its two tuplestores as PostgreSQL's would hold them.
    acct: Option<Box<RecursiveAcct>>,
}

/// A Recursive Union's working and intermediate tables (nodeRecursiveunion.c), counted as
/// tuplestores (`workmem::StoreMem`): what EXPLAIN ANALYZE's "Storage" line shows.
struct RecursiveAcct {
    key: usize,
    instr: Rc<super::Instr>,
    types: Vec<crate::types::DataType>,
    /// The working table, then the intermediate one, and the t_lens of the rows each holds
    /// in memory.
    tables: [super::workmem::StoreMem; 2],
    held: [Vec<i64>; 2],
    /// The recursive term has started (the base rows are the first working table).
    started: bool,
}

impl RecursiveAcct {
    /// show_recursive_union_info: the bigger table's storage type, the two tables' bytes.
    fn record(&self) {
        let (wd, wb) = self.tables[0].stats();
        let (id, ib) = self.tables[1].stats();
        let disk = if wb > ib { wd } else { id };
        let bytes = (wb + ib) as u64;
        self.instr.update(self.key, |s| {
            let (d0, b0) = s.material.unwrap_or_default();
            s.material = Some((d0 || disk, b0.max(bytes)));
        });
    }
}

impl Drop for RecursiveNode {
    fn drop(&mut self) {
        if let Some(a) = &self.acct {
            a.record();
        }
    }
}

impl RecursiveNode {
    /// Keep a new row (UNION drops duplicates); it joins the next working table.
    fn keep(&mut self, r: Row, ctx: &ExecCtx) -> Option<Row> {
        if !self.all {
            if self.seen.contains_key(&r) {
                return None;
            }
            self.seen.insert(r.clone(), ());
        }
        if let Some(a) = self.acct.as_deref_mut() {
            let t = usize::from(a.started);
            let tlen = super::workmem::minimal_tuple_len(&r, &a.types, ctx);
            if a.tables[t].disk.is_none() {
                a.held[t].push(tlen);
            }
            if a.tables[t].put(tlen) {
                for l in a.held[t].drain(..) {
                    a.tables[t].dumped(l);
                }
            }
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
                    Some(r) => match self.keep(r, ctx) {
                        Some(r) => return Ok(Some(r)),
                        None => continue,
                    },
                    None => self.base = None,
                }
                continue;
            }
            if let Some(p) = self.produced.as_mut() {
                match p.next(ctx)? {
                    Some(r) => match self.keep(r, ctx) {
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
            // (The intermediate table becomes the working one; the old working table,
            // cleared, the next intermediate.)
            if let Some(a) = self.acct.as_deref_mut() {
                if a.started {
                    a.tables[0].clear();
                    a.held[0].clear();
                    a.tables.swap(0, 1);
                    a.held.swap(0, 1);
                }
                a.started = true;
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
    /// Postgres's stack its call holds while it runs (`sql_fn_frame`).
    pg_frame: usize,
    /// auto_explain's nested statement, logged when its rows are done.
    explain: Option<Box<NestedExplain>>,
}

struct NestedExplain {
    run: crate::ext::auto_explain::Run,
    instr: Option<Rc<super::Instr>>,
    plan: Arc<Plan>,
    params: Vec<Option<String>>,
    query: Arc<str>,
    /// A scalar call's (its plan's LIMIT 1 is the call's, not the statement's).
    scalar: bool,
    started: std::time::Instant,
}

impl NestedExplain {
    fn log(self, ctx: &ExecCtx) {
        let secs = self.started.elapsed().as_secs_f64();
        if self.run.message(secs * 1000.0, "", "", &[]).is_none() {
            return;
        }
        let plan = crate::plpgsql::statement_plan(&self.plan, self.scalar);
        let text = crate::sql::explain::explain(plan, 0, &self.run.opts, ctx, &[], self.instr.as_deref(), 0.0, secs);
        if let Some(m) = self.run.message(secs * 1000.0, &text, &self.query, &self.params) {
            crate::ext::auto_explain::emit_nested(ctx, self.run.level, &m);
        }
    }
}

/// Postgres's stack a SQL function's call holds while its statement runs: fmgr_sql's, and
/// in FROM also the function scan's above it.
fn sql_fn_frame(scan: bool) -> usize {
    crate::stack::PG_SQL_FN_FRAME + if scan { crate::stack::PG_FUNCSCAN_FRAME } else { 0 }
}

impl Node for ContextNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        let input = &mut self.input;
        let frame = &self.frame;
        let pg_frame = self.pg_frame;
        let mut run = || {
            ctx.sess.framed(frame.clone(), |_| {
                let _frame = crate::stack::FnFrame::enter(pg_frame)?;
                input.next(ctx)
            })
        };
        // The rows a SQL function's statement writes aren't the calling statement's.
        let processed = ctx.processed.get();
        let r = match &self.snap {
            Some(s) => ctx.with_snapshot(s.clone(), run),
            None => run(),
        };
        ctx.processed.set(processed);
        if matches!(r, Ok(None))
            && let Some(e) = self.explain.take()
        {
            e.log(ctx);
        }
        r
    }
}
