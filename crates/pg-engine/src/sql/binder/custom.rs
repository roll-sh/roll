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

use super::Analyzed;
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
                Sites::One(Box::new(Noted { site: WhereSite { scan, pred }, after: after.clone() }))
            }
            _ => Sites::Many,
        };
    }

    /// The site of the analyzed `plan`, when its base scan is still the one `add_filter` made.
    pub(crate) fn finish(self, plan: &Plan) -> Option<Arc<WhereSite>> {
        let Sites::One(n) = self else { return None };
        let n = *n;
        let (Plan::Scan(after), Some(now)) = (&n.after, base_scan(plan)) else { return None };
        let same = format!("{after:?}") == format!("{now:?}");
        same.then(|| Arc::new(n.site))
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

    fn values_in(&self, vals: &[Value], ctx: &ExecCtx) -> Put<Analyzed> {
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
        let sub = Subst { vals, ctx };
        let pred = sub.expr(&site.pred, true)?;
        let mut scan = site.scan.clone();
        sub.plan(&mut scan)?;
        let Plan::Scan(placed) = super::optimize::add_filter(&self.cat, scan, pred) else { return Err(Bail::Rebind) };
        let mut plan = self.plan.clone();
        sub.plan(&mut plan)?;
        *base_scan_mut(&mut plan).ok_or(Bail::Never)? = placed;
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

