use super::*;
use crate::catalog::IndexKey;
use crate::sql::plan::{Access, IndexBounds};

fn integral(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Bool | DataType::Int2 | DataType::Int4 | DataType::Int8
    )
}

fn scalar(e: &Expr, columns: usize) -> bool {
    match e {
        Expr::Col(i, ty) => *i < columns && integral(ty),
        Expr::Const(_, ty) | Expr::Param(_, ty) => integral(ty),
        Expr::And(args) => args.iter().all(|a| scalar(a, columns)),
        Expr::Call(def, args, ty)
            if integral(ty)
                && def.oid != 0
                && def.srf.is_none()
                && matches!(def.name, "=" | "<" | "<=" | ">" | ">=" | "+" | "-" | "*") =>
        {
            let registry = crate::sql::func::registry();
            [&registry.binops, &registry.prefix]
                .into_iter()
                .filter_map(|ops| ops.get(def.name))
                .flatten()
                .any(|builtin| Arc::ptr_eq(builtin, def))
                && args.iter().all(|a| scalar(a, columns))
        }
        _ => false,
    }
}

fn restrictions(e: &Expr, columns: usize) -> bool {
    match e {
        Expr::And(args) => args.iter().all(|a| restrictions(a, columns)),
        _ => !e.columns().is_empty() && scalar(e, columns),
    }
}

/// Evidence from this discovery operation, never retained across statements.
pub(super) struct NoGather {
    #[cfg(debug_assertions)]
    reads: Vec<crate::sql::literals::SizeRead>,
}

#[cfg(debug_assertions)]
impl NoGather {
    pub(super) fn sizes_unchanged(&self, ctx: &ExecCtx) -> bool {
        self.reads.iter().all(|read| read.holds(ctx))
    }
}

/// A current, enabled serial scan costs less than every possible Gather over it.
/// The closed grammar has no other place to launch workers. Unknown input falls back.
pub(super) fn prove_no_gather(plan: &Arc<Plan>, ctx: &ExecCtx) -> Option<NoGather> {
    if crate::sql::literals::is_recording()
        || !WINDOW_TLIST.with_borrow(|w| w.is_empty())
        || FILTER_OVER_WINDOW.get()
        || FOREIGN_TOP.get()
    {
        return None;
    }
    let mut p = plan;
    while let Plan::Project { input, exprs } = &**p {
        if !exprs
            .iter()
            .all(|e| matches!(e, Expr::Col(_, ty) if integral(ty)))
        {
            return None;
        }
        p = input;
    }
    let Plan::Scan(s) = &**p else { return None };
    let ScanPlan {
        table,
        index,
        filter,
        with_key,
        reverse: _,
        mark,
        lock_order: _,
        limit,
        sys,
        alias: _,
        current_of,
        sample,
        excl: _, // Consumed only during binding, never by scan costing or discovery.
        part,
        part_excl,
        access,
        col_aliases: _,
        written,
        phv_quals,
        phv_proj,
        check_as: _,
        rti: _,
        useful,
        params,
        reltarget: _,
        fdw,
        rtindex: _,
    } = &**s;
    if table.oid < crate::catalog::FIRST_USER_OID
        || table.temporary
        || table.is_foreign()
        || table.partkey.is_some()
        || !table.inherits.is_empty()
        || *with_key
        || *sys
        || mark.is_some()
        || limit.is_some()
        || current_of.is_some()
        || sample.is_some()
        || part.is_some()
        || part_excl.is_some()
        || !params.0.is_empty()
        || !phv_quals.is_empty()
        || *phv_proj
        || fdw.is_some()
    {
        return None;
    }
    let columns = table.columns.len();
    if filter.as_ref().is_some_and(|f| !restrictions(f, columns))
        || !written.iter().all(|e| scalar(e, columns))
        || useful.0.as_ref().is_some_and(|u| {
            !u.0.iter()
                .all(|k| k.cmp == 0 && k.coll == 0 && scalar(&k.expr, columns))
                || !u.1.iter().all(|e| scalar(e, columns))
        })
    {
        return None;
    }
    match access {
        Access::Seq => {}
        Access::Index {
            only: _,
            quals,
            ordered: _,
            loops,
        } if *loops == 1.0 => {
            if !quals
                .iter()
                .all(|q| scalar(&q.expr, columns) && scalar(&q.orig, columns))
            {
                return None;
            }
        }
        _ => return None,
    }
    if let Some((_, b)) = index {
        let IndexBounds {
            eq,
            any,
            lower,
            upper,
            order,
            colls,
            pads,
            skip,
        } = b;
        if skip.is_some()
            || any.is_some()
            || colls.iter().any(Option::is_some)
            || pads.iter().any(|p| *p != 0)
            || !eq
                .iter()
                .chain(lower.iter().chain(upper).map(|x| &x.0))
                .all(|e| scalar(e, columns))
            || !order.is_empty()
        {
            return None;
        }
    }
    let cat = ctx.catalog();
    if cat.class_has_subclass.contains(&table.oid)
        || cat.statistics.values().any(|stat| stat.table == table.oid)
    {
        return None;
    }
    for ix in cat
        .table_indexes(table)
        .into_iter()
        .chain(table.pkey_index.and_then(|oid| cat.index(oid).cloned()))
    {
        if ix.method != "btree"
            || ix.predicate.is_some()
            || !ix.columns.iter().all(|key| {
                key.opclass == 0
                    && key.collation.is_none()
                    && key.opclass_options.is_empty()
                    && matches!(key.key, IndexKey::Column(i)
                        if table.columns.get(i).is_some_and(|c| integral(&c.ty)))
            })
        {
            return None;
        }
        if let Some(IndexKey::Column(i)) = ix.columns.first().map(|c| &c.key) {
            let correlation = costs::correlation(&cat, table.oid, *i);
            if !correlation.is_finite() || correlation.abs() > 1.0 {
                return None;
            }
        }
    }
    let settings = ctx.sess.settings.borrow();
    if settings
        .get_str("debug_parallel_query")
        .is_some_and(|v| v != "off")
    {
        return None;
    }
    drop(settings);
    let cfg = crate::sql::paths::parallel::ParCfg::read(ctx);
    let g = crate::sql::paths::Gucs::read(ctx);
    if ![
        cfg.setup_cost,
        cfg.tuple_cost,
        g.seq_page_cost,
        g.random_page_cost,
        g.cache_pages,
    ]
    .iter()
    .all(|n| n.is_finite() && *n >= 0.0)
        || !g.work_mem.is_finite()
        || g.work_mem <= 0.0
    {
        return None;
    }
    // Gather and Gather Merge add setup to a nonnegative partial cost. Neither
    // comparison can prefer them to an enabled serial path strictly below setup.
    #[cfg(debug_assertions)]
    let (description, reads) =
        crate::sql::literals::recording(|| crate::sql::paths::describe(ctx, s));
    #[cfg(not(debug_assertions))]
    let description = crate::sql::paths::describe(ctx, s);
    let d = description?;
    (!d.disabled
        && d.startup.is_finite()
        && d.startup >= 0.0
        && d.total.is_finite()
        && d.total >= d.startup
        && d.total < cfg.setup_cost)
        .then_some(NoGather {
            #[cfg(debug_assertions)]
            reads,
        })
}
