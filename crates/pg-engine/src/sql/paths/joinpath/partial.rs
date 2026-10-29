//! Partial paths in the join search (allpaths.c's partial_pathlist): a base relation's
//! parallel scans (create_plain_partial_paths), and each join relation's partial nested
//! loop and hash join paths over its outer side's partial paths
//! (consider_parallel_nestloop, hash_inner_and_outer's partial part), kept by
//! add_partial_path. The top relation's cheapest partial path may join in another order
//! than its cheapest serial path (TPC-H q05, q08): EXPLAIN builds its parallel plans along
//! that tree too (`JoinMeta::par_alt`).

use super::*;
use crate::sql::paths::parallel::{ParCfg, hazard};

/// What the group's partial paths need: the settings, and which leaves and clauses may
/// run in a worker (set_rel_consider_parallel, is_parallel_safe of the restrictions).
pub(super) struct Par {
    pub(super) cfg: ParCfg,
    safe_leaves: u32,
    unsafe_clauses: Vec<usize>,
    /// The most a parallel plan of the group may cost and still win (`partial_pass`).
    ceiling: f64,
}

/// A join pair of the search whose partial paths `partial_pass` makes: add_paths_to_joinrel's
/// arguments, the outer join being made, and for a semi join's right side made unique
/// (create_unique_path) the side and clauses to make it again from.
pub(super) struct Job {
    pub(super) ri: usize,
    pub(super) outer: usize,
    pub(super) inner: usize,
    pub(super) jt: Jt,
    pub(super) uniq: Uniq,
    pub(super) restrict: Vec<usize>,
    pub(super) cur_sj: Option<usize>,
    pub(super) unique_of: Option<(usize, Vec<usize>)>,
}

/// Per row of the top relation, the most an upper node's work (aggregate transitions and
/// finals, projections, a sort's comparisons beside its log factor) could save by running
/// in workers: ten times cpu_tuple_cost, far above what cost_agg or cost_sort charge.
const UPPER_ROW_ALLOWANCE: f64 = 0.1;

/// A group path as add_path's comparisons see it.
fn as_path(p: &JP) -> Path {
    Path {
        kind: PathKind::Seq,
        startup: p.startup,
        total: p.total,
        rows: p.rows,
        keys: p
            .keys
            .iter()
            .map(|k| PathKey {
                expr: Expr::Col(k.ec, DataType::Int4),
                desc: k.desc,
                nulls_first: k.nulls_first,
                cmp: 0,
                coll: 0,
            })
            .collect(),
        disabled: p.disabled,
    }
}

/// add_partial_path: keep `new` unless a path with pathkeys at least as good dominates it
/// (fewer disabled nodes, else fuzzily cheaper in total), dropping those it dominates;
/// the list stays in total-cost order.
fn add_partial_jpath(list: &mut Vec<(JP, usize)>, new: (JP, usize)) {
    let np = as_path(&new.0);
    let mut insert_at = 0;
    let mut i = 0;
    while i < list.len() {
        let old = as_path(&list[i].0);
        let mut remove_old = false;
        let mut accept = true;
        let kc = cmp_keys(&np.keys, &old.keys);
        if kc != KeysCmp::Different {
            if np.disabled != old.disabled {
                if np.disabled > old.disabled {
                    accept = false;
                } else {
                    remove_old = true;
                }
            } else if np.total > old.total * FUZZ {
                accept = kc == KeysCmp::Better1;
            } else if old.total > np.total * FUZZ {
                remove_old = kc != KeysCmp::Better2;
            } else if kc == KeysCmp::Better1 {
                remove_old = true;
            } else if kc == KeysCmp::Better2 {
                accept = false;
            } else if old.total > np.total * 1.000_000_000_1 {
                remove_old = true;
            } else {
                accept = false;
            }
        }
        if !accept {
            return;
        }
        if remove_old {
            list.remove(i);
        } else {
            if np.total >= old.total {
                insert_at = i + 1;
            }
            i += 1;
        }
    }
    list.insert(insert_at, new);
}

impl Search<'_, '_, '_> {
    /// The group's relations get partial paths when parallel query is on and the group
    /// has a leaf that can be scanned in parallel. (Not yet: groups computing
    /// PlaceHolderVars, partitionwise child joins, LATERAL items.)
    pub(super) fn parallel_settings(&self) -> Option<Par> {
        let cfg = ParCfg::read(self.pl.ctx).enabled()?;
        if !self.phvs.is_empty() || self.pw_rels.is_some() || self.leaves.len() > 31 {
            return None;
        }
        let cat = &self.pl.cat;
        let mut safe_leaves = 0u32;
        for (l, leaf) in self.leaves.iter().enumerate() {
            let Some((sp, _)) = &leaf.scan else { continue };
            let safe = leaf.sys.is_none()
                && leaf.members.is_none()
                && leaf.lateral.is_none()
                && leaf.proj.is_none()
                && !sp.table.temporary
                && sp.sample.is_none()
                && sp.current_of.is_none()
                && sp.mark.is_none()
                && !sp.table.is_foreign()
                && sp.filter.as_ref().is_none_or(|f| hazard(f, cat) == 's');
            if safe {
                safe_leaves |= 1 << l;
            }
        }
        if safe_leaves == 0 {
            return None;
        }
        let unsafe_clauses = (0..self.clauses.len()).filter(|&c| hazard(&self.clauses[c].expr, cat) != 's').collect();
        Some(Par { cfg, safe_leaves, unsafe_clauses, ceiling: f64::INFINITY })
    }

    /// consider_parallel of relation `set`: its leaves and the clauses among them run in a
    /// worker.
    fn rel_parallel(&self, set: u32) -> bool {
        let Some(par) = &self.par else { return false };
        set & !par.safe_leaves == 0 && par.unsafe_clauses.iter().all(|&c| self.clauses[c].leaves & !set != 0)
    }

    /// A serial path a worker can run (an inner side of a partial join).
    fn path_parallel(&self, p: &JP) -> bool {
        self.rel_parallel(p.set | p.req)
    }

    /// create_plain_partial_paths and the partial index and bitmap paths of scan leaf `l`.
    /// (`serial`: the leaf's serial paths of `rel`, its relation with its restrictions.)
    pub(super) fn base_partial(&self, l: usize, width: u32, rel: &Rc<Rel>, serial: &[Path]) -> Vec<(JP, usize)> {
        let set = 1u32 << l;
        let Some(par) = &self.par else {
            return Vec::new();
        };
        if !self.rel_parallel(set) || self.leaves[l].scan.is_none() {
            return Vec::new();
        }
        let seq = self.pl.partial_seq(rel, &par.cfg, false);
        let mut out = Vec::new();
        for (path, w) in self.pl.partial_paths_of(rel, &par.cfg, false, serial, seq) {
            let jp = JP {
                rows: path.rows,
                startup: path.startup,
                total: path.total,
                width,
                disabled: path.disabled,
                keys: self.gkeys(l, &path.keys),
                req: 0,
                set,
                node: JNode::Scan {
                    leaf: l,
                    path,
                    params: Vec::new(),
                    enforced: Vec::new(),
                    rel: rel.clone(),
                },
            };
            add_partial_jpath(&mut out, (jp, w));
        }
        out
    }

    /// A partial join path of `w` workers into relation `ri`'s partial paths (its rows
    /// those of the relation over the parallel divisor).
    pub(super) fn add_partial(&mut self, ri: usize, mut jp: JP, w: usize) {
        let Some(par) = &self.par else { return };
        jp.rows = costs::clamp_rows(jp.rows / par.cfg.divisor(w));
        add_partial_jpath(&mut self.rels[ri].partial, (jp, w));
    }

    /// add_paths_to_joinrel's partial paths: nested loops over each partial path of the
    /// outer side (consider_parallel_nestloop), and hash joins over its cheapest one, the
    /// inner side whole in each worker or (a Parallel Hash) its own cheapest partial path.
    pub(super) fn join_partial(&mut self, ri: usize, outer: usize, inner: usize, hash: &[(usize, bool)], ex: &Extra) {
        if self.rels[outer].partial.is_empty()
            || ex.uniq == Uniq::Outer
            || ex.jt == Jt::RightSemi
            || !self.rel_parallel(ex.set)
            || self.lateral_relids(ex.set) != 0
        {
            return;
        }
        let outer_parts = self.rels[outer].partial.clone();
        let inner_total = cheapest_of(&self.rels[inner].paths, false).cloned();
        if matches!(ex.jt, Jt::Inner | Jt::Left | Jt::Semi | Jt::Anti) {
            let oset = self.rels[outer].set;
            let matpath = inner_total.as_ref().filter(|it| {
                ex.uniq != Uniq::Inner
                    && self.g.material
                    && self.path_parallel(it)
                    && it.req & oset == 0
                    && !it.input().kind.materializes()
            });
            let matpath = matpath.map(|it| {
                let m = joins::material(&it.input(), &self.g);
                JP {
                    node: JNode::Material(Rc::new(it.clone())),
                    rows: m.rows,
                    startup: m.startup,
                    total: m.total,
                    width: it.width,
                    disabled: m.disabled,
                    keys: it.keys.clone(),
                    req: it.req,
                    set: it.set,
                }
            });
            let inner_params: Vec<JP> = inner_total
                .iter()
                .cloned()
                .chain(self.rels[inner].param.iter().filter(|_| ex.uniq != Uniq::Inner).cloned())
                .filter(|p| self.path_parallel(p))
                .collect();
            for (op, w) in &outer_parts {
                let pk = self.join_pathkeys(ex.jt, ex.set, op.set, &op.keys);
                for ip in &inner_params {
                    self.try_nestloop_w(ri, op, ip, pk.clone(), ex, Some(*w));
                    if let Some(m) = self.memoize_path(outer, op, ip, ex) {
                        self.try_nestloop_w(ri, op, &m, pk.clone(), ex, Some(*w));
                    }
                }
                if let Some(m) = &matpath {
                    self.try_nestloop_w(ri, op, m, pk.clone(), ex, Some(*w));
                }
            }
        }
        if hash.is_empty() || !(self.g.hashjoin || ex.jt == Jt::Full) {
            return;
        }
        let (op, w) = &outer_parts[0];
        let enable_hash = self.par.as_ref().is_some_and(|p| p.cfg.hash);
        if ex.uniq != Uniq::Inner
            && enable_hash
            && let Some((ip, iw)) = self.rels[inner].partial.first().cloned()
        {
            self.try_hash_w(ri, op, &ip, hash, ex, Some((*w, Some(iw))));
        }
        if matches!(ex.jt, Jt::Full | Jt::Right | Jt::RightAnti) {
            return;
        }
        let safe_inner = match &inner_total {
            Some(it) if self.path_parallel(it) => Some(it.clone()),
            _ if ex.uniq == Uniq::Inner => None,
            _ => self.rels[inner]
                .paths
                .iter()
                .filter(|p| p.req == 0 && self.path_parallel(p))
                .fold(None::<&JP>, |b, p| match b {
                    Some(b) if !cheaper(p, b, false) => Some(b),
                    _ => Some(p),
                })
                .cloned(),
        };
        if let Some(it) = safe_inner {
            self.try_hash_w(ri, op, &it, hash, ex, Some((*w, None)));
        }
    }

    /// The join relations' partial paths, made after the serial search from the pairs it
    /// joined (in its order), when a parallel plan of the group could beat the serial one.
    /// A parallel plan costs at least its partial path, which costs at least the cheapest
    /// partial scan of a leaf, plus parallel_setup_cost for its Gather; the serial plan at
    /// most the top relation's cheapest path plus the upper work on its rows that workers
    /// could share. When even that can't make up the difference (OLTP lookups joined to a
    /// big table), the pass is skipped and planning costs what it did without partial paths.
    pub(super) fn partial_pass(&mut self, top: usize) {
        let Some(setup) = self.par.as_ref().map(|p| p.cfg.setup_cost) else { return };
        let jobs = std::mem::take(&mut self.partial_jobs);
        let floor = self
            .rels
            .iter()
            .filter(|r| r.set.count_ones() == 1)
            .filter_map(|r| r.partial.first().map(|p| p.0.total))
            .fold(f64::INFINITY, f64::min);
        let Some(serial) = cheapest_of(&self.rels[top].paths, false).map(|p| p.total) else { return };
        let n = self.rels[top].rows.max(1.0);
        let ceiling = serial + n * (UPPER_ROW_ALLOWANCE + 2.0 * costs::CPU_OPERATOR_COST * (n + 2.0).log2());
        // (Under a LIMIT the plans compete for the rows read: the serial one's cost for
        // them against a Gather's start, which alone costs parallel_setup_cost.)
        let limited = self.order.as_ref().filter(|o| !o.hint).and_then(|o| match o.limit {
            Some((c, off)) if c > 0.0 && off >= 0.0 => Some((c, off)),
            _ => None,
        });
        if let Some((count, offset)) = limited
            && let Some((p, _)) = self.final_path(top)
        {
            let path = Path {
                kind: PathKind::Seq,
                startup: p.startup,
                total: p.total,
                rows: p.rows,
                keys: Vec::new(),
                disabled: p.disabled,
            };
            let (_, tot) = limit_costs(&path, count, offset);
            if setup > tot + (count + offset) * UPPER_ROW_ALLOWANCE {
                return;
            }
        }
        if floor + setup > ceiling {
            return;
        }
        if let Some(p) = &mut self.par {
            p.ceiling = ceiling;
        }
        let saved = self.cur_sj;
        for j in jobs {
            // (A partial join costs at least its outer side's cheapest partial path: past the
            // bound, nothing made over it could win.)
            if self.rels[j.outer].partial.first().is_none_or(|p| p.0.total + setup > ceiling) {
                continue;
            }
            self.cur_sj = j.cur_sj;
            let inner = match &j.unique_of {
                Some((r2, uclauses)) => match self.unique_rel(*r2, uclauses) {
                    Some(u) => {
                        self.rels.push(u);
                        self.rels.len() - 1
                    }
                    None => continue,
                },
                None => j.inner,
            };
            let (ex, _, hash) = self.pair_setup(j.ri, j.outer, inner, j.jt, j.uniq, &j.restrict);
            self.join_partial(j.ri, j.outer, inner, &hash, &ex);
            if j.unique_of.is_some() {
                self.rels.pop();
            }
        }
        self.cur_sj = saved;
    }

    /// Path `p` of the group's top relation as a plan returning the serial plan's row.
    fn alt_plan(&self, p: &JP, layout: &[usize]) -> Option<Plan> {
        let (plan, alayout) = self.build(p);
        if !matches!(plan, Plan::Join { .. }) {
            return None;
        }
        let exprs = layout
            .iter()
            .map(|g| alayout.iter().position(|h| h == g).map(|i| Expr::Col(i, self.col_type(*g))))
            .collect::<Option<Vec<_>>>()?;
        Some(Plan::Project { input: Arc::new(plan), exprs })
    }

    /// The top relation's cheapest partial path as a plan returning the serial plan's row
    /// (`layout`), when it joins otherwise than the serial path `best` (`JoinMeta::par_alt`).
    pub(super) fn par_alt(&self, best: &JP, layout: &[usize]) -> Option<(Plan, f64)> {
        let (pp, _) = self.rels.iter().find(|r| r.set == best.set)?.partial.first()?;
        let par = self.par.as_ref()?;
        if same_tree(pp, best) || pp.total + par.cfg.setup_cost > par.ceiling {
            return None;
        }
        Some((self.alt_plan(pp, layout)?, pp.total))
    }
}

/// Two paths join the same relations the same way round by the same methods over the same
/// kinds of scans: EXPLAIN's parallel plans along one are those along the other.
fn same_tree(a: &JP, b: &JP) -> bool {
    match (&a.node, &b.node) {
        (
            JNode::Join { method: m1, outer: o1, inner: i1, .. },
            JNode::Join { method: m2, outer: o2, inner: i2, .. },
        ) => m1 == m2 && same_tree(o1, o2) && same_tree(i1, i2),
        (JNode::Scan { leaf: l1, path: p1, .. }, JNode::Scan { leaf: l2, path: p2, .. }) => {
            l1 == l2 && std::mem::discriminant(&p1.kind) == std::mem::discriminant(&p2.kind)
        }
        (JNode::Material(x), JNode::Material(y))
        | (JNode::Memoize { input: x, .. }, JNode::Memoize { input: y, .. })
        | (JNode::Unique { input: x, .. }, JNode::Unique { input: y, .. }) => same_tree(x, y),
        _ => false,
    }
}
