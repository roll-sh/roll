//! Partitionwise join (joinrels.c's try_partitionwise_join, allpaths.c's
//! generate_partitionwise_join_paths): a join group whose FROM items are all partitioned
//! the same way and joined on their partition keys is the Append of the joins of matching
//! partitions, each planned on its own. It competes with the plain join by cost.

use super::partmerge;
use super::*;
use crate::catalog::partition::{PartBound, bound_order};

/// One FROM item as partitions: per partition (in bound order) the member plan, or None
/// where plan-time pruning left the partition out.
struct Parted {
    strategy: crate::catalog::PartStrategy,
    types: Vec<DataType>,
    bounds: Vec<PartBound>,
    parts: Vec<Option<Plan>>,
    /// Per partition key column, the expressions over the group's row that are it (a
    /// joined item has one per partitioned table in it).
    keys: Vec<Vec<Expr>>,
    /// The partitioned tables' row types.
    rowtypes: Vec<u32>,
    /// The partition key's collations (bounds compare under them).
    colls: Vec<Option<Arc<crate::types::collation::Collation>>>,
}

/// A partitionwise join's Append, as a FROM item of a join above it is partitioned: its
/// scheme, the partition of each child join, the key expressions over its row.
#[derive(Clone)]
pub(in crate::sql::paths) struct PwJoin {
    strategy: crate::catalog::PartStrategy,
    types: Vec<DataType>,
    bounds: Vec<PartBound>,
    idx: Vec<usize>,
    keys: Vec<Vec<Expr>>,
    rowtypes: Vec<u32>,
    colls: Vec<Option<Arc<crate::types::collation::Collation>>>,
}

/// The leaves of the join tree at `p`, in `gather`'s order, replaced by `repl`'s plans.
fn substitute(p: &Plan, top: bool, repl: &mut std::vec::IntoIter<Plan>) -> Option<Plan> {
    if let Plan::Join {
        kind,
        left,
        right,
        cond,
        hash,
        params,
        nleft,
        nright,
        meta,
    } = p
        && params.is_empty()
        && (*kind == JoinKind::Inner || top)
    {
        let (l, r) = if *kind == JoinKind::Inner {
            (substitute(left, false, repl)?, substitute(right, false, repl)?)
        } else {
            (repl.next()?, repl.next()?)
        };
        return Some(Plan::Join {
            kind: *kind,
            left: Arc::new(l),
            right: Arc::new(r),
            cond: cond.clone(),
            hash: hash.clone(),
            params: params.clone(),
            nleft: *nleft,
            nright: *nright,
            meta: meta.clone(),
        });
    }
    repl.next()
}

/// `substitute` for a group with outer joins inside: its leaves as `outerjoins::walk`
/// finds them (through every join but a full one, and the filters over joins).
fn substitute_general(p: &Plan, repl: &mut std::vec::IntoIter<Plan>) -> Option<Plan> {
    match p {
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
        } if params.is_empty() && *kind != JoinKind::Full => {
            let l = substitute_general(left, repl)?;
            let r = substitute_general(right, repl)?;
            Some(Plan::Join {
                kind: *kind,
                left: Arc::new(l),
                right: Arc::new(r),
                cond: cond.clone(),
                hash: hash.clone(),
                params: Vec::new(),
                nleft: *nleft,
                nright: *nright,
                meta: meta.clone(),
            })
        }
        Plan::Filter { input, pred }
            if matches!(&**input, Plan::Join { kind, params, .. } if *kind != JoinKind::Full && params.is_empty())
                && super::outerjoins::absorbable(pred) =>
        {
            Some(Plan::Filter {
                input: Arc::new(substitute_general(input, repl)?),
                pred: pred.clone(),
            })
        }
        _ => repl.next(),
    }
}

/// A member of an inheritance Append as a join leaf: its scan, without a projection that
/// only passes the parent's columns through.
fn member_plan(p: &Arc<Plan>) -> Plan {
    if let Plan::Project { input, exprs } = &**p
        && let Plan::Scan(s) = &**input
        && exprs.len() == crate::sql::binder::plan_width(input)
        && exprs
            .iter()
            .enumerate()
            .all(|(i, e)| matches!(e, Expr::Col(j, _) if *j == i))
        && s.table.columns.len() == exprs.len()
    {
        return (**input).clone();
    }
    (**p).clone()
}

impl Search<'_, '_, '_> {
    /// The leaf as a partitioned table's partitions (see `Parted`).
    fn parted(&self, l: usize) -> Option<Parted> {
        let leaf = &self.leaves[l];
        // A partitionwise join below.
        if let Plan::Append { inputs, inh: None } = &leaf.plan
            && let Some(first) = inputs.first()
            && let Some(pw) = self.pl.pw_joins.borrow().iter().find(|(a, _)| *a == Arc::as_ptr(first) as usize).map(|(_, p)| p.clone())
            && pw.idx.len() == inputs.len()
        {
            let mut parts: Vec<Option<Plan>> = vec![None; pw.bounds.len()];
            for (i, inp) in pw.idx.iter().zip(inputs) {
                parts[*i] = Some((**inp).clone());
            }
            let keys = pw
                .keys
                .iter()
                .map(|alts| alts.iter().map(|e| e.clone().shift_cols(leaf.off as isize)).collect())
                .collect();
            return Some(Parted {
                strategy: pw.strategy,
                types: pw.types,
                bounds: pw.bounds,
                parts,
                keys,
                rowtypes: pw.rowtypes,
                colls: pw.colls,
            });
        }
        // A subquery's projection over the Append (its placeholders: constants and the
        // parent's columns) goes on each partition.
        let (proj, own);
        let (members, inputs) = match &leaf.plan {
            Plan::Append {
                inputs,
                inh: Some(_),
            } => {
                proj = None;
                (leaf.members.as_ref()?, inputs)
            }
            Plan::Project { input, exprs } => {
                let Plan::Append {
                    inputs,
                    inh: Some(_),
                } = &**input
                else {
                    return None;
                };
                if exprs.iter().any(|e| {
                    !matches!(e, Expr::Col(..) | Expr::Const(..)) || e.is_volatile()
                }) {
                    return None;
                }
                proj = Some(exprs);
                let need = vec![true; crate::sql::binder::plan_width(input)];
                own = self.pl.members(inputs, &need)?;
                (&own, inputs)
            }
            _ => return None,
        };
        let first = members.first()?;
        let level = first.scan.part.as_ref()?.first()?.clone();
        let parent = level.parent.clone();
        parent.partkey.as_ref()?;
        let mut bounds: Vec<PartBound> = level.siblings.clone();
        bounds.push(level.bound.clone());
        {
            let _g = crate::catalog::partition::under(level.colls.clone());
            bounds.sort_by(bound_order);
        }
        // The key over the leaf's (the parent's) row: the members pass the parent's
        // columns through in place.
        let mut keys: Vec<Vec<Expr>> = Vec::new();
        for k in &level.key {
            let mut ok = true;
            let mapped = k
                .clone()
                .transform(&mut |e| {
                    Ok(match e {
                        Expr::Col(c, t) => match first.map.iter().position(|m| *m == Some(c)).and_then(|pc| match proj {
                            None => Some(pc),
                            Some(ex) => ex.iter().position(|e| matches!(e, Expr::Col(j, _) if *j == pc)),
                        }) {
                            Some(pc) => Expr::Col(leaf.off + pc, t),
                            None => {
                                ok = false;
                                Expr::Col(c, t)
                            }
                        },
                        e => e,
                    })
                })
                .ok()?;
            if !ok {
                return None;
            }
            keys.push(vec![mapped]);
        }
        let mut parts: Vec<Option<Plan>> = vec![None; bounds.len()];
        for m in members {
            if m.map != first.map {
                return None;
            }
            let lv = m.scan.part.as_ref()?.first()?;
            if lv.parent.oid != parent.oid {
                return None;
            }
            let i = bounds.iter().position(|b| *b == lv.bound)?;
            if parts[i].is_some() {
                return None;
            }
            parts[i] = Some(match proj {
                None => member_plan(&inputs[m.input]),
                Some(ex) => Plan::Project {
                    input: inputs[m.input].clone(),
                    exprs: ex.clone(),
                },
            });
        }
        // Every input is a member (none is something else).
        if members.len() != inputs.iter().filter(|i| !matches!(&***i, Plan::Empty { .. })).count() {
            return None;
        }
        Some(Parted {
            rowtypes: vec![parent.rowtype],
            colls: level.colls.clone(),
            strategy: level.strategy,
            types: level.types.clone(),
            bounds,
            parts,
            keys,
        })
    }

    /// have_partkey_equi_join over the group: each partition key column of every leaf is
    /// equated with the others' (an equivalence class; an outer join's own clause).
    fn partkey_equi_join(&self, parted: &[Parted]) -> bool {
        let nkeys = parted[0].keys.len();
        let n = parted.len();
        for j in 0..nkeys {
            let keys: Vec<&Vec<Expr>> = parted.iter().map(|p| &p.keys[j]).collect();
            // Leaves joined on key j: by an equality clause between their keys (an outer
            // join's own), or (inner) by an equivalence class of columns.
            let mut joined = vec![vec![false; n]; n];
            for c in &self.clauses {
                if self.kind != JoinKind::Inner && !c.on {
                    continue;
                }
                let Some((x, y, _, _)) = &c.eq else { continue };
                if !(c.hashable || c.mergeable) {
                    continue;
                }
                // (Equal under a nondeterministic collation the partition key isn't under, or
                // the other way round: rows of different partitions may match.)
                let ccoll = match &c.expr {
                    Expr::Call(d, ..) => d.coll.clone(),
                    _ => None,
                };
                let kcoll = parted[0].colls.get(j).cloned().flatten();
                let nondet = |x: &Option<Arc<crate::types::collation::Collation>>| x.as_ref().is_some_and(|c| !c.deterministic);
                if (nondet(&ccoll) || nondet(&kcoll))
                    && ccoll.as_ref().map(|c| c.name.as_str()) != kcoll.as_ref().map(|c| c.name.as_str())
                {
                    continue;
                }
                let at = |e: &Expr| {
                    keys.iter()
                        .position(|alts| alts.iter().any(|k| key_operand(k).same_as(key_operand(e))))
                };
                if let (Some(a), Some(b)) = (at(x), at(y))
                    && a != b
                {
                    joined[a][b] = true;
                    joined[b][a] = true;
                }
            }
            if self.kind == JoinKind::Inner {
                for a in 0..n {
                    for b in 0..n {
                        for ka in keys[a] {
                            for kb in keys[b] {
                                if let (Some(x), Some(y)) = (key_col(ka), key_col(kb))
                                    && self.ec[x] == self.ec[y]
                                {
                                    joined[a][b] = true;
                                }
                            }
                        }
                    }
                }
            }
            // Connected.
            let mut seen = vec![false; n];
            let mut stack = vec![0];
            seen[0] = true;
            while let Some(a) = stack.pop() {
                for b in 0..n {
                    if joined[a][b] && !seen[b] {
                        seen[b] = true;
                        stack.push(b);
                    }
                }
            }
            if seen.iter().any(|s| !s) {
                return false;
            }
        }
        true
    }
}

impl Planner<'_, '_> {
    /// The group at `p` (whose search is `s`) joined partition by partition: the child
    /// joins' plans and their total cost and rows; None when the group can't be.
    pub(super) fn partitionwise(
        &self,
        p: &Plan,
        s: &Search,
        needed: &[bool],
    ) -> Option<(Vec<Plan>, f64, f64, PwJoin)> {
        {
            let st = self.ctx.sess.settings.borrow();
            if !st
                .get_raw("enable_partitionwise_join")
                .is_some_and(|v| v == "on" || v == "true")
            {
                return None;
            }
        }
        let parted: Vec<Parted> = (0..s.leaves.len())
            .map(|l| s.parted(l))
            .collect::<Option<_>>()?;
        if parted
            .iter()
            .any(|p| p.rowtypes.iter().any(|r| self.whole_rows.borrow().contains(r)))
        {
            return None;
        }
        let first = &parted[0];
        if parted.iter().any(|p| {
            p.strategy != first.strategy
                || p.types.len() != first.types.len()
                || p.types
                    .iter()
                    .zip(&first.types)
                    .any(|(a, b)| a.without_typmod() != b.without_typmod())
        }) {
            return None;
        }
        if !s.partkey_equi_join(&parted) {
            return None;
        }
        // Different bounds: matched partition by partition where each overlaps at most
        // one of the other side's (partition_bounds_merge).
        if parted.iter().any(|p| p.bounds != first.bounds) {
            return self.partitionwise_merged(p, s, needed, &parted);
        }
        let mut children = Vec::new();
        let mut idx = Vec::new();
        for i in 0..first.bounds.len() {
            let have: Vec<bool> = parted.iter().map(|p| p.parts[i].is_some()).collect();
            let skip = match s.kind {
                // (A group with outer joins inside: only partitions every side has.)
                _ if s.general => {
                    if have.iter().all(|h| !h) {
                        true
                    } else if have.iter().any(|h| !h) {
                        return None;
                    } else {
                        false
                    }
                }
                JoinKind::Inner | JoinKind::Semi => have.iter().any(|h| !h),
                JoinKind::Left | JoinKind::Anti => {
                    if !have[0] {
                        true
                    } else if !have[1] {
                        return None;
                    } else {
                        false
                    }
                }
                JoinKind::Right => {
                    if !have[1] {
                        true
                    } else if !have[0] {
                        return None;
                    } else {
                        false
                    }
                }
                JoinKind::Full => {
                    if !have[0] && !have[1] {
                        true
                    } else if !have[0] || !have[1] {
                        return None;
                    } else {
                        false
                    }
                }
            };
            if skip {
                continue;
            }
            // (Planned again in the child join: its own paths, parameterized ones too.)
            let plans: Vec<Plan> = parted.iter().map(|p| super::lift::unplanned(p.parts[i].clone().unwrap())).collect();
            let mut child = if s.general {
                substitute_general(p, &mut plans.into_iter())?
            } else {
                substitute(p, true, &mut plans.into_iter())?
            };
            self.absorbed.replace(s.extra.clone());
            self.pw_loop_rows.replace(s.leaves.iter().map(|l| l.rows).collect());
            let planned = self.plan_joins_with(&mut child, needed, None, &[]);
            self.pw_loop_rows.take();
            self.absorbed.take();
            planned?;
            children.push(child);
            idx.push(i);
        }
        if children.is_empty() {
            return None;
        }
        let mut total = 0.0;
        let mut rows = 0.0;
        for c in &children {
            let e = crate::sql::explain::estimate(&Arc::new(c.clone()), self.ctx);
            total += e.total;
            rows += e.rows;
        }
        // cost_append: half a tuple cost per row passed up.
        total += rows * CPU_TUPLE_COST * 0.5;
        // The join's partition key: each item's, as far as the join's row has it.
        let width = needed.len();
        let keys: Vec<Vec<Expr>> = (0..first.keys.len())
            .map(|j| {
                parted
                    .iter()
                    .flat_map(|p| p.keys[j].iter())
                    .filter(|e| e.columns().iter().all(|c| *c < width))
                    .cloned()
                    .collect()
            })
            .collect();
        let pw = PwJoin {
            strategy: first.strategy,
            types: first.types.clone(),
            bounds: first.bounds.clone(),
            idx,
            keys,
            rowtypes: parted.iter().flat_map(|p| p.rowtypes.iter().copied()).collect(),
            colls: first.colls.clone(),
        };
        Some((children, total, rows, pw))
    }
}

/// The row types of the whole-row references (`t.*` as a value) the plan reads: a
/// partitioned table referenced so isn't joined partitionwise
/// (set_base_rel_consider_partitionwise_join).
pub(in crate::sql::paths) fn whole_rows(p: &Plan, out: &mut Vec<u32>) {
    let mut exprs: Vec<&Expr> = Vec::new();
    match p {
        Plan::Project { exprs: e, .. } | Plan::ProjectSet { exprs: e, .. } => exprs.extend(e),
        Plan::Filter { pred, .. } => exprs.push(pred),
        Plan::Scan(s) => exprs.extend(s.filter.iter()),
        Plan::Join { cond, hash, .. } => {
            exprs.extend(cond.iter());
            if let Some((l, r)) = hash {
                exprs.extend(l);
                exprs.extend(r);
            }
        }
        Plan::Sort { keys, .. } => exprs.extend(keys.iter().map(|k| &k.expr)),
        Plan::Agg { groups, aggs, .. } => {
            exprs.extend(groups);
            for a in aggs {
                exprs.extend(&a.args);
            }
        }
        _ => {}
    }
    for e in exprs {
        e.walk(&mut |x| {
            if let Expr::Row(args, DataType::Row(rt)) = x
                && rt.oid != 0
                && args.iter().all(|a| matches!(a, Expr::Col(..)))
                && !out.contains(&rt.oid)
            {
                out.push(rt.oid);
            }
        });
    }
    let kids: Vec<&Arc<Plan>> = match p {
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Agg { input, .. }
        | Plan::Window { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => vec![input],
        Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => vec![left, right],
        Plan::Append { inputs, .. } => inputs.iter().collect(),
        Plan::With { ctes, body, .. } => ctes.iter().map(|(_, c)| c).chain([body]).collect(),
        _ => Vec::new(),
    };
    for k in kids {
        whole_rows(k, out);
    }
}

impl Planner<'_, '_> {
    /// `partitionwise` for items partitioned with different bounds: their partitions
    /// matched (partmerge), a side's missing partition an empty relation.
    fn partitionwise_merged(
        &self,
        p: &Plan,
        s: &Search,
        needed: &[bool],
        parted: &[Parted],
    ) -> Option<(Vec<Plan>, f64, f64, PwJoin)> {
        let n = parted.len();
        let first = &parted[0];
        let live = |pt: &Parted| -> Vec<bool> {
            pt.parts
                .iter()
                .map(|x| x.as_ref().is_some_and(|pl| !matches!(pl, Plan::Empty { .. })))
                .collect()
        };
        let _g = crate::catalog::partition::under(first.colls.clone());
        // Folded pairwise in the items' order (an inner join's; an outer join of two
        // items is its own, its outer side first; in a group with outer joins inside, the
        // join that brings each item in).
        let (mut slots, mut bounds): (Vec<Vec<Option<usize>>>, Vec<PartBound>) = (
            (0..first.bounds.len()).map(|i| vec![Some(i)]).collect(),
            first.bounds.clone(),
        );
        let mut acc_live = live(first);
        // Each item's join to the ones before it (its kind; whether it is the outer side).
        let mut kinds: Vec<(Jt, bool)> = vec![(Jt::Inner, false)];
        for (k, other) in parted.iter().enumerate().skip(1) {
            let (kind, swap) = if s.general {
                let bit = 1u32 << k;
                let acc = bit - 1;
                let mut kind = (Jt::Inner, false);
                for sj in &s.sjs {
                    let j = match sj.kind {
                        JoinKind::Left => Jt::Left,
                        JoinKind::Semi => Jt::Semi,
                        JoinKind::Anti => Jt::Anti,
                        _ => continue,
                    };
                    if sj.syn_r & bit != 0 && sj.syn_l & acc == sj.syn_l {
                        kind = (j, false);
                    } else if j == Jt::Left && sj.syn_l & bit != 0 && sj.syn_r & acc == sj.syn_r {
                        kind = (Jt::Left, true);
                    }
                }
                kind
            } else {
                match s.kind {
                    _ if n > 2 => (Jt::Inner, false),
                    JoinKind::Inner => (Jt::Inner, false),
                    JoinKind::Left => (Jt::Left, false),
                    JoinKind::Right => (Jt::Left, true),
                    JoinKind::Full => (Jt::Full, false),
                    JoinKind::Semi => (Jt::Semi, false),
                    JoinKind::Anti => (Jt::Anti, false),
                }
            };
            let acc = partmerge::Side {
                bounds: &bounds,
                live: acc_live.clone(),
            };
            let oth = partmerge::Side {
                bounds: &other.bounds,
                live: live(other),
            };
            let merged = if swap {
                partmerge::merge(first.strategy, &oth, &acc, kind)
                    .map(|(ps, b)| (ps.into_iter().map(|(a, b)| (b, a)).collect::<Vec<_>>(), b))
            } else {
                partmerge::merge(first.strategy, &acc, &oth, kind)
            };
            let (ps, nb) = merged?;
            kinds.push((kind, swap));
            let mut nslots = Vec::new();
            for (a, b) in ps {
                let mut row = match a {
                    Some(a) => slots[a].clone(),
                    None => vec![None; k],
                };
                row.push(b);
                nslots.push(row);
            }
            slots = nslots;
            bounds = nb;
            acc_live = vec![true; bounds.len()];
        }
        let mut children = Vec::new();
        let mut idx = Vec::new();
        for (i, row) in slots.iter().enumerate() {
            // try_partitionwise_join: a pair with no partition on a side (merged with a
            // dummy, or pruned) is left out when the join can produce no rows from it, else
            // the join can't be done partitionwise.
            let missing = |k: usize| row[k].is_none_or(|j| parted[k].parts[j].is_none());
            let mut skip = false;
            let mut reject = false;
            if kinds.iter().any(|k| k.0 == Jt::Full) {
                if (0..row.len()).all(missing) {
                    skip = true;
                } else if (0..row.len()).any(missing) {
                    reject = true;
                }
            } else {
                for k in 0..row.len() {
                    if !missing(k) {
                        continue;
                    }
                    let (kind, swap) = kinds[k];
                    match kind {
                        // (The first item, or one whose rows an inner or semi join needs.)
                        _ if k == 0 => {
                            if kinds.iter().skip(1).all(|x| !x.1) {
                                skip = true;
                            } else {
                                reject = true;
                            }
                        }
                        Jt::Inner | Jt::Semi => skip = true,
                        Jt::Left if swap => skip = true,
                        _ => reject = true,
                    }
                }
            }
            if skip {
                continue;
            }
            if reject {
                return None;
            }
            let mut plans = Vec::new();
            for (k, slot) in row.iter().enumerate() {
                let leaf = &s.leaves[k];
                plans.push(match slot.and_then(|j| parted[k].parts[j].clone()) {
                    Some(pl) => super::lift::unplanned(pl),
                    None => Plan::Empty {
                        ncols: leaf.ncols,
                        cols: None,
                    },
                });
            }
            let mut child = if s.general {
                substitute_general(p, &mut plans.into_iter())?
            } else {
                substitute(p, true, &mut plans.into_iter())?
            };
            self.absorbed.replace(s.extra.clone());
            self.pw_loop_rows.replace(s.leaves.iter().map(|l| l.rows).collect());
            let planned = self.plan_joins_with(&mut child, needed, None, &[]);
            self.pw_loop_rows.take();
            self.absorbed.take();
            planned?;
            children.push(child);
            idx.push(i);
        }
        if children.is_empty() {
            return None;
        }
        let mut total = 0.0;
        let mut rows = 0.0;
        for c in &children {
            let e = crate::sql::explain::estimate(&Arc::new(c.clone()), self.ctx);
            total += e.total;
            rows += e.rows;
        }
        total += rows * CPU_TUPLE_COST * 0.5;
        let width = needed.len();
        let keys: Vec<Vec<Expr>> = (0..first.keys.len())
            .map(|j| {
                parted
                    .iter()
                    .flat_map(|p| p.keys[j].iter())
                    .filter(|e| e.columns().iter().all(|c| *c < width))
                    .cloned()
                    .collect()
            })
            .collect();
        let pw = PwJoin {
            strategy: first.strategy,
            types: first.types.clone(),
            bounds,
            idx,
            keys,
            rowtypes: parted.iter().flat_map(|p| p.rowtypes.iter().copied()).collect(),
            colls: first.colls.clone(),
        };
        Some((children, total, rows, pw))
    }
}
