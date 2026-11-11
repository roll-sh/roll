//! Redundant sort keys (pathkeys.c's pathkey_is_redundant): an ORDER BY key whose
//! equivalence class has a constant, or that an earlier key's class already orders by,
//! adds nothing to the sort. The classes are those of the plan below the sort: the
//! equalities of its inner joins and restrictions (an outer join's nullable side keeps
//! its own).

use super::*;

/// Equivalence classes over a plan's output columns: a union-find, and which classes hold
/// a constant.
struct Classes {
    parent: Vec<usize>,
    constant: Vec<bool>,
    /// Expressions (over the row) a clause equates with a constant.
    const_exprs: Vec<Expr>,
    /// Columns computing a pulled-up subquery's sub-select output. Postgres copies such an
    /// output into each place reading it (pullup_replace_vars, each copy a SubPlan of its
    /// own), so a clause's copy is never equal() to the target list's: no clause puts
    /// the column in a class (get_eclass_for_sort_expr finds no member for the sort key's
    /// copy) or makes it constant.
    subsel: Vec<bool>,
}

impl Classes {
    fn new(n: usize) -> Classes {
        Classes {
            parent: (0..n).collect(),
            constant: vec![false; n],
            const_exprs: Vec::new(),
            subsel: vec![false; n],
        }
    }

    fn copied(&self, a: usize) -> bool {
        self.subsel.get(a).copied().unwrap_or(false)
    }

    fn find(&mut self, x: usize) -> usize {
        let mut r = x;
        while self.parent[r] != r {
            r = self.parent[r];
        }
        let mut y = x;
        while self.parent[y] != r {
            let n = self.parent[y];
            self.parent[y] = r;
            y = n;
        }
        r
    }

    fn union(&mut self, a: usize, b: usize) {
        if a >= self.parent.len() || b >= self.parent.len() || self.copied(a) || self.copied(b) {
            return;
        }
        let (x, y) = (self.find(a), self.find(b));
        if x != y {
            self.parent[y] = x;
            self.constant[x] |= self.constant[y];
        }
    }

    fn set_const(&mut self, a: usize) {
        if a < self.parent.len() && !self.copied(a) {
            let r = self.find(a);
            self.constant[r] = true;
        }
    }

    fn is_const(&mut self, a: usize) -> bool {
        let r = self.find(a);
        self.constant[r]
    }

    /// The classes of `inner`'s columns as seen through `map` (output column → inner
    /// column).
    fn mapped(inner: &mut Classes, map: &[Option<usize>]) -> Classes {
        let mut out = Classes::new(map.len());
        out.map_from(inner, map);
        out
    }

    fn map_from(&mut self, inner: &mut Classes, map: &[Option<usize>]) {
        let out = self;
        for (i, a) in map.iter().enumerate() {
            let Some(a) = a else { continue };
            if *a >= inner.parent.len() {
                continue;
            }
            if inner.is_const(*a) {
                out.set_const(i);
            }
            for (j, b) in map.iter().enumerate().skip(i + 1) {
                if let Some(b) = b
                    && *b < inner.parent.len()
                    && inner.find(*a) == inner.find(*b)
                {
                    out.union(i, j);
                }
            }
        }
    }
}

/// A column, through the casts and relabelings an equality reads through.
fn col_of(e: &Expr) -> Option<usize> {
    match e {
        Expr::Col(i, _) => Some(*i),
        Expr::Relabel(x, _) => col_of(x),
        Expr::Cast(x, from, to, false)
            if (from.is_string() && to.is_string())
                || matches!(
                    (from, to),
                    (DataType::Int2, DataType::Int4 | DataType::Int8) | (DataType::Int4, DataType::Int8)
                ) =>
        {
            col_of(x)
        }
        // (A domain without constraints: a RelabelType once eval_const_expressions drops
        // its CoerceToDomain, which equivalence classes see through.)
        Expr::CoerceToDomain(x, d) if !d.array && !d.not_null && d.checks.is_empty() => col_of(x),
        _ => None,
    }
}

/// Take in the equalities among `pred`'s conjuncts (column = column, column = constant).
fn absorb(c: &mut Classes, pred: &Expr, ps: Params) {
    absorb_in(c, pred, &|_| None, ps)
}

/// The parameters of the nested loop whose inner side a plan is (its `Outer(1, k)` are
/// those values of the outer row), None outside one.
type Params<'a> = Option<&'a [Expr]>;

/// A value of no column the same for every row: a pseudo-constant, but not a nested loop
/// parameter that is a column of the outer side (a join clause the inner side enforces).
fn constant_value(k: &Expr, ps: Params) -> bool {
    if !k.columns().is_empty() || !pseudo_constant(k) {
        return false;
    }
    let Some(ps) = ps else { return true };
    // (A parameter reading an enclosing nested loop's parameters is left alone: not known
    // constant here.)
    let reads_params = |v: &Expr| {
        let mut any = false;
        v.walk(&mut |x| any |= matches!(x, Expr::Outer(1, ..)));
        any
    };
    let mut ok = true;
    k.walk(&mut |x| {
        if let Expr::Outer(1, j, _) = x {
            ok &= ps.get(*j).is_some_and(|v| v.columns().is_empty() && pseudo_constant(v) && !reads_params(v));
        }
    });
    ok
}

/// `absorb`, knowing the columns' collations: a column equated with a constant under its
/// own nondeterministic collation is a class with that constant (its pathkeys, of that
/// collation, are redundant), though no class joins columns under one.
fn absorb_in(c: &mut Classes, pred: &Expr, coll: &dyn Fn(usize) -> Option<String>, ps: Params) {
    let mut parts = Vec::new();
    conjuncts(pred.clone(), &mut parts);
    for q in parts {
        let Expr::Call(def, args, _) = &q else { continue };
        // (An expression equated with a constant, of whatever types.)
        if def.name == "=" && args.len() == 2 && !q.is_volatile() && col_of(&args[0]).is_none() && col_of(&args[1]).is_none() {
            for (e, k) in [(&args[0], &args[1]), (&args[1], &args[0])] {
                if !e.columns().is_empty() && constant_value(k, ps) {
                    c.const_exprs.push(e.clone());
                }
            }
            continue;
        }
        if def.name == "="
            && args.len() == 2
            && !q.is_volatile()
            && let Some(dc) = def.coll.as_ref().filter(|c| !c.deterministic)
        {
            let own = |k: usize| coll(k).as_deref() == Some(dc.name.as_str());
            match (col_of(&args[0]), col_of(&args[1])) {
                (Some(a), None) if constant_value(&args[1], ps) && own(a) => {
                    c.set_const(a)
                }
                (None, Some(b)) if constant_value(&args[0], ps) && own(b) => {
                    c.set_const(b)
                }
                _ => {}
            }
            continue;
        }
        if def.name != "="
            || args.len() != 2
            || q.is_volatile()
            || def.coll.as_ref().is_some_and(|c| !c.deterministic)
            || (args[0].ty().without_typmod() != args[1].ty().without_typmod()
                // (integer_ops spans the integer types: one class across them.)
                && !(matches!(args[0].ty(), DataType::Int2 | DataType::Int4 | DataType::Int8)
                    && matches!(args[1].ty(), DataType::Int2 | DataType::Int4 | DataType::Int8)))
        {
            continue;
        }
        match (col_of(&args[0]), col_of(&args[1])) {
            (Some(a), Some(b)) => c.union(a, b),
            (Some(a), None) if constant_value(&args[1], ps) => c.set_const(a),
            (None, Some(b)) if constant_value(&args[0], ps) => c.set_const(b),
            (None, None) => {
                for (e, k) in [(&args[0], &args[1]), (&args[1], &args[0])] {
                    if !e.columns().is_empty() && constant_value(k, ps) {
                        c.const_exprs.push(e.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

/// Whether `e` holds a sub-select (not an inlined or called function).
fn has_subselect(e: &Expr) -> bool {
    let mut any = false;
    e.walk(&mut |x| any |= matches!(x, Expr::Subquery(s) if s.func.is_none() && s.call.is_none()));
    any
}

/// A partitionwise join's child join: a join (through projections and filters) over
/// partitions.
fn child_join(p: &Plan) -> bool {
    fn has_partition(p: &Plan) -> bool {
        match p {
            Plan::Scan(s) => s.part.is_some(),
            Plan::Join { left, right, .. } => has_partition(left) || has_partition(right),
            Plan::Project { input, .. } | Plan::Filter { input, .. } | Plan::Sort { input, .. } => has_partition(input),
            _ => false,
        }
    }
    match p {
        Plan::Project { input, .. } | Plan::Filter { input, .. } => child_join(input),
        Plan::Join { .. } => has_partition(p),
        _ => false,
    }
}

/// Per output column of an inheritance member (a scan, through projections): the
/// constant its filter equates it with.
fn member_consts(p: &Plan) -> Vec<Option<Expr>> {
    let w = crate::sql::binder::plan_width(p);
    match p {
        Plan::Project { input, exprs } => {
            let inner = member_consts(input);
            exprs
                .iter()
                .map(|e| match col_of(e) {
                    Some(i) => inner.get(i).cloned().flatten(),
                    None => None,
                })
                .collect()
        }
        Plan::Scan(s) => {
            let mut out = vec![None; w];
            let mut parts = Vec::new();
            if let Some(f) = &s.filter {
                conjuncts(f.clone(), &mut parts);
            }
            for q in parts {
                let Expr::Call(def, args, _) = &q else { continue };
                if def.name != "=" || args.len() != 2 || q.is_volatile() || def.coll.as_ref().is_some_and(|c| !c.deterministic) {
                    continue;
                }
                for (a, b) in [(&args[0], &args[1]), (&args[1], &args[0])] {
                    if let Some(i) = col_of(a)
                        && b.columns().is_empty()
                        && matches!(b, Expr::Const(..))
                        && i < w
                    {
                        out[i] = Some(b.clone());
                    }
                }
            }
            out
        }
        _ => vec![None; w],
    }
}

/// Copy `inner`'s classes onto `c`'s columns from `off` on.
fn import(c: &mut Classes, inner: &mut Classes, off: usize) {
    for i in 0..inner.parent.len().min(c.parent.len().saturating_sub(off)) {
        c.subsel[off + i] = inner.subsel[i];
    }
    for i in 0..inner.parent.len() {
        if off + i >= c.parent.len() {
            break;
        }
        let r = inner.find(i);
        c.union(off + r, off + i);
        if inner.constant[r] {
            c.set_const(off + i);
        }
    }
}

fn classes(p: &Plan) -> Classes {
    classes_in(p, None)
}

/// `classes` of a plan that is (inside) a nested loop's parameterized inner side (`ps`).
fn classes_in(p: &Plan, ps: Params) -> Classes {
    let w = crate::sql::binder::plan_width(p);
    match p {
        Plan::Scan(s) => {
            let mut c = Classes::new(w);
            if let Some(f) = &s.filter {
                absorb_in(&mut c, f, &|i| s.table.columns.get(i).and_then(|col| col.collation.clone()), ps);
            }
            c
        }
        // (A relation found empty keeps the classes its quals made.)
        Plan::Empty { info: Some(d), .. } if d.ec.len() == w => {
            let mut c = Classes::new(w);
            for (i, &(r, k)) in d.ec.iter().enumerate() {
                c.union(r, i);
                if k {
                    c.set_const(i);
                }
            }
            c
        }
        Plan::Filter { input, pred } => {
            let mut c = classes_in(input, ps);
            // (A HAVING makes no equivalence classes.)
            if !matches!(&**input, Plan::Agg { .. }) {
                absorb_in(&mut c, pred, &|i| crate::sql::binder::optimize::output_collation(input, i), ps);
            }
            c
        }
        Plan::Sort { input, .. } | Plan::Limit { input, .. } | Plan::Context { input, .. } => classes_in(input, ps),
        // (A subquery scan's outputs are its Vars: one member wherever they are read.)
        Plan::Barrier { input, .. } => {
            let mut c = classes_in(input, ps);
            c.subsel.iter_mut().for_each(|f| *f = false);
            c
        }
        // An inheritance tree's columns the parent's clauses (pushed into every member)
        // equate with constants.
        Plan::Append { inputs, inh: Some(_), .. } => {
            let mut c = Classes::new(w);
            let mut live = inputs.iter().filter(|i| !matches!(&***i, Plan::Empty { .. })).peekable();
            if live.peek().is_none() {
                return c;
            }
            // (The same constant in every member: one class with it.)
            let mut fixed: Vec<Option<Expr>> = Vec::new();
            for (n, i) in live.enumerate() {
                let m = member_consts(i);
                if n == 0 {
                    fixed = m;
                    continue;
                }
                for (k, f) in fixed.iter_mut().enumerate() {
                    if !f.as_ref().is_some_and(|x| m.get(k).and_then(|y| y.as_ref()).is_some_and(|y| y.same_as(x))) {
                        *f = None;
                    }
                }
            }
            for (k, f) in fixed.into_iter().enumerate() {
                if f.is_some() && k < w {
                    c.set_const(k);
                }
            }
            c
        }
        // (A partitionwise join's child joins: the columns every one equates, the parent
        // join's classes. Not a UNION ALL's arms: their classes aren't the query's.)
        Plan::Append { inputs, inh: None, .. } if !inputs.is_empty() && inputs.iter().all(|i| child_join(i)) => {
            let mut c = Classes::new(w);
            let mut members: Vec<Classes> = inputs
                .iter()
                .filter(|i| !matches!(&***i, Plan::Empty { .. }))
                .map(|i| classes_in(i, ps))
                .collect();
            if members.is_empty() {
                return c;
            }
            for i in 0..w {
                for j in 0..i {
                    if members.iter_mut().all(|m| i < m.parent.len() && j < m.parent.len() && m.find(i) == m.find(j)) {
                        c.union(i, j);
                    }
                }
            }
            c
        }
        // A grouping's keys keep their classes (the aggregates make none).
        Plan::Agg { input, groups, sets: None, .. } => {
            let mut inner = classes_in(input, ps);
            let mut map: Vec<Option<usize>> = groups.iter().map(col_of).collect();
            map.resize(w, None);
            Classes::mapped(&mut inner, &map)
        }
        Plan::Project { input, exprs } => {
            let mut inner = classes_in(input, ps);
            let map: Vec<Option<usize>> = exprs.iter().map(col_of).collect();
            let mut out = Classes::new(map.len());
            for (i, e) in exprs.iter().enumerate() {
                out.subsel[i] = match map[i] {
                    Some(k) => inner.copied(k),
                    None => has_subselect(e),
                };
            }
            out.map_from(&mut inner, &map);
            // A constant output column is a class with a constant; so is one computing an
            // expression a clause below equates with one.
            for (i, e) in exprs.iter().enumerate() {
                if matches!(e, Expr::Const(..)) || inner.const_exprs.iter().any(|x| x.same_as(e)) {
                    out.set_const(i);
                }
            }
            out
        }
        Plan::Join {
            kind,
            left,
            right,
            cond,
            hash,
            nleft,
            params,
            ..
        } => {
            let nl = *nleft;
            // (The inner side of a nested loop passing it parameters: its equalities with
            // them are join clauses, not constants.)
            let rps = if params.is_empty() { ps } else { Some(&params[..]) };
            let mut c = Classes::new(w);
            // (An outer join's nullable side's columns aren't its input's any more.)
            match kind {
                JoinKind::Right => {
                    let mut rc = classes_in(right, rps);
                    import(&mut c, &mut rc, nl);
                }
                JoinKind::Full => {}
                _ => {
                    let mut lc = classes_in(left, ps);
                    import(&mut c, &mut lc, 0);
                    c.const_exprs.append(&mut lc.const_exprs);
                }
            }
            if *kind == JoinKind::Inner {
                let mut rc = classes_in(right, rps);
                import(&mut c, &mut rc, nl);
                if let Some(e) = cond {
                    absorb(&mut c, e, ps);
                }
                if let Some((lk, rk)) = hash {
                    for (a, b) in lk.iter().zip(rk) {
                        if let (Some(a), Some(b)) = (col_of(a), col_of(b)) {
                            c.union(a, nl + b);
                        }
                    }
                }
            }
            c
        }
        _ => Classes::new(w),
    }
}

impl Planner<'_, '_> {
    /// Drop the keys of the Sort at `p` that its input's equivalence classes make
    /// redundant; the Sort itself when none is left.
    pub(super) fn redundant_sort_keys(&self, p: &mut Plan) {
        let Plan::Sort { input, keys } = p else {
            return;
        };
        // (A key sorted under its column's own collation is that column's pathkey; one under
        // another ordering operator, its own.)
        let own_order = |k: &SortKey| match &k.using {
            None => true,
            Some(u) => {
                matches!(u.name, "<" | ">")
                    && u.coll.as_ref().is_some_and(|c| {
                        col_of(&k.expr).is_some_and(|col| {
                            crate::sql::binder::optimize::output_collation(input, col).as_deref() == Some(c.name.as_str())
                        })
                    })
            }
        };
        if keys.iter().any(|k| !own_order(k)) {
            return;
        }
        let mut c = classes(input);
        let mut seen: Vec<usize> = Vec::new();
        let mut kept = Vec::new();
        let mut changed = false;
        for k in keys.iter() {
            if let Some(col) = col_of(&k.expr)
                && col < c.parent.len()
            {
                let r = c.find(col);
                if c.constant[r] || seen.contains(&r) {
                    changed = true;
                    continue;
                }
                seen.push(r);
            }
            kept.push(k.clone());
        }
        if !changed {
            return;
        }
        if kept.is_empty() {
            let Plan::Sort { input, .. } = std::mem::replace(p, Plan::One) else {
                unreachable!()
            };
            *p = Arc::try_unwrap(input).unwrap_or_else(|a| (*a).clone());
        } else {
            *keys = kept;
        }
    }
}

/// The output columns of `p` its clauses equate with a constant (their sort keys are
/// redundant).
pub fn constant_columns(p: &Plan) -> Vec<bool> {
    let mut c = classes(p);
    (0..c.parent.len()).map(|i| c.is_const(i)).collect()
}

/// Per output column of `p`: its equivalence class (a representative column), and
/// whether the class holds a constant.
pub fn column_classes(p: &Plan) -> Vec<(usize, bool)> {
    let mut c = classes(p);
    (0..c.parent.len())
        .map(|i| {
            let r = c.find(i);
            (r, c.constant[r])
        })
        .collect()
}
