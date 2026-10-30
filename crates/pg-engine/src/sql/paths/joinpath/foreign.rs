//! Foreign joins in the join search (add_paths_to_joinrel's GetForeignJoinPaths): a join of
//! relations of one foreign server whose wrapper can run it remotely gets a path of its
//! own, a Foreign Scan of the join, costed by the wrapper and kept by add_path like any.
//! The wrapper decides once per join relation, from the first pair of inputs it is
//! offered (`joinrel->fdw_private`).

use super::*;
use crate::ext::postgres_fdw::join as pj;
use std::sync::Arc as SArc;

/// postgres_fdw's DEFAULT_FDW_SORT_MULTIPLIER: a sort sent without remote estimates.
pub(in crate::sql::paths) const DEFAULT_FDW_SORT_MULTIPLIER: f64 = 1.2;

impl Search<'_, '_, '_> {
    /// A base relation's description by its wrapper (GetForeignRelSize's fpinfo): a scan
    /// of a foreign table whose wrapper pushes joins down, read plainly.
    pub(super) fn fdw_base(&self, l: usize) -> Option<pj::RelInfo> {
        if self.fdw_list.borrow().len() >= 64 {
            return None;
        }
        let leaf = &self.leaves[l];
        let (s, r) = leaf.scan.as_ref()?;
        if leaf.sys.is_some()
            || leaf.lateral.is_some()
            || leaf.proj.is_some()
            || s.with_key
            || s.sys
            || s.mark.is_some()
            || s.rtindex.0 == 0
            || s.current_of.is_some()
        {
            return None;
        }
        crate::ext::fdw::wrapper_of(&self.pl.cat, &s.table)?;
        let spec = crate::ext::postgres_fdw::scan::spec(s, None, &self.pl.cat, self.pl.ctx.fmt());
        let off = leaf.off;
        let shift = |e: &Expr| e.clone().remap_cols(&|i| i + off);
        let set = 1u32 << l;
        let reltarget: Vec<usize> = (0..leaf.ncols).map(|k| off + k).filter(|&g| self.needed_above(g, set)).collect();
        let local_sel = if spec.local.is_empty() { 1.0 } else { self.pl.sel(r, &spec.local) };
        let base = self.fdw_list.borrow().len();
        let mut info = pj::base_info(
            &self.pl.cat,
            &pj::BaseIn {
                table: &s.table,
                base,
                rtindex: s.rtindex.0,
                user: s.check_as,
                remote_conds: spec.remote.iter().map(shift).collect(),
                local_conds: spec.local.iter().map(shift).collect(),
                reltarget,
                rows: r.rows,
                tuples: r.size.tuples,
                pages: r.size.pages,
                width: self.width_of(set),
                qual: (r.qual_startup, r.qual_cost),
                local_sel,
                seq_page_cost: self.pl.g.seq_page_cost,
            },
        )?;
        // (use_remote_estimate: the remote EXPLAIN's estimates of the scan's query.)
        if info.opts.use_remote_estimate {
            let need: Vec<bool> = (0..leaf.ncols).map(|k| self.needed_above(off + k, set)).collect();
            let q = crate::ext::postgres_fdw::scan::spec_with(s, Some(&need), &self.pl.cat, self.pl.ctx.fmt(), true);
            if let Some((rows, width, st, tot)) = pj::explain_estimate(self.pl.ctx, s, &format!("EXPLAIN {}", q.sql)) {
                let (lst, lpt) = self.pl.rel_quals_cost(r, &spec.local);
                let retrieved = rows;
                pj::finish(&mut info, costs::clamp_rows(rows * local_sel), width, retrieved, st + lst, tot + lpt * retrieved);
            }
        }
        self.fdw_list.borrow_mut().push(pj::Base::at(s.clone(), off, leaf.ncols, r.size.tuples));
        Some(info)
    }

    /// A FROM item planned on its own whose plan is a foreign join (an outer join's side
    /// planned as a group of its own): the wrapper's description of it, its base relations
    /// taken in, the join's columns the item doesn't return given ids of their own.
    pub(super) fn fdw_adopt(&self, l: usize) -> Option<pj::RelInfo> {
        let leaf = &self.leaves[l];
        // (Under the projection putting its columns in the item's order, unread ones NULL.)
        let (fr, proj): (&crate::sql::plan::ForeignRel, Option<&Vec<Expr>>) = match &leaf.plan {
            Plan::Foreign(fr) => (fr, None),
            Plan::Project { input, exprs } => match &**input {
                Plan::Foreign(fr) if exprs.iter().all(|e| matches!(e, Expr::Col(..) | Expr::Const(..))) => (fr, Some(exprs)),
                _ => return None,
            },
            _ => return None,
        };
        let js = fr.private.0.as_any().downcast_ref::<crate::ext::postgres_fdw::joinscan::JoinScan>()?;
        let width = proj.map_or(fr.out.len(), |e| e.len());
        if !js.order.is_empty() || js.limit.is_some() || width != leaf.ncols || leaf.lateral.is_some() {
            return None;
        }
        // (The item's column k: the join's column out[j] its projection reads.)
        let item_col = |g: usize| -> Option<usize> {
            let j = fr.out.iter().position(|&x| x == g)?;
            match proj {
                None => Some(j),
                Some(e) => e.iter().position(|x| matches!(x, Expr::Col(i, _) if *i == j)),
            }
        };
        let mut list = self.fdw_list.borrow_mut();
        if list.len() + js.bases.len() > 64 {
            return None;
        }
        let fresh = pj::HIDDEN_COLS + (l << 20);
        let map = |g: usize| item_col(g).map_or(fresh + g, |k| leaf.off + k);
        let first = list.len();
        for b in &js.bases {
            list.push(pj::Base { scan: b.scan.clone(), cols: b.cols.iter().map(|c| c.map(map)).collect(), tuples: b.tuples });
        }
        Some(js.info.map_cols(&map).renumber(&|k| first + k))
    }

    /// GetForeignJoinPaths for the join relation `ri` from `outer` and `inner`.
    pub(super) fn foreign_join_path(&mut self, ri: usize, outer: usize, inner: usize, jt: Jt, uniq: Uniq, restrict: &[usize]) {
        let set = self.rels[ri].set;
        if self.fdw_tried.contains(&set) {
            return;
        }
        self.fdw_tried.push(set);
        let (oset, iset) = (self.rels[outer].set, self.rels[inner].set);
        let (Some(o), Some(i)) = (self.fdw_rel(oset), self.fdw_rel(iset)) else { return };
        let kind = match (jt, uniq) {
            (Jt::Inner, Uniq::No) => JoinKind::Inner,
            (Jt::Left, Uniq::No) => JoinKind::Left,
            (Jt::Full, Uniq::No) => JoinKind::Full,
            (Jt::Semi, Uniq::No) => JoinKind::Semi,
            _ => return,
        };
        // (Not with lateral references: those need parameterized paths.)
        if self.lateral_relids(set) != 0 {
            return;
        }
        // (A PlaceHolderVar evaluated inside the join, below its top: deparsing can't
        // compute it.)
        if self.phvs.iter().any(|p| p.eval_at & !set == 0 && p.eval_at != set) {
            return;
        }
        // (semijoin_target_ok: what comes out of a semi join is its outer side's only.)
        if kind == JoinKind::Semi
            && (0..self.leaf_of.len()).any(|g| iset >> self.leaf_of[g] & 1 == 1 && self.needed_above(g, set))
        {
            return;
        }
        let (Some(bases), Some(cat)) = (self.fdw_bases(set), Some(self.pl.cat.clone())) else { return };
        // (Each relation named apart in the remote query: `r<range table index>`.)
        let rels = o.relids | i.relids;
        let idx: Vec<u32> = (0..bases.len()).filter(|k| rels >> k & 1 == 1).map(|k| bases[k].scan.rtindex.0).collect();
        if idx.iter().enumerate().any(|(k, &x)| x == 0 || idx[..k].contains(&x)) {
            return;
        }
        let fmt = self.pl.ctx.fmt().clone();
        let mut order: Vec<usize> = restrict.to_vec();
        // (build_joinrel_restrictlist: the equivalence classes' clauses first.)
        order.sort_by_key(|&c| !(self.clauses[c].ecable && !self.clauses[c].on || self.clauses[c].derived));
        // (RINFO_IS_PUSHED_DOWN: a clause over the join from above it, not an outer join's
        // own; the join's Filter when the join runs here.)
        let pushed = |c: usize| {
            let cl = &self.clauses[c];
            self.general
                && jt != Jt::Inner
                && !(self.semi_classes() && jt == Jt::Semi)
                && (cl.sj.is_none() || (cl.req | cl.leaves) & !set != 0)
        };
        let has_phv = |e: &Expr| e.columns().iter().any(|&g| self.phv(g).is_some());
        let scope = pj::RelInfo { relids: o.relids | i.relids, kind: pj::Kind::Base(0), ..(*o).clone() };
        let rows = self.rels[ri].rows;
        let width = self.rels[ri].width;
        let reltarget = self.fdw_reltarget(set);
        let relation_index = self.fdw_rt_len() + (ri + 1 - self.leaves.len()) as u32;
        let info = {
            let q = crate::ext::postgres_fdw::joinsql::Sql::new(&cat, &fmt, &bases);
            let ship = |e: &Expr| !has_phv(e) && q.shippable(&scope, e);
            let jin = pj::JoinIn {
                jt: kind,
                outer: &o,
                inner: &i,
                restrict: order.iter().map(|&c| (self.clauses[c].expr.clone(), pushed(c))).collect(),
                ship: &ship,
                reltarget,
                relation_index,
            };
            pj::join_ok(&jin)
        };
        let Some(mut info) = info else { return };
        // (Selectivities and costs of the clause lists, by the planner's estimates.)
        let sel_of = |e: &Expr| -> f64 {
            order.iter().find(|&&c| self.clauses[c].expr.same_as(e)).map_or(1.0, |&c| self.clause_sel(&self.clauses[c], None))
        };
        let (joinclauses, side_sel) = match &info.kind {
            pj::Kind::Join { joinclauses, .. } => {
                // (Restrictions of a side pulled into the join clauses: their selectivity.)
                let pulled = |side: &pj::RelInfo| !side.remote_conds.is_empty() && side.remote_conds.iter().all(|c| joinclauses.iter().any(|j| j.same_as(c)));
                let base_sel = |side: &pj::RelInfo| if side.is_join() { 1.0 } else { side.rows / self.fdw_tuples(side).max(1.0) };
                let s = [&*o, &*i].iter().filter(|s| pulled(s)).map(|s| base_sel(s)).product::<f64>();
                (joinclauses.clone(), s)
            }
            _ => return,
        };
        let cost = |es: &[Expr]| (0.0, es.iter().map(costs::qual_cost).sum::<f64>() * costs::CPU_OPERATOR_COST);
        let cin = pj::JoinCostIn {
            rows,
            width,
            joinclause_sel: joinclauses.iter().map(sel_of).product::<f64>() * side_sel,
            local_sel: info.local_conds.iter().map(sel_of).product::<f64>(),
            join_cost: cost(&joinclauses),
            remote_cost: cost(&info.remote_conds),
            local_cost: cost(&info.local_conds),
        };
        pj::estimate_join(&mut info, &cin);
        if info.opts.use_remote_estimate {
            let tlist: Vec<usize> = info.reltarget.clone();
            pj::remote_estimate(self.pl.ctx, &mut info, &bases, &tlist, cin.local_sel, cin.local_cost);
            self.rels[ri].rows = info.rows;
        }
        let info = SArc::new(info);
        self.fdw_rels.push((set, info.clone()));
        let jp = JP {
            node: JNode::Foreign { info: info.clone() },
            rows: info.rows,
            startup: info.startup,
            total: info.total,
            width,
            disabled: 0,
            keys: Vec::new(),
            req: 0,
            set,
        };
        self.add_join_path(ri, jp);
    }

    /// postgresGetForeignPaths' parameterized paths of a relation estimated remotely:
    /// the join clauses its equivalence classes imply are made then, its member first
    /// (generate_implied_equalities_for_column), which fixes their order.
    pub(super) fn fdw_param_clauses(&self, l: usize) {
        let me = 1u32 << l;
        for c in &self.clauses {
            if !c.ecable || c.on && !(self.semi_classes() && c.sj.is_some()) {
                continue;
            }
            let Some((a, b, al, bl)) = &c.eq else { continue };
            let (Some(x), Some(y)) = (self.mem_id(a), self.mem_id(b)) else { continue };
            if *al == me && *bl & me == 0 {
                self.note_derived(x, y, true);
            } else if *bl == me && *al & me == 0 {
                self.note_derived(y, x, true);
            }
        }
    }

    /// postgresGetForeignPaths' parameterized paths (use_remote_estimate only): one for each
    /// set of other relations a movable, shippable join clause reads, over the join clauses
    /// movable into the relation given them (get_baserel_parampathinfo), costed by the
    /// remote EXPLAIN of the scan with them (estimate_path_cost_size).
    pub(super) fn fdw_param_paths(&self, l: usize, r: &Rc<Rel>, width: u32) -> Vec<JP> {
        let leaf = &self.leaves[l];
        let Some((s, _)) = leaf.scan.as_ref() else { return Vec::new() };
        let (set, off) = (1u32 << l, leaf.off);
        let lat = if self.has_lat(l) { self.lateral_closure(l) } else { 0 };
        let cat = self.pl.cat.clone();
        let fmt = self.pl.ctx.fmt().clone();
        let d = crate::ext::postgres_fdw::deparse::Deparser::new(&cat, &s.table, &fmt);
        // (A clause over this relation's columns, the others' values parameters.)
        let as_param = |e: &Expr| {
            e.clone().transform(&mut |x| {
                Ok(match x {
                    Expr::Col(g, t) if self.leaf_of.get(g) == Some(&l) => Expr::Col(g - off, t),
                    Expr::Col(_, t) => Expr::Outer(1, 0, t),
                    x => x,
                })
            })
        };
        let mut reqs: Vec<u32> = Vec::new();
        for (ci, c) in self.clauses.iter().enumerate() {
            if c.leaves & set == 0 || c.leaves == set || c.expr.is_volatile() || !self.movable_to(ci, l) {
                continue;
            }
            if !as_param(&c.expr).is_ok_and(|e| d.shippable(&e)) {
                continue;
            }
            let req = (c.leaves & !set) | lat;
            if !reqs.contains(&req) {
                reqs.push(req);
            }
        }
        let opts = pj::opts(&cat, &s.table);
        let need: Vec<bool> = (0..leaf.ncols).map(|k| self.needed_above(off + k, set)).collect();
        let mut out = Vec::new();
        for req in reqs {
            let Some((mut prel, params, enforced)) = self.lateral_param_rel(l, r, req) else { continue };
            self.orient_param_quals(&mut prel.quals[r.quals.len()..], &enforced, set);
            let mut scan = s.clone();
            scan.filter = match prel.quals.len() {
                0 => None,
                1 => Some(prel.quals[0].clone()),
                _ => Some(Expr::And(prel.quals.clone())),
            };
            scan.written = prel.quals.clone();
            let q = crate::ext::postgres_fdw::scan::spec_with(&scan, Some(&need), &cat, &fmt, true);
            let Some((retrieved, _, st, tot)) = pj::explain_estimate(self.pl.ctx, &scan, &format!("EXPLAIN {}", q.sql)) else {
                continue;
            };
            let local_sel = if q.local.is_empty() { 1.0 } else { self.pl.sel(&prel, &q.local) };
            let (lst, lpt) = self.pl.rel_quals_cost(&prel, &q.local);
            let rows = costs::clamp_rows(retrieved * local_sel);
            let startup = st + lst + opts.startup_cost;
            let total = tot + lpt * retrieved + opts.startup_cost + (opts.tuple_cost + costs::CPU_TUPLE_COST) * retrieved;
            prel.rows = rows;
            out.push(JP {
                rows,
                startup,
                total,
                width,
                disabled: 0,
                keys: Vec::new(),
                req,
                set,
                node: JNode::Scan {
                    leaf: l,
                    path: Path { kind: PathKind::Seq, startup, total, rows, keys: Vec::new(), disabled: 0 },
                    params,
                    enforced,
                    rel: Rc::new(prel),
                },
            });
        }
        out
    }

    /// Equivalence classes' equalities among a parameterized path's clauses in the order
    /// their first making gave them (`note_derived` by a remote estimate's
    /// generate_implied_equalities_for_column), else as the clause has its members:
    /// `quals[i]` enforces `enforced[i]`, its outer side the parameters, of leaf set `set`.
    fn orient_param_quals(&self, quals: &mut [Expr], enforced: &[usize], set: u32) {
        let derives = self.derives.borrow();
        for (q, &ci) in quals.iter_mut().zip(enforced) {
            let Some((a, b, al, _)) = &self.clauses[ci].eq else { continue };
            // (Between plain columns only: an expression member's clause stays as it is.)
            let plain = |e: &Expr| key_col(e).is_some();
            if !plain(a) || !plain(b) {
                continue;
            }
            let (Some(x), Some(y)) = (self.mem_id(a), self.mem_id(b)) else { continue };
            let a_first = !derives.iter().any(|&(p, r, apply)| apply && p == y && r == x);
            let mine_first = a_first == (*al == set);
            if let Expr::Call(def, args, _) = q
                && def.name == "="
                && args.len() == 2
                && args[0].ty().without_typmod() == args[1].ty().without_typmod()
                && args[0].has_outer_refs() == mine_first
            {
                args.swap(0, 1);
            }
        }
    }

    /// add_paths_with_pathkeys_for_rel: the relation's scan with the remote server sorting,
    /// for each useful ordering (get_useful_pathkeys_for_relation: the query's ORDER BY when
    /// it is all this relation's; with use_remote_estimate also each column whose
    /// equivalence class joins another relation, ascending), costed by estimate_path_cost_size
    /// (the unsorted estimate times DEFAULT_FDW_SORT_MULTIPLIER, or the remote EXPLAIN).
    pub(super) fn fdw_sorted_paths(&self, l: usize, r: &Rc<Rel>, width: u32, f: &pj::RelInfo, want: &Want, base: &Path) -> Vec<JP> {
        let leaf = &self.leaves[l];
        let Some((s, _)) = leaf.scan.as_ref() else { return Vec::new() };
        let (set, off) = (1u32 << l, leaf.off);
        let cat = self.pl.cat.clone();
        let fmt = self.pl.ctx.fmt().clone();
        let d = crate::ext::postgres_fdw::deparse::Deparser::new(&cat, &s.table, &fmt);
        let shippable = |keys: &[PathKey]| keys.iter().all(|k| d.shippable(&k.expr));
        let mut useful: Vec<Vec<PathKey>> = Vec::new();
        let nq = self.query_keys().len();
        if nq > 0 && want.keys.len() == nq && shippable(&want.keys) {
            useful.push(want.keys.clone());
        }
        if f.opts.use_remote_estimate {
            for e in &want.merge {
                let key = PathKey { expr: e.clone(), desc: false, nulls_first: false, cmp: 0, coll: 0 };
                let Expr::Col(k, _) = e else { continue };
                // (The query's own single key is taken already.)
                if nq == 1 && useful.first().is_some_and(|u| u[0].expr.same_as(e)) || !shippable(std::slice::from_ref(&key)) {
                    continue;
                }
                let ec = self.ec[off + k];
                if useful.iter().any(|u| u.len() == 1 && matches!(u[0].expr, Expr::Col(j, _) if self.ec[off + j] == ec)) {
                    continue;
                }
                useful.push(vec![key]);
            }
        }
        let need: Vec<bool> = (0..leaf.ncols).map(|k| self.needed_above(off + k, set)).collect();
        let mut out = Vec::new();
        for keys in useful {
            let (startup, total, rows) = if f.opts.use_remote_estimate {
                let mut scan = s.clone();
                let order = keys.iter().map(|k| crate::sql::plan::SortKey { expr: k.expr.clone(), desc: k.desc, nulls_first: k.nulls_first, using: None }).collect();
                scan.fdw = Some(SArc::new(crate::sql::plan::FdwPushdown { order, ..scan.fdw.as_deref().cloned().unwrap_or_default() }));
                let q = crate::ext::postgres_fdw::scan::spec_with(&scan, Some(&need), &cat, &fmt, true);
                let Some((retrieved, _, st, tot)) = pj::explain_estimate(self.pl.ctx, s, &format!("EXPLAIN {}", q.sql)) else { continue };
                let (lst, lpt) = self.pl.rel_quals_cost(r, &q.local);
                let st = st + lst + f.opts.startup_cost;
                let tot = tot + lpt * retrieved + f.opts.startup_cost + (f.opts.tuple_cost + costs::CPU_TUPLE_COST) * retrieved;
                (st, tot, f.rows)
            } else {
                let st = f.rel_startup * DEFAULT_FDW_SORT_MULTIPLIER;
                let tot = f.rel_total * DEFAULT_FDW_SORT_MULTIPLIER;
                let fdw = f.opts.startup_cost;
                (st + fdw, tot + fdw + (f.opts.tuple_cost + costs::CPU_TUPLE_COST) * f.retrieved_rows, f.rows)
            };
            let gk = self.gkeys(l, &keys);
            let path = Path { kind: PathKind::Sort(Box::new(base.clone())), startup, total, rows, keys, disabled: 0 };
            out.push(JP {
                rows,
                startup,
                total,
                width,
                disabled: 0,
                keys: gk,
                req: 0,
                set,
                node: JNode::Scan { leaf: l, path, params: Vec::new(), enforced: Vec::new(), rel: r.clone() },
            });
        }
        out
    }

    /// A foreign scan sorted by its server (`fdw_sorted_paths`): the ORDER BY goes with its
    /// remote query.
    #[inline(never)]
    pub(super) fn fdw_presorted(&self, s: &mut crate::sql::plan::ScanPlan, path: &Path) {
        if s.fdw.is_some() || !crate::ext::postgres_fdw::scan::is_postgres_fdw(&self.pl.cat, &s.table) {
            return;
        }
        let order = path
            .keys
            .iter()
            .map(|k| crate::sql::plan::SortKey { expr: k.expr.clone(), desc: k.desc, nulls_first: k.nulls_first, using: None })
            .collect();
        s.fdw = Some(SArc::new(crate::sql::plan::FdwPushdown { order, ..Default::default() }));
    }

    /// `e` as the search shows its clause now (an equivalence class's equality in the
    /// order its first making gave it, `orient_derived`).
    fn reoriented(&self, e: &Expr) -> Expr {
        if let Expr::Call(_, args, _) = e
            && args.len() == 2
            && let Some(c) = self.clauses.iter().find(|c| match &c.expr {
                Expr::Call(_, b, _) if b.len() == 2 => b[0].same_as(&args[1]) && b[1].same_as(&args[0]),
                _ => false,
            })
        {
            return c.expr.clone();
        }
        e.clone()
    }

    /// The relation's description with its clauses as the search shows them.
    fn reoriented_info(&self, r: &pj::RelInfo) -> pj::RelInfo {
        let f = |v: &Vec<Expr>| v.iter().map(|e| self.reoriented(e)).collect::<Vec<_>>();
        let kind = match &r.kind {
            pj::Kind::Join { jt, outer, inner, joinclauses, make_outer_subquery, make_inner_subquery } => pj::Kind::Join {
                jt: *jt,
                outer: SArc::new(self.reoriented_info(outer)),
                inner: SArc::new(self.reoriented_info(inner)),
                joinclauses: f(joinclauses),
                make_outer_subquery: *make_outer_subquery,
                make_inner_subquery: *make_inner_subquery,
            },
            k => k.clone(),
        };
        pj::RelInfo { kind, remote_conds: f(&r.remote_conds), local_conds: f(&r.local_conds), ..r.clone() }
    }

    /// The wrapper's description of relation `set`, if it can run remotely.
    fn fdw_rel(&self, set: u32) -> Option<SArc<pj::RelInfo>> {
        self.fdw_rels.iter().find(|(s, _)| *s == set).map(|(_, r)| r.clone())
    }

    /// The base relations of the group as a foreign join sees them (every leaf a pushdown
    /// scan, else None for the leaves that aren't: only those in `set` must be).
    fn fdw_bases(&self, _set: u32) -> Option<Vec<pj::Base>> {
        Some(self.fdw_list.borrow().clone())
    }

    /// A base relation's tuples (for its restrictions' selectivity).
    fn fdw_tuples(&self, r: &pj::RelInfo) -> f64 {
        match r.kind {
            pj::Kind::Base(k) => self.fdw_list.borrow().get(k).map_or(1.0, |b| b.tuples),
            _ => 1.0,
        }
    }

    /// The join relation's reltarget: its columns needed above it, its relations' in the
    /// join relation's order (build_joinrel_tlist).
    fn fdw_reltarget(&self, set: u32) -> Vec<usize> {
        let tl = self.rel_index(set).map(|i| self.rels[i].tl.clone()).unwrap_or_default();
        let mut out = Vec::new();
        for l in tl {
            let leaf = &self.leaves[l];
            out.extend((leaf.off..leaf.off + leaf.ncols).filter(|&g| self.needed_above(g, set)));
        }
        out
    }

    /// The level's range table length (relation_index of join relations counts on from it):
    /// its relations' highest index.
    fn fdw_rt_len(&self) -> u32 {
        self.leaves.iter().filter_map(|l| l.scan.as_ref().map(|s| s.0.rtindex.0)).max().unwrap_or(0)
    }

    /// The whole group's foreign join path, when it has one, as a plan returning the
    /// columns `layout` (NULL for those it doesn't read).
    pub(super) fn fdw_alt(&self, layout: &[usize]) -> Option<Plan> {
        let full = (1u32 << self.leaves.len()) - 1;
        let ri = self.rel_index(full)?;
        let jp = self.rels[ri].paths.iter().chain(&self.rels[ri].param).find(|p| matches!(p.node, JNode::Foreign { .. }));
        let info = self.fdw_rel(full)?;
        let est = jp.cloned().unwrap_or(JP {
            node: JNode::Foreign { info: info.clone() },
            rows: info.rows,
            startup: info.startup,
            total: info.total,
            width: self.rels[ri].width,
            disabled: 0,
            keys: Vec::new(),
            req: 0,
            set: full,
        });
        let (plan, out) = self.build_foreign(&info, &est);
        let exprs = layout
            .iter()
            .map(|&g| match out.iter().position(|&x| x == g) {
                Some(i) => Expr::Col(i, self.col_type(g)),
                None => Expr::Const(Value::Null, self.col_type(g)),
            })
            .collect();
        Some(Plan::Project { input: Arc::new(plan), exprs })
    }

    /// The plan of a foreign join path: a Foreign Scan returning the relation's reltarget.
    pub(super) fn build_foreign(&self, info: &SArc<pj::RelInfo>, p: &JP) -> (Plan, Vec<usize>) {
        // (Its base relations only, renumbered in leaf order.)
        let all = self.fdw_bases(0).unwrap_or_default();
        let keep: Vec<usize> = (0..all.len()).filter(|k| info.relids >> k & 1 == 1).collect();
        let bases: Vec<pj::Base> = keep.iter().map(|&l| all[l].clone()).collect();
        let info = &SArc::new(self.reoriented_info(info).renumber(&|l| keep.iter().position(|&k| k == l).unwrap_or(0)));
        let out = info.reltarget.clone();
        let mut tlist = out.clone();
        for c in &info.local_conds {
            for g in c.columns() {
                if !tlist.contains(&g) {
                    tlist.push(g);
                }
            }
        }
        let mut scans = Vec::new();
        let mut cols = Vec::new();
        for b in &bases {
            scans.push(Arc::new(Plan::Scan(Box::new(b.scan.clone()))));
            cols.push(b.cols.clone());
        }
        let private = crate::ext::postgres_fdw::joinscan::JoinScan { info: info.clone(), bases, tlist, order: Vec::new(), limit: None };
        let fr = crate::sql::plan::ForeignRel {
            scans,
            cols,
            out: out.clone(),
            private: crate::sql::plan::FdwPrivate(SArc::new(private)),
            est: (p.rows, p.startup, p.total, p.width),
        };
        (Plan::Foreign(Box::new(fr)), out)
    }
}
