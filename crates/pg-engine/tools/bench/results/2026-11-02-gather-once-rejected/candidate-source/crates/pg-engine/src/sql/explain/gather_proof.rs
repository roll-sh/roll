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

/// One eligible leaf in a single Gather-discovery Builder; no estimate is retained.
pub(super) struct Probe {
    scan: Option<usize>,
    #[cfg(debug_assertions)]
    serial: Option<(f64, f64)>,
}

impl Probe {
    pub(super) fn for_root(plan: &Arc<Plan>) -> Option<Self> {
        if crate::sql::literals::is_recording()
            || !WINDOW_TLIST.with_borrow(|w| w.is_empty())
            || FILTER_OVER_WINDOW.get()
            || FOREIGN_TOP.get()
        {
            return None;
        }
        let mut p = plan;
        while let Plan::Project { input, exprs } = &**p {
            let width = crate::sql::binder::plan_width(input);
            if !exprs
                .iter()
                .all(|e| matches!(e, Expr::Col(i, ty) if *i < width && integral(ty)))
            {
                return None;
            }
            p = input;
        }
        matches!(&**p, Plan::Scan(_)).then_some(Self {
            scan: Some(addr(p)),
            #[cfg(debug_assertions)]
            serial: None,
        })
    }

    pub(super) fn prune(
        &mut self,
        p: &Arc<Plan>,
        s: &ScanPlan,
        d: &super::super::paths::ScanDesc,
        ctx: &ExecCtx,
        cfg: Option<&super::super::paths::parallel::ParCfg>,
        member: bool,
    ) -> bool {
        if member || self.scan != Some(addr(p)) {
            return false;
        }
        self.scan = None;
        #[cfg(test)]
        super::super::paths::test_counts::note(|n| n.attempted += 1);
        let proved = cfg.is_some_and(|cfg| no_gather(s, d, ctx, cfg));
        if proved {
            #[cfg(debug_assertions)]
            {
                self.serial = Some((d.startup, d.total));
            }
            #[cfg(test)]
            super::super::paths::test_counts::note(|n| n.proved += 1);
        }
        proved
    }

    #[cfg(debug_assertions)]
    pub(super) fn check_root(&self, root: &ExNode) {
        if let Some((startup, total)) = self.serial {
            debug_assert!(!root.disabled);
            debug_assert_eq!(root.startup_cost.to_bits(), startup.to_bits());
            debug_assert_eq!(root.total_cost.to_bits(), total.to_bits());
            debug_assert!(!parallel::has_gather(root));
        }
    }
}

// Every admitted Gather costs at least setup, while the serial root has no
// computed projection or other node adding cost. Unknown inputs use full discovery.
fn no_gather(
    s: &ScanPlan,
    d: &super::super::paths::ScanDesc,
    ctx: &ExecCtx,
    cfg: &super::super::paths::parallel::ParCfg,
) -> bool {
    if cfg.max_workers == 0
        || !cfg.setup_cost.is_finite()
        || cfg.setup_cost <= 0.0
        || d.disabled
        || !d.startup.is_finite()
        || d.startup < 0.0
        || !d.total.is_finite()
        || d.total < d.startup
        || d.total >= cfg.setup_cost
        || crate::sql::literals::is_recording()
    {
        return false;
    }
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
    } = s;
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
        return false;
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
        return false;
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
                return false;
            }
        }
        _ => return false,
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
            return false;
        }
    }
    let cat = ctx.catalog();
    if table
        .constraints
        .iter()
        .any(|c| matches!(c.kind, crate::catalog::ConstraintKind::Exclusion { .. }))
        || filter
            .as_ref()
            .is_some_and(|f| crate::sql::paths::parallel::hazard(f, &cat) != 's')
    {
        return false;
    }
    if cat.class_has_subclass.contains(&table.oid)
        || cat.statistics.values().any(|stat| stat.table == table.oid)
    {
        return false;
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
            return false;
        }
        if let Some(IndexKey::Column(i)) = ix.columns.first().map(|c| &c.key) {
            let correlation = costs::correlation(&cat, table.oid, *i);
            if !correlation.is_finite() || correlation.abs() > 1.0 {
                return false;
            }
        }
    }
    let settings = ctx.sess.settings.borrow();
    if settings
        .get_str("debug_parallel_query")
        .is_some_and(|v| v != "off")
    {
        return false;
    }
    drop(settings);

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
        return false;
    }
    true
}
