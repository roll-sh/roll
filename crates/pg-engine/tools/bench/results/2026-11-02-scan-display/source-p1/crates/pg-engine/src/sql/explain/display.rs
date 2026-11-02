use super::*;

fn scalar_type(ty: &DataType) -> bool {
    matches!(
        ty,
        DataType::Bool
            | DataType::Int2
            | DataType::Int4
            | DataType::Int8
            | DataType::Float4
            | DataType::Float8
            | DataType::Numeric(_)
            | DataType::Text
            | DataType::Varchar(_)
            | DataType::Bpchar(_)
            | DataType::Name
            | DataType::Char
            | DataType::Bytea
            | DataType::Date
            | DataType::Time(_)
            | DataType::Timetz(_)
            | DataType::Timestamp(_)
            | DataType::Timestamptz(_)
            | DataType::Interval(_)
            | DataType::Uuid
            | DataType::Oid
    )
}

fn builtin_scalar(def: &Arc<super::super::func::FuncDef>) -> bool {
    if def.oid == 0 || def.srf.is_some() {
        return false;
    }
    let registry = super::super::func::registry();
    [&registry.funcs, &registry.binops, &registry.prefix]
        .into_iter()
        .filter_map(|functions| functions.get(def.name))
        .flatten()
        .any(|builtin| Arc::ptr_eq(builtin, def))
}

/// These expressions cannot register SubPlans, inline outer parameters, or expand GROUPING.
pub(super) fn scalar_condition(expr: &Expr) -> bool {
    let mut accepted = true;
    expr.walk(&mut |e| {
        if !accepted {
            return;
        }
        accepted = match e {
            Expr::Const(_, ty) | Expr::Col(_, ty) | Expr::Param(_, ty) | Expr::CaseTest(ty) => {
                scalar_type(ty)
            }
            Expr::Call(def, _, ty) => scalar_type(ty) && builtin_scalar(def),
            Expr::Cast(_, from, to, _) => scalar_type(from) && scalar_type(to),
            Expr::And(_) | Expr::Or(_) | Expr::Not(_) | Expr::IsNull(..) | Expr::BoolTest(..) => {
                true
            }
            Expr::Coalesce(_, ty) | Expr::Case { ty, .. } => scalar_type(ty),
            _ => false,
        };
    });
    accepted
}

#[cfg(test)]
pub(crate) fn assert_same_estimate(plan: &Arc<Plan>, ctx: &ExecCtx) -> (usize, Vec<String>) {
    fn real_keys(plan: &Arc<Plan>, keys: &mut std::collections::HashSet<usize>) {
        if !keys.insert(addr(plan)) {
            return;
        }
        if let Plan::Scan(scan) = &**plan {
            keys.insert(scan_addr(scan));
        }
        for input in plan_inputs(plan) {
            real_keys(input, keys);
        }
        let mut exprs = Vec::new();
        node_exprs(plan, &mut exprs);
        for expr in exprs {
            expr.walk(&mut |e| {
                if let Expr::Subquery(s) = e {
                    real_keys(&s.plan, keys);
                }
            });
        }
    }
    fn pair_key(
        a: usize,
        b: &mut usize,
        real: &std::collections::HashSet<usize>,
        paired: &mut (HashMap<usize, usize>, HashMap<usize, usize>),
    ) {
        if real.contains(&a) || real.contains(b) {
            assert_eq!(a, *b);
            return;
        }
        assert_eq!(*paired.0.entry(a).or_insert(*b), *b);
        assert_eq!(*paired.1.entry(*b).or_insert(a), a);
        *b = a;
    }
    fn normalize(
        original: &mut ExNode,
        candidate: &mut ExNode,
        real: &std::collections::HashSet<usize>,
        paired: &mut (HashMap<usize, usize>, HashMap<usize, usize>),
        node_types: &mut Vec<String>,
    ) -> usize {
        node_types.push(original.title.clone());
        assert_eq!(original.key.is_some(), candidate.key.is_some());
        if let (Some(a), Some(b)) = (original.key, &mut candidate.key) {
            pair_key(a, b, real, paired);
        }
        for (a, b) in original.details.iter().zip(&mut candidate.details) {
            match (a, b) {
                (Detail::Removed(_, a), Detail::Removed(_, b)) => {
                    assert_eq!(a.len(), b.len());
                    for (a, b) in a.iter().zip(b) {
                        match (a, b) {
                            (Removed::Scan(a), Removed::Scan(b))
                            | (Removed::Diff(a), Removed::Diff(b)) => pair_key(*a, b, real, paired),
                            _ => panic!("removed-row source changed"),
                        }
                    }
                }
                (Detail::IndexSearches(a), Detail::IndexSearches(b))
                | (Detail::HeapFetches(a), Detail::HeapFetches(b))
                | (Detail::HeapBlocks(a), Detail::HeapBlocks(b)) => pair_key(*a, b, real, paired),
                _ => {}
            }
        }
        assert_eq!(
            original.total_cost.to_bits(),
            candidate.total_cost.to_bits()
        );
        assert_eq!(
            original.startup_cost.to_bits(),
            candidate.startup_cost.to_bits()
        );
        assert_eq!(original.rows.to_bits(), candidate.rows.to_bits());
        let columns = |n: &ExNode| {
            format!(
                "{:?}",
                n.cols
                    .iter()
                    .map(|c| (
                        c.width,
                        c.var.as_ref().map(|v| (
                            Arc::as_ptr(&v.table),
                            v.col,
                            v.tuples.to_bits(),
                            &v.expr,
                            v.rows.to_bits(),
                            v.inh
                        )),
                        c.distinct.map(f64::to_bits),
                        c.groups.map(f64::to_bits)
                    ))
                    .collect::<Vec<_>>()
            )
        };
        assert_eq!(columns(original), columns(candidate));
        assert_eq!(original.details.len(), candidate.details.len());
        let scan = matches!(
            original.node_type.trim_start_matches("Parallel "),
            "Seq Scan"
                | "Index Scan"
                | "Index Only Scan"
                | "Bitmap Heap Scan"
                | "Tid Scan"
                | "Tid Range Scan"
                | "Foreign Scan"
                | "Foreign Update"
                | "Foreign Delete"
        );
        let mut bytes = 0;
        for (a, b) in original.details.iter_mut().zip(&mut candidate.details) {
            if let (Detail::Text(label, before), Detail::Text(other, after)) = (a, b)
                && before != after
            {
                assert_eq!(label, other);
                assert!(
                    scan && matches!(*label, "Index Cond" | "Recheck Cond" | "Filter"),
                    "unexpected text change: {label}"
                );
                assert!(
                    after.is_empty()
                        || (after.starts_with("( AND ") && before.ends_with(&after[1..])),
                    "unexpected replacement: {before:?} -> {after:?}"
                );
                bytes += before.len().checked_sub(after.len()).unwrap();
                before.clear();
                after.clear();
            }
        }
        for (a, b) in [
            (&mut original.children, &mut candidate.children),
            (&mut original.subplans, &mut candidate.subplans),
            (&mut original.initplans, &mut candidate.initplans),
            (&mut original.partials, &mut candidate.partials),
        ] {
            assert_eq!(a.len(), b.len());
            for (a, b) in a.iter_mut().zip(b) {
                bytes += normalize(a, b, real, paired, node_types);
            }
        }
        for (a, b) in [
            (&mut original.alt, &mut candidate.alt),
            (&mut original.partial, &mut candidate.partial),
            (&mut original.safe_alt, &mut candidate.safe_alt),
        ] {
            assert_eq!(a.is_some(), b.is_some());
            if let (Some(a), Some(b)) = (a, b) {
                bytes += normalize(a, b, real, paired, node_types);
            }
        }
        assert_eq!(original.presort.is_some(), candidate.presort.is_some());
        if let (Some(a), Some(b)) = (&mut original.presort, &mut candidate.presort) {
            bytes += normalize(&mut a.0, &mut b.0, real, paired, node_types);
        }
        bytes
    }
    let mut real = std::collections::HashSet::new();
    real_keys(plan, &mut real);
    let options = estimate_options();
    let mut suppressed = 0;
    let mut node_types = Vec::new();
    for mode in 0..3 {
        let mut original = estimate_builder(plan, ctx, &options);
        original.scan_conditions = true;
        let mut candidate = estimate_builder(plan, ctx, &options);
        match mode {
            1 => {
                original.par = None;
                candidate.par = None;
            }
            2 => {
                original.outer_depth = vec![usize::MAX; 16];
                candidate.outer_depth = vec![usize::MAX; 16];
            }
            _ => {}
        }
        let (mut original, mut candidate) = if mode == 1 {
            let candidate = candidate.level(plan);
            (original.level(plan), candidate)
        } else {
            let original = original.level(plan);
            (original, candidate.level(plan))
        };
        suppressed += normalize(
            &mut original,
            &mut candidate,
            &real,
            &mut (HashMap::new(), HashMap::new()),
            &mut node_types,
        );
        assert_eq!(format!("{original:?}"), format!("{candidate:?}"));
    }
    (suppressed, node_types)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_policy_requires_actual_builtins_and_rejects_semantic_wrappers() {
        let col = || Expr::Col(0, DataType::Int4);
        let def = super::super::super::func::resolve_binop("+", &DataType::Int4, &DataType::Int4)
            .unwrap()
            .def;
        assert!(scalar_condition(&Expr::Call(
            def.clone(),
            vec![col(), Expr::Param(0, DataType::Int4)],
            DataType::Int4
        )));
        assert!(!scalar_condition(&Expr::Call(
            Arc::new((*def).clone()),
            vec![col(), col()],
            DataType::Int4
        )));
        for expr in [
            Expr::Outer(1, 0, DataType::Int4),
            Expr::Agg(0, DataType::Int4),
            Expr::Window(0, DataType::Int4),
            Expr::Srf(0, DataType::Int4),
            Expr::Grouping(vec![0]),
            Expr::Row(vec![col()], DataType::Record),
            Expr::Array(vec![col()], DataType::Array(Box::new(DataType::Int4))),
        ] {
            assert!(!scalar_condition(&expr));
            assert!(!scalar_condition(&Expr::IsNull(Box::new(expr), false)));
        }
    }
}
