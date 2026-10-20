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
}

impl Classes {
    fn new(n: usize) -> Classes {
        Classes {
            parent: (0..n).collect(),
            constant: vec![false; n],
            const_exprs: Vec::new(),
        }
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
        if a >= self.parent.len() || b >= self.parent.len() {
            return;
        }
        let (x, y) = (self.find(a), self.find(b));
        if x != y {
            self.parent[y] = x;
            self.constant[x] |= self.constant[y];
        }
    }

    fn set_const(&mut self, a: usize) {
        if a < self.parent.len() {
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
        out
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
        _ => None,
    }
}

/// Take in the equalities among `pred`'s conjuncts (column = column, column = constant).
fn absorb(c: &mut Classes, pred: &Expr) {
    let mut parts = Vec::new();
    conjuncts(pred.clone(), &mut parts);
    for q in parts {
        let Expr::Call(def, args, _) = &q else { continue };
        // (An expression equated with a constant, of whatever types.)
        if def.name == "=" && args.len() == 2 && !q.is_volatile() && col_of(&args[0]).is_none() && col_of(&args[1]).is_none() {
            for (e, k) in [(&args[0], &args[1]), (&args[1], &args[0])] {
                if !e.columns().is_empty() && k.columns().is_empty() && pseudo_constant(k) {
                    c.const_exprs.push(e.clone());
                }
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
            (Some(a), None) if args[1].columns().is_empty() && pseudo_constant(&args[1]) => c.set_const(a),
            (None, Some(b)) if args[0].columns().is_empty() && pseudo_constant(&args[0]) => c.set_const(b),
            (None, None) => {
                for (e, k) in [(&args[0], &args[1]), (&args[1], &args[0])] {
                    if !e.columns().is_empty() && k.columns().is_empty() && pseudo_constant(k) {
                        c.const_exprs.push(e.clone());
                    }
                }
            }
            _ => {}
        }
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
    let w = crate::sql::binder::plan_width(p);
    match p {
        Plan::Scan(s) => {
            let mut c = Classes::new(w);
            if let Some(f) = &s.filter {
                absorb(&mut c, f);
            }
            c
        }
        Plan::Filter { input, pred } => {
            let mut c = classes(input);
            // (A HAVING makes no equivalence classes.)
            if !matches!(&**input, Plan::Agg { .. }) {
                absorb(&mut c, pred);
            }
            c
        }
        Plan::Sort { input, .. } | Plan::Limit { input, .. } | Plan::Barrier { input, .. } | Plan::Context { input, .. } => {
            classes(input)
        }
        // An inheritance tree's columns the parent's clauses (pushed into every member)
        // equate with constants.
        Plan::Append { inputs, inh: Some(_) } => {
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
        Plan::Project { input, exprs } => {
            let mut inner = classes(input);
            let map: Vec<Option<usize>> = exprs.iter().map(col_of).collect();
            let mut out = Classes::mapped(&mut inner, &map);
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
            ..
        } => {
            let nl = *nleft;
            let mut c = Classes::new(w);
            // (An outer join's nullable side's columns aren't its input's any more.)
            match kind {
                JoinKind::Right => {
                    let mut rc = classes(right);
                    import(&mut c, &mut rc, nl);
                }
                JoinKind::Full => {}
                _ => {
                    let mut lc = classes(left);
                    import(&mut c, &mut lc, 0);
                    c.const_exprs.append(&mut lc.const_exprs);
                }
            }
            if *kind == JoinKind::Inner {
                let mut rc = classes(right);
                import(&mut c, &mut rc, nl);
                if let Some(e) = cond {
                    absorb(&mut c, e);
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
        if keys.iter().any(|k| k.using.is_some()) {
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
