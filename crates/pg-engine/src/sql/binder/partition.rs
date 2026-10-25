//! Declarative partitioning in plans: partition keys over a table's row, the partition
//! constraint levels of a partition, routing trees for inserts, and partition pruning
//! (`partprune`, with the values known at plan time or when the executor runs).

use super::Binder;
use crate::catalog::Table;
use crate::error::Result;
use crate::sql::expr::Expr;
use crate::sql::plan::{PartLeaf, PartLevel, PartRouter, PartRouting, PartTarget};
use crate::types::Value;
use std::sync::Arc;

impl Binder<'_, '_> {
    /// The support function of hash partition key column `k` when its operator class is a
    /// made one whose hash function (FUNCTION 2 for the class's input type) is a made
    /// function (the partition hash then comes from calling it).
    pub(crate) fn hash_support(&self, p: &Table, k: usize) -> Option<Arc<crate::catalog::Function>> {
        self.cat.hash_support_fn(p, k)
    }

    /// A built-in hash support function a made operator class names (FUNCTION 2).
    fn hash_support_builtin(&self, p: &Table, k: usize) -> Option<Arc<crate::sql::func::FuncDef>> {
        let oid = self.cat.hash_support_oid(p, k)?;
        if self.cat.functions.contains_key(&oid) {
            return None;
        }
        crate::sql::func::registry()
            .funcs
            .values()
            .flatten()
            .find(|d| d.oid == oid)
            .cloned()
    }

    /// Key column `k`'s value hashed by its operator class's made or built-in support
    /// function (None: hashed as its type is).
    fn hash_call(&mut self, p: &Table, k: usize, e: Expr) -> Result<Option<Expr>> {
        let seed = Expr::Const(
            Value::Int8(crate::types::hash::PARTITION_SEED as i64),
            crate::types::DataType::Int8,
        );
        if let Some(f) = self.hash_support(p, k) {
            let arg_ty = f.args.first().map(|a| a.1.clone()).unwrap_or_else(|| e.ty());
            let arg = self.coerce_implicit(e, &arg_ty)?;
            let prev = std::mem::replace(&mut self.fmgr_call, true);
            let r = self.user_call(f, vec![arg, seed], -1);
            self.fmgr_call = prev;
            return r.map(Some);
        }
        if let Some(d) = self.hash_support_builtin(p, k) {
            let arg_ty = d.args.first().cloned().unwrap_or_else(|| e.ty());
            let arg = self.coerce_implicit(e, &arg_ty)?;
            return Ok(Some(Expr::Call(d, vec![arg, seed], crate::types::DataType::Int8)));
        }
        Ok(None)
    }

    /// The key before hashing, its types and each hashed column's call over `Col(0)`, when
    /// a column is hashed by a made operator class's support function.
    fn raw_key(&mut self, p: &Arc<Table>, over: &Table) -> Result<Option<Arc<crate::sql::plan::RawPartKey>>> {
        let Some(key) = &p.partkey else {
            return Ok(None);
        };
        if key.strategy != crate::catalog::PartStrategy::Hash
            || !(0..key.cols.len()).any(|i| self.cat.hash_support_oid(p, i).is_some())
        {
            return Ok(None);
        }
        let raw = self.partition_key_values(p, over)?;
        let mut hash = Vec::new();
        for (i, e) in raw.iter().enumerate() {
            hash.push(self.hash_call(p, i, Expr::Col(0, e.ty()))?);
        }
        // Each column's operator class's equality operator, when it is a made one.
        let eq_ops = key
            .cols
            .iter()
            .map(|c| {
                let name = c.opclass.as_ref()?;
                let oc = self
                    .cat
                    .opclasses
                    .values()
                    .find(|o| o.method == 405 && &o.name == name)?;
                let m = self.cat.opmembers.values().find(|m| {
                    !m.func
                        && m.family == oc.family
                        && m.number == 1
                        && m.left == oc.intype
                        && m.right == oc.intype
                })?;
                self.cat.operators.get(&m.object).map(|o| o.name.clone())
            })
            .collect();
        Ok(Some(Arc::new(crate::sql::plan::RawPartKey {
            types: raw.iter().map(|e| e.ty()).collect(),
            key: raw,
            hash,
            eq_ops,
        })))
    }

    /// The types `row_hash` and bound matching take a partitioned table's key as: the key
    /// columns' types, with `void` for columns `partition_key` gives as their hash already
    /// (see `hash_support`).
    pub(crate) fn partition_types(&self, p: &Table) -> Vec<crate::types::DataType> {
        let Some(key) = &p.partkey else {
            return Vec::new();
        };
        key.cols
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if self.hash_support(p, i).is_some() || self.hash_support_builtin(p, i).is_some() {
                    crate::types::DataType::Void
                } else {
                    c.ty.clone()
                }
            })
            .collect()
    }

    /// A partitioned table's key over the row of `over` (the table itself or a descendant).
    /// A hash key column whose operator class has a made support function is its hash
    /// (`support(key, seed)`, as compute_partition_hash_value calls it).
    pub(crate) fn partition_key(&mut self, p: &Arc<Table>, over: &Table) -> Result<Vec<Expr>> {
        let mut out = self.partition_key_values(p, over)?;
        for (i, e) in out.iter_mut().enumerate() {
            if let Some(h) = self.hash_call(p, i, e.clone())? {
                *e = h;
            }
        }
        Ok(out)
    }

    /// A partitioned table's key over the row of `over`, as values.
    fn partition_key_values(&mut self, p: &Arc<Table>, over: &Table) -> Result<Vec<Expr>> {
        let Some(key) = &p.partkey else {
            return Ok(Vec::new());
        };
        let saved = std::mem::take(&mut self.perms);
        let mut out = Vec::new();
        let mut r = Ok(());
        for k in &key.cols {
            match (k.col, &k.expr) {
                (Some(i), _) => {
                    let j = if p.oid == over.oid {
                        i
                    } else {
                        over.column_index(&p.columns[i].name).unwrap_or(i)
                    };
                    out.push(Expr::Col(
                        j,
                        over.columns.get(j).map_or(k.ty.clone(), |c| c.ty.clone()),
                    ));
                }
                (None, Some(sql)) => match super::ast::parse_expr(sql)
                    .and_then(|n| self.bind_over_table(p, &n, super::Clause::PartitionKey))
                {
                    Ok(e) => out.push(super::inherit::to_child(e, p, over)),
                    Err(e) => {
                        r = Err(e);
                        break;
                    }
                },
                _ => {}
            }
        }
        self.perms = saved;
        r.map(|_| out)
    }

    /// The partition constraint levels of `t` over its row, from its own bound up to the
    /// partitioned table `top` (None: the root).
    pub(crate) fn partition_levels(
        &mut self,
        t: &Arc<Table>,
        top: Option<u32>,
    ) -> Result<Option<Arc<Vec<PartLevel>>>> {
        let mut out = Vec::new();
        let mut cur = t.clone();
        while let Some(bound) = cur.partbound.clone() {
            if top == Some(cur.oid) {
                break;
            }
            let Some(parent) = cur
                .inherits
                .first()
                .and_then(|p| self.cat.table(*p))
                .cloned()
            else {
                break;
            };
            let Some(key) = &parent.partkey else { break };
            let siblings = self
                .cat
                .partitions(parent.oid)
                .into_iter()
                .filter(|o| *o != cur.oid)
                .filter_map(|o| self.cat.table(o).and_then(|s| s.partbound.clone()))
                .collect();
            out.push(PartLevel {
                strategy: key.strategy,
                key: self.partition_key(&parent, t)?,
                types: self.partition_types(&parent),
                bound,
                siblings,
                colls: crate::catalog::partition::key_collations(&self.cat, &parent),
                raw: self.raw_key(&parent, t)?,
                parent: parent.clone(),
            });
            cur = parent;
        }
        Ok((!out.is_empty()).then(|| Arc::new(out)))
    }

    /// Routing for inserts into the partitioned table `root`.
    pub(crate) fn partition_routing(
        &mut self,
        root: &Arc<Table>,
    ) -> Result<Option<Arc<PartRouting>>> {
        if root.partkey.is_none() {
            return Ok(None);
        }
        let mut leaves = Vec::new();
        let router = self.router(root, root, &mut leaves)?;
        let check = self.partition_levels(root, None)?;
        Ok(Some(Arc::new(PartRouting {
            router,
            leaves,
            check,
        })))
    }

    fn router(
        &mut self,
        root: &Arc<Table>,
        p: &Arc<Table>,
        leaves: &mut Vec<PartLeaf>,
    ) -> Result<PartRouter> {
        let key = p.partkey.clone().unwrap();
        let key_names = key
            .cols
            .iter()
            .map(|k| match (k.col, &k.expr) {
                (Some(i), _) => p.columns[i].name.clone(),
                // (pg_get_partkeydef_columns, pretty: parenthesized unless a function call.)
                (_, Some(e)) => {
                    let shown = crate::sql::ruleutils::stored_expr(
                        self.ctx,
                        p,
                        e,
                        crate::sql::ruleutils::Stored::Plain,
                        true,
                    )
                    .unwrap_or_else(|| e.clone());
                    let bound = super::ast::parse_expr(e).ok().and_then(|n| {
                        crate::sql::ruleutils::bind_stored(
                            self.ctx,
                            p,
                            &n,
                            crate::sql::ruleutils::Stored::Plain,
                        )
                    });
                    if bound
                        .as_ref()
                        .is_some_and(crate::sql::ruleutils::looks_like_function)
                    {
                        shown
                    } else {
                        format!("({shown})")
                    }
                }
                _ => String::new(),
            })
            .collect();
        let mut parts = Vec::new();
        for oid in self.cat.routing_partitions(p.oid) {
            let Some(c) = self.cat.table(oid).cloned() else {
                continue;
            };
            let bound = c.partbound.clone().unwrap();
            let target = if c.partkey.is_some() {
                PartTarget::Sub(Box::new(self.router(root, &c, leaves)?))
            } else {
                let mut rules = self.table_rules(&c)?;
                let check = rules.part_check.clone();
                // Routed rows fit by construction; a BEFORE ROW trigger moving one is caught.
                let m = {
                    use crate::catalog::trigger_type as tt;
                    tt::ROW | tt::BEFORE | tt::INSERT
                };
                if !rules.triggers.iter().any(|t| t.trig.tgtype & m == m) {
                    rules.part_check = None;
                }
                let map = root
                    .columns
                    .iter()
                    .map(|rc| {
                        if rc.dropped {
                            None
                        } else {
                            c.column_index(&rc.name)
                        }
                    })
                    .collect();
                leaves.push(PartLeaf { rules, check, map });
                PartTarget::Leaf(leaves.len() - 1)
            };
            parts.push((bound, target));
        }
        Ok(PartRouter {
            table: p.clone(),
            strategy: key.strategy,
            key: self.partition_key(p, root)?,
            key_names,
            types: self.partition_types(p),
            parts,
            colls: crate::catalog::partition::key_collations(&self.cat, p),
        })
    }
}

/// Which leaf of a routing tree takes a row (over the root's columns); Err(router) names the
/// partitioned table with no partition for it.
pub fn route_row<'a>(
    r: &'a PartRouter,
    key_of: &mut dyn FnMut(&[Expr]) -> Result<Vec<Value>>,
) -> Result<std::result::Result<usize, (&'a PartRouter, Vec<Value>)>> {
    let key = key_of(&r.key)?;
    let routed = {
        let _g = crate::catalog::partition::under(r.colls.clone());
        crate::catalog::partition::route(r.parts.iter().map(|(b, _)| b), &key, &r.types)
    };
    match routed {
        Some(i) => match &r.parts[i].1 {
            PartTarget::Leaf(l) => Ok(Ok(*l)),
            PartTarget::Sub(s) => route_row(s, key_of),
        },
        None => Ok(Err((r, key))),
    }
}

// ── Pruning ──

thread_local! {
    /// enable_partition_pruning, as the statement being bound sees it (`Binder::new`).
    static PRUNING: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Note enable_partition_pruning for the statement bound next on this thread.
pub fn set_pruning(on: bool) {
    PRUNING.with(|p| p.set(on));
}

/// Whether partition pruning with the restriction `quals` (over a member's row) leaves the
/// member out (partprune.c, at each level of the partition tree); never with
/// enable_partition_pruning off.
pub fn pruned(quals: &[Expr], levels: &[PartLevel]) -> bool {
    PRUNING.with(|p| p.get())
        && (0..levels.len()).any(|i| super::partprune::level_pruned(quals, levels, i))
}

/// When a clause's comparison value becomes known: when the executor starts (parameters,
/// stable functions), or only once it runs (an uncorrelated subquery's value).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Known {
    Start,
    Running,
    /// Each time the plan is (re)scanned: parameters from an outer row too (exec
    /// pruning).
    Exec,
}

fn knowable(e: &Expr) -> Option<Known> {
    // (A constant under a COLLATE is a constant: the COLLATE stays.)
    let mut c = e;
    while let Expr::Collate(x, ..) | Expr::Relabel(x, _) = c {
        c = x;
    }
    if matches!(c, Expr::Const(..)) {
        return None;
    }
    let mut k = Some(Known::Start);
    e.walk(&mut |x| match x {
        Expr::Col(..) => k = None,
        // (An outer row's value: known when the plan is rescanned for it.)
        Expr::Outer(..) => k = k.map(|k| k.max(Known::Exec)),
        Expr::Call(d, ..) if d.volatility == 'v' => k = None,
        // A call of a stable function over values known at the start: known then too
        // (its arguments are walked as its params).
        Expr::Subquery(s) if s.call.is_some() && s.kind == crate::sql::expr::SubqueryKind::Scalar => {
            if s.func.is_none_or(|v| v == 'v') {
                k = None;
            }
        }
        Expr::Subquery(s) => {
            if !s.params.is_empty() || s.kind != crate::sql::expr::SubqueryKind::Scalar {
                k = None;
            } else {
                k = k.map(|k| k.max(Known::Running));
            }
        }
        _ => {}
    });
    k
}

/// The clauses with comparison values known `by` then made constants (through `eval`), if
/// any was; None when the clauses have no such value.
pub fn with_known(
    quals: &[Expr],
    by: Known,
    eval: &mut dyn FnMut(&Expr) -> crate::error::Result<Value>,
) -> crate::error::Result<Option<Vec<Expr>>> {
    fn subst(
        e: &Expr,
        by: Known,
        eval: &mut dyn FnMut(&Expr) -> crate::error::Result<Value>,
        any: &mut bool,
    ) -> crate::error::Result<Expr> {
        let mut val = |x: &Expr, any: &mut bool| -> crate::error::Result<Expr> {
            match knowable(x) {
                Some(k) if k <= by => {
                    *any = true;
                    Ok(Expr::Const(eval(x)?, x.ty()))
                }
                _ => Ok(x.clone()),
            }
        };
        Ok(match e {
            Expr::And(items) => Expr::And(
                items
                    .iter()
                    .map(|i| subst(i, by, eval, any))
                    .collect::<crate::error::Result<_>>()?,
            ),
            Expr::Or(items) => Expr::Or(
                items
                    .iter()
                    .map(|i| subst(i, by, eval, any))
                    .collect::<crate::error::Result<_>>()?,
            ),
            Expr::Call(d, args, ty)
                if args.len() == 2 && matches!(d.name, "=" | "<>" | "<" | "<=" | ">" | ">=") =>
            {
                let mut args = vec![val(&args[0], any)?, val(&args[1], any)?];
                // (A key compared with a constant of the other time stamp type: the
                // comparison reads TimeZone, stable, so the executor prunes with it.)
                for (k, c) in [(0, 1), (1, 0)] {
                    if let Some(key_ty) = tz_cross(&args[k], &args[c]) {
                        *any = true;
                        if let Expr::Const(v, vt) = strip_const(&args[c])
                            && let Some(v) = tz_converted(v, vt, &key_ty, eval)
                        {
                            args[c] = Expr::Const(v, key_ty);
                        }
                    }
                }
                Expr::Call(d.clone(), args, ty.clone())
            }
            Expr::InList(x, list, neg) => {
                let list = list
                    .iter()
                    .map(|(v, op)| Ok((val(v, any)?, op.clone())))
                    .collect::<crate::error::Result<_>>()?;
                Expr::InList(x.clone(), list, *neg)
            }
            Expr::ArrayOp {
                left,
                right,
                op,
                all,
            } if !matches!(&**right, Expr::Cast(_, from, to, _) if from.is_array() && to.is_array() && from != to) => {
                let mut right = val(right, any)?;
                // (An array of the other time stamp type: its elements converted, as above.)
                if let Some(key_ty) = tz_cross(left, &right) {
                    *any = true;
                    if let Expr::Const(Value::Array(a), _) = strip_const(&right) {
                        let elem_ty = a.elem.clone();
                        let conv: Option<Vec<Value>> = a
                            .items
                            .iter()
                            .map(|v| match v {
                                Value::Null => Some(Value::Null),
                                v => tz_converted(v, &elem_ty, &key_ty, eval),
                            })
                            .collect();
                        if let Some(vals) = conv {
                            right = Expr::Const(
                                Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(
                                    key_ty.clone(),
                                    vals,
                                ))),
                                crate::types::DataType::array_of(key_ty),
                            );
                        }
                    }
                }
                Expr::ArrayOp {
                    left: left.clone(),
                    right: Box::new(right),
                    op: op.clone(),
                    all: *all,
                }
            }
            e => e.clone(),
        })
    }
    fn strip_const(e: &Expr) -> &Expr {
        match e {
            Expr::Relabel(x, _) | Expr::Collate(x, ..) => strip_const(x),
            e => e,
        }
    }
    /// The key's type when `key` reads columns of a time stamp type and `other` is a
    /// constant (an array of them) of the other one.
    fn tz_cross(key: &Expr, other: &Expr) -> Option<crate::types::DataType> {
        use crate::types::DataType as T;
        if key.columns().is_empty() || !matches!(strip_const(other), Expr::Const(..)) {
            return None;
        }
        let kt = key.ty();
        let ot = match strip_const(other).ty() {
            T::Array(e) => *e,
            t => t,
        };
        match (&kt, &ot) {
            (T::Timestamp(_), T::Timestamptz(_)) | (T::Timestamptz(_), T::Timestamp(_)) => Some(kt),
            _ => None,
        }
    }
    /// `v` (of type `from`) as a value of the other time stamp type `to`, when the
    /// conversion is exact there and an hour either side (no daylight-saving shift for the
    /// comparison to straddle); None otherwise (that clause then prunes nothing).
    fn tz_converted(
        v: &Value,
        from: &crate::types::DataType,
        to: &crate::types::DataType,
        eval: &mut dyn FnMut(&Expr) -> crate::error::Result<Value>,
    ) -> Option<Value> {
        const HOUR: i64 = 3_600_000_000;
        let raw = |v: &Value| match v {
            Value::Timestamp(t) | Value::Timestamptz(t) => Some(*t),
            _ => None,
        };
        let with = |v: &Value, t: i64| match v {
            Value::Timestamp(_) => Value::Timestamp(t),
            _ => Value::Timestamptz(t),
        };
        let mut cast = |v: Value, a: &crate::types::DataType, b: &crate::types::DataType| {
            eval(&Expr::Cast(Box::new(Expr::Const(v, a.clone())), a.clone(), b.clone(), false)).ok()
        };
        let x = raw(v)?;
        x.checked_sub(HOUR)?.checked_add(2 * HOUR)?;
        let out = cast(v.clone(), from, to)?;
        let y = raw(&out)?;
        for d in [-HOUR, 0, HOUR] {
            let back = cast(with(&out, y.checked_add(d)?), to, from)?;
            if raw(&back)? != x + d {
                return None;
            }
        }
        Some(out)
    }
    let mut any = false;
    let out = quals
        .iter()
        .map(|q| subst(q, by, eval, &mut any))
        .collect::<crate::error::Result<Vec<_>>>()?;
    Ok(any.then_some(out))
}

/// Run-time partition pruning of a member scan: whether the clauses, with the values known
/// `by` then, exclude it.
pub fn pruned_at(
    quals: &[Expr],
    levels: &[PartLevel],
    by: Known,
    eval: &mut dyn FnMut(&Expr) -> crate::error::Result<Value>,
) -> bool {
    match with_known(quals, by, eval) {
        Ok(Some(q)) => pruned(&q, levels),
        _ => false,
    }
}

/// Whether a key passes every level of a partition constraint.
pub fn check_levels(
    levels: &[PartLevel],
    key_of: &mut dyn FnMut(&[Expr]) -> Result<Vec<Value>>,
) -> Result<bool> {
    for l in levels {
        let key = key_of(&l.key)?;
        let _g = crate::catalog::partition::under(l.colls.clone());
        if !crate::catalog::partition::fits(&l.bound, &l.siblings, &key, &l.types) {
            return Ok(false);
        }
    }
    Ok(true)
}
