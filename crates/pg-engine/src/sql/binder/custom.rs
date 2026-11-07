//! Custom plans from a statement's analysis, as PostgreSQL makes them from its cached
//! query tree (BuildCachedPlan with boundParams): each `$n` its value, the constants folded,
//! the WHERE placed again, planned. Binding the statement again with the values as constants
//! (`Binder::param_consts`) gives the same plan; this skips parse analysis.
//!
//! Step one covers a single-level SELECT over one table (`Analyzed::replannable`): its WHERE,
//! placed by `optimize::add_filter` while analyzing, is placed again with the values in, so
//! qual order by cost, equivalence-class constants, constraint exclusion, constant conflicts
//! and the index bounds are what a binding with the values would decide. Whatever folding
//! the values would change the shape of (a boolean or NULL result, a cast, a NULL value, a
//! failing call, a LIMIT) leaves it to that binding.

use std::sync::Arc;

use super::{Analyzed, BoundQuery};
use crate::sql::paths::ScanChoice;
use crate::error::Result;
use crate::exec::ExecCtx;
use crate::sql::expr::{Expr, eval};
use crate::sql::plan::{Plan, ScanPlan};
use crate::types::{DataType, Value};

/// Where a SELECT's own level placed its WHERE: its FROM item's scan before, and the
/// predicate placed in it.
pub(crate) struct WhereSite {
    scan: Plan,
    pred: Expr,
    /// No `$n` but in `pred`: the scan and the nodes over it read none, so a custom plan
    /// puts its values only where the WHERE is placed.
    alone: bool,
}

/// The site as `Binder` notes it: the scan after the WHERE was placed, to check nothing
/// changed it after (`finish`).
pub(crate) struct Noted {
    site: WhereSite,
    after: Plan,
}

/// `Binder::where_site`: nothing noted, one site, or more than one (no site).
#[derive(Default)]
pub(crate) enum Sites {
    #[default]
    None,
    One(Box<Noted>),
    Many,
}

impl Sites {
    /// `add_filter` placed `pred` in `scan` (a `Plan::Scan`), giving `after`.
    pub(crate) fn note(&mut self, scan: Plan, pred: Expr, after: &Plan) {
        *self = match self {
            Sites::None if matches!(after, Plan::Scan(_)) => {
                Sites::One(Box::new(Noted { site: WhereSite { scan, pred, alone: false }, after: after.clone() }))
            }
            _ => Sites::Many,
        };
    }

    /// The site of the analyzed `plan`, when its base scan is still the one `add_filter` made.
    pub(crate) fn finish(self, plan: &Plan) -> Option<Arc<WhereSite>> {
        let Sites::One(n) = self else { return None };
        let n = *n;
        let (Plan::Scan(after), Some(now)) = (&n.after, base_scan(plan)) else { return None };
        if format!("{after:?}") != format!("{now:?}") {
            return None;
        }
        let mut site = n.site;
        site.alone = match &site.scan {
            Plan::Scan(s) => with_base(plan, s.clone()).is_some_and(|mut p| {
                let mut found = false;
                crate::sql::paths::sublinkcopy::plan_exprs_mut(&mut p, &mut |e| found |= has_param(e));
                !found
            }),
            _ => false,
        };
        Some(Arc::new(site))
    }
}

/// `p` with `s` as its base scan: the nodes over it copied, what they read besides shared.
fn with_base(p: &Plan, s: Box<ScanPlan>) -> Option<Plan> {
    Some(match p {
        Plan::Scan(_) => Plan::Scan(s),
        Plan::Project { input, .. } | Plan::Agg { input, .. } | Plan::Sort { input, .. } | Plan::Limit { input, .. } => {
            let below = Arc::new(with_base(input, s)?);
            let mut n = p.clone();
            if let Plan::Project { input, .. } | Plan::Agg { input, .. } | Plan::Sort { input, .. } | Plan::Limit { input, .. } = &mut n {
                *input = below;
            }
            n
        }
        _ => return None,
    })
}

/// `p`, a scan under projections, with the scan `f` makes of its own.
fn scan_swapped(p: &Plan, f: &dyn Fn(&ScanPlan) -> Option<Box<ScanPlan>>) -> Option<Plan> {
    match p {
        Plan::Scan(s) => Some(Plan::Scan(f(s)?)),
        Plan::Project { input, exprs } => Some(Plan::Project { input: Arc::new(scan_swapped(input, f)?), exprs: exprs.clone() }),
        _ => None,
    }
}

/// The one scan under a chain of the nodes a single-level SELECT puts over it.
fn base_scan(p: &Plan) -> Option<&ScanPlan> {
    match p {
        Plan::Scan(s) => Some(s),
        Plan::Project { input, .. } | Plan::Agg { input, .. } | Plan::Sort { input, .. } | Plan::Limit { input, .. } => base_scan(input),
        _ => None,
    }
}

fn base_scan_mut(p: &mut Plan) -> Option<&mut Box<ScanPlan>> {
    match p {
        Plan::Scan(s) => Some(s),
        Plan::Project { input, .. } | Plan::Agg { input, .. } | Plan::Sort { input, .. } | Plan::Limit { input, .. } => {
            base_scan_mut(Arc::make_mut(input))
        }
        _ => None,
    }
}

/// What `Analyzed::bind_values` gives.
pub enum Custom {
    /// The custom plan (or the error planning it).
    Plan(Result<super::BoundQuery>),
    /// Binding these values would decide otherwise (a null, a fold that fails or gives null,
    /// constants that empty the scan): bind them afresh.
    Rebind,
    /// No values ever substitute in this analysis.
    Never,
}

/// Why values aren't put in.
enum Bail {
    Rebind,
    Never,
}

type Put<T> = std::result::Result<T, Bail>;

impl Analyzed {
    /// Whether values may be put in at all: a SELECT's own level planned by `plan`, its
    /// WHERE's place noted (`Binder::keep_site`).
    pub fn substitutes(&self) -> bool {
        self.site.is_some() && self.replannable() && matches!(self.step, super::Step::Select(_))
    }

    /// The custom plan with `vals` as the parameters' values, when it is this analysis with
    /// them put in; else whether binding the statement again with them
    /// (`Binder::param_consts`) is needed for these values or for any.
    pub fn bind_values(&self, vals: &[Value], ctx: &ExecCtx) -> Custom {
        match self.values_in(vals, ctx) {
            Ok(a) => Custom::Plan(a.into_bound(ctx)),
            Err(Bail::Rebind) => Custom::Rebind,
            Err(Bail::Never) => Custom::Never,
        }
    }

    /// Whether values may be put in at all (else whether binding them is needed for these
    /// values or for any), and where the WHERE goes.
    fn site_for(&self, vals: &[Value]) -> Put<&WhereSite> {
        let Some(site) = self.site.as_deref() else { return Err(Bail::Never) };
        if !self.replannable()
            || !matches!(self.step, super::Step::Select(_))
            || vals.len() != self.params.len()
            || !self.fold_err.is_empty()
            || !self.param_domains.is_empty()
            || self.untyped.is_some()
            || self.plan_err.is_some()
            || self.params.iter().any(|t| matches!(t, DataType::Unknown | DataType::Void))
        {
            return Err(Bail::Never);
        }
        if vals.iter().any(Value::is_null) {
            return Err(Bail::Rebind);
        }
        Ok(site)
    }

    /// The scan with the WHERE placed again, the values in.
    fn placed(&self, site: &WhereSite, sub: &Subst<'_, '_>) -> Put<Box<ScanPlan>> {
        let pred = sub.expr(&site.pred, true)?;
        let mut scan = site.scan.clone();
        if !site.alone {
            sub.plan(&mut scan)?;
        }
        match super::optimize::add_filter(&self.cat, scan, pred) {
            Plan::Scan(placed) => Ok(placed),
            _ => Err(Bail::Rebind),
        }
    }

    /// Whether a custom plan of this analysis may be another's with its scan chosen again
    /// (`rechosen`): no value is read but in the WHERE.
    pub fn rechoosable(&self) -> bool {
        self.site.as_deref().is_some_and(|s| s.alone)
    }

    /// The custom plan for `vals` made from `kept`, a custom plan of this analysis for other
    /// values, with its scan's path chosen again for them (`ScanChoice::rechoose`): what
    /// planning the analysis with them gives when the same path wins. None: plan it.
    pub fn rechosen(&self, kept: &BoundQuery, choice: &ScanChoice, vals: &[Value], ctx: &ExecCtx) -> Option<BoundQuery> {
        let site = self.site_for(vals).ok().filter(|s| s.alone)?;
        let placed = self.placed(site, &Subst { vals, ctx }).ok()?;
        let plan = Arc::new(scan_swapped(&kept.plan, &|old| choice.rechoose(old, &placed, ctx))?);
        // (Still costed by its scan's path alone, no Gather over it.)
        crate::sql::paths::planned_total(&plan, ctx, true)?;
        Some(BoundQuery {
            plan,
            cols: kept.cols.clone(),
            params: kept.params.clone(),
            perms: kept.perms.clone(),
            hidden_rtes: kept.hidden_rtes,
            param_domains: Vec::new(),
            gathers: Default::default(),
            cost: Default::default(),
        })
    }

    fn values_in(&self, vals: &[Value], ctx: &ExecCtx) -> Put<Analyzed> {
        let site = self.site_for(vals)?;
        let sub = Subst { vals, ctx };
        let placed = self.placed(site, &sub)?;
        // (With no `$n` outside the WHERE, the nodes over the scan are the analysis's.)
        let plan = if site.alone {
            with_base(&self.plan, placed).ok_or(Bail::Never)?
        } else {
            let mut plan = self.plan.clone();
            sub.plan(&mut plan)?;
            *base_scan_mut(&mut plan).ok_or(Bail::Never)? = placed;
            plan
        };
        Ok(Analyzed {
            plan,
            cols: self.cols.clone(),
            params: self.params.clone(),
            perms: self.perms.clone(),
            hidden_rtes: self.hidden_rtes,
            param_domains: Vec::new(),
            cat: self.cat.clone(),
            step: self.step,
            check_keys: self.check_keys,
            check_folds: self.check_folds,
            untyped: None,
            plan_err: None,
            fold_err: Vec::new(),
            rls: self.rls.clone(),
            replannable: self.replannable,
            site: None,
        })
    }
}

/// Putting the values in, folding what binding them would fold.
struct Subst<'a, 'c> {
    vals: &'a [Value],
    ctx: &'a ExecCtx<'c>,
}

impl Subst<'_, '_> {
    /// Every expression of `p` with the values in: its nodes a scan under projections,
    /// aggregation, sorts, and limits that read no parameter (`push_limit` decides by a
    /// constant one).
    fn plan(&self, p: &mut Plan) -> Put<()> {
        fn chain(p: &Plan) -> bool {
            match p {
                Plan::Scan(_) => true,
                Plan::Project { input, .. } | Plan::Agg { input, .. } | Plan::Sort { input, .. } => chain(input),
                Plan::Limit { input, limit, offset, .. } => {
                    !limit.iter().chain(offset.iter()).any(has_param) && chain(input)
                }
                _ => false,
            }
        }
        if !chain(p) {
            return Err(Bail::Never);
        }
        let mut r = Ok(());
        crate::sql::paths::sublinkcopy::plan_exprs_mut(p, &mut |e| {
            if r.is_ok() && has_param(e) {
                match self.expr(e, false) {
                    Ok(x) => *e = x,
                    Err(b) => r = Err(b),
                }
            }
        });
        r
    }

    /// `e` with the values in. `qual`: `e` is a qual, or an operand of AND / OR / NOT (where a
    /// parameter of its own would fold to a boolean binding simplifies).
    fn expr(&self, e: &Expr, qual: bool) -> Put<Expr> {
        crate::stack::guard(|| Ok(self.expr_go(e, qual))).unwrap_or(Err(Bail::Never))
    }

    fn expr_go(&self, e: &Expr, qual: bool) -> Put<Expr> {
        Ok(match e {
            _ if !has_param(e) => e.clone(),
            Expr::Param(i, t) if !qual => Expr::Const(self.vals.get(*i).ok_or(Bail::Never)?.clone(), t.clone()),
            Expr::And(v) => Expr::And(v.iter().map(|x| self.expr_go(x, true)).collect::<Put<_>>()?),
            Expr::Or(v) => Expr::Or(v.iter().map(|x| self.expr_go(x, true)).collect::<Put<_>>()?),
            Expr::Not(x) => Expr::Not(Box::new(self.expr_go(x, true)?)),
            Expr::Call(def, args, ty) => {
                let args: Vec<Expr> = args.iter().map(|x| self.expr_go(x, false)).collect::<Put<_>>()?;
                // (`Binder::fold_call`: an immutable call over constants is its value. One
                // whose value or inputs could carry a collation, or a boolean binding would
                // simplify further, never substitutes; a failing call or a null binds again.)
                if def.volatility == 'i' && !def.is_srf() && args.iter().all(|a| matches!(a, Expr::Const(..))) {
                    if matches!(ty, DataType::Bool) || ty.collatable() || args.iter().any(|a| a.ty().collatable()) {
                        return Err(Bail::Never);
                    }
                    let call = Expr::Call(def.clone(), args, ty.clone());
                    match eval(&call, &[], self.ctx) {
                        Ok(v) if !v.is_null() => Expr::Const(v, ty.clone()),
                        _ => return Err(Bail::Rebind),
                    }
                } else {
                    Expr::Call(def.clone(), args, ty.clone())
                }
            }
            _ => return Err(Bail::Never),
        })
    }
}

fn has_param(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| found |= matches!(x, Expr::Param(..)));
    found
}

/// A custom plan's casts of its values left to run (`Binder::kept_casts`): PostgreSQL's
/// selectivity estimates evaluate the operands of a scan's restriction clauses
/// (estimate_expression_value in get_restriction_variable and scalararraysel), so a value
/// a cast's input routine rejects fails the plan while it is made, a scan's clauses in
/// the order they are written.
pub fn estimate_casts(plan: &Plan, ctx: &ExecCtx) -> Result<()> {
    fn stale(e: &Expr) -> bool {
        let mut found = false;
        e.walk(&mut |x| {
            found |= matches!(x, Expr::Cast(c, from, to, _)
                if matches!(**c, Expr::Const(ref v, _) if !v.is_null()) && !crate::sql::expr::cast_immutable(from, to));
        });
        found
    }
    fn reads_row(e: &Expr) -> bool {
        let mut found = false;
        e.walk(&mut |x| found |= matches!(x, Expr::Col(..)));
        found
    }
    fn clause(e: &Expr, ctx: &ExecCtx) -> Result<()> {
        let operands: Vec<&Expr> = match e {
            Expr::And(v) | Expr::Or(v) => return v.iter().try_for_each(|x| clause(x, ctx)),
            Expr::Not(x) => return clause(x, ctx),
            Expr::ArrayOp { left, right, .. } => vec![&**left, &**right],
            Expr::Call(_, args, _) if args.len() == 2 && args.iter().any(reads_row) => args.iter().collect(),
            _ => return Ok(()),
        };
        for x in operands {
            if !reads_row(x) && stale(x) {
                eval(x, &[], ctx)?;
            }
        }
        Ok(())
    }
    if let Plan::Scan(s) = plan {
        s.written.iter().try_for_each(|w| clause(w, ctx))?;
    }
    crate::sql::explain::plan_inputs(plan).into_iter().try_for_each(|p| estimate_casts(p, ctx))
}
