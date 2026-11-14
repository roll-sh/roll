//! Plans of simple queries kept by their template (`sql::literals`).
//!
//! PostgreSQL plans every simple query afresh with its literals as constants. Most OLTP
//! statements' plans don't depend on those values: a primary-key lookup is the same index
//! scan with the same estimates for every key, and a value stored or added to a column
//! changes nothing the planner decides. Such a statement's template is bound once with
//! those literals as parameters (the generic binding), and its later queries run that plan
//! with their values, skipping binding and planning.
//!
//! The literals made parameters are only (`candidates`): in WHERE, a value compared by `=`
//! with a column that a unique single-column index covers, every conjunct of the WHERE being
//! one such comparison (the planner estimates 1 / rows for any value, and no conjunct reads
//! anything that changes with time); in an UPDATE's SET, an INSERT's VALUES row, a value
//! stored as it is or combined with a column by `+`, `-` or `*`. Other literals stay
//! constants of the plan, which is kept for their texts only. The statement reads one plain
//! table (no rules, row security, inheritance or partitions) and no sub-select.
//!
//! When the template's plan is made, the query's own plan (bound the usual way, with its
//! literals as constants) is compared with it, parameters put in as those constants: they
//! must be the same plan with the same estimates, or the template keeps no plan (whatever a
//! literal's value changed, folding it, coercing it, estimating with it, shows there). A kept
//! plan is used while the catalog, settings, search_path, role and row security are the same
//! and each relation size the planner read when it was made reads the same
//! (`literals::SizeRead`), as PostgreSQL would plan that query now.

use super::{Prepared, Session};
use crate::catalog::Catalog as Cat;
use crate::sql::binder::{Analyzed, Binder, BoundQuery};
use crate::sql::binder::custom::Custom;
use crate::sql::expr::Expr;
use crate::sql::literals::{LitKind, Scan, SizeRead, Template};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};
use std::collections::HashMap;
use std::sync::Arc;

/// A simple query read through a template.
pub(crate) struct LitStmt {
    pub text: Box<str>,
    pub scan: Scan,
    pub tpl: Arc<Template>,
}

/// A statement's parse tree. One read through a template is made from the template and its
/// literals the first time something reads it: a query run with a plan kept for its
/// template's shape needs no tree of its own, and `shape` (the template's tree, whose
/// literals are another query's) answers what kind of statement it is.
pub struct StmtTree {
    tree: std::sync::OnceLock<Option<Node>>,
    lit: Option<Arc<LitStmt>>,
}

impl StmtTree {
    pub fn new(tree: Option<Node>) -> Self {
        StmtTree { tree: std::sync::OnceLock::from(tree), lit: None }
    }

    /// The statement of `l`, its tree made when it is read (None: `l`'s template isn't one
    /// statement that is read as it is).
    pub(super) fn lazy(l: Arc<LitStmt>) -> Option<Self> {
        let [s] = &l.tpl.stmts[..] else { return None };
        let n = s.stmt.as_deref()?;
        // (SELECT ... INTO is read as CREATE TABLE AS.)
        if crate::sql::binder::ast::select_into_as_ctas(n).is_some() {
            return None;
        }
        Some(StmtTree { tree: std::sync::OnceLock::new(), lit: Some(l) })
    }

    /// The statement's shape: its tree, or while it hasn't been made its template's (the
    /// same statement but for its literals' values and locations).
    pub(crate) fn shape(&self) -> Option<&Node> {
        match (self.tree.get(), &self.lit) {
            (Some(t), _) => t.as_ref(),
            (None, Some(l)) => l.tpl.stmts[0].stmt.as_deref(),
            (None, None) => None,
        }
    }
}

impl From<Option<Node>> for StmtTree {
    fn from(tree: Option<Node>) -> Self {
        StmtTree::new(tree)
    }
}

impl std::ops::Deref for StmtTree {
    type Target = Option<Node>;

    fn deref(&self) -> &Option<Node> {
        self.tree.get_or_init(|| {
            let l = self.lit.as_deref().expect("a statement tree to make");
            let stmts = crate::stack::with_headroom(crate::stack::tree_headroom(l.text.len()), || {
                crate::sql::literals::instantiate(&l.tpl, &l.text, &l.scan)
            });
            stmts.into_iter().next().and_then(|s| s.stmt).map(|n| *n)
        })
    }
}

impl std::ops::DerefMut for StmtTree {
    fn deref_mut(&mut self) -> &mut Option<Node> {
        let _ = &**self;
        self.tree.get_mut().expect("made")
    }
}

impl LitStmt {
    fn lit_text(&self, k: u32) -> &str {
        let l = &self.scan.lits[k as usize];
        &self.text[l.start..l.end]
    }

    /// The value (and type) of literal `k`, negated or not, as its A_Const binds.
    fn value(&self, k: u32, neg: bool) -> Option<(Value, DataType)> {
        let text = self.lit_text(k);
        match self.scan.lits[k as usize].kind {
            LitKind::Int4 => {
                let v: i32 = text.parse().ok()?;
                Some((Value::Int4(if neg { -v } else { v }), DataType::Int4))
            }
            LitKind::Int8 | LitKind::Numeric => {
                let s = if neg { format!("-{text}") } else { text.to_string() };
                match crate::sql::binder::numeric_const(&s).ok()? {
                    Expr::Const(v, t) => Some((v, t)),
                    _ => None,
                }
            }
            LitKind::Str => None,
        }
    }
}

/// A session's plans of templates, by shape (`Scan::shape`: queries whose literals have other
/// lengths have other keys and one shape).
#[derive(Default)]
pub(crate) struct LitPlans {
    map: HashMap<Box<str>, KeyPlans>,
    /// Queries run with a kept plan (tests).
    uses: u64,
    /// Queries planned from their template's analysis (tests).
    from_values: u64,
}

#[derive(Default)]
struct KeyPlans {
    /// Times a plan couldn't be kept for the template: it stops trying after a few.
    failures: u32,
    plans: Vec<Kept>,
    /// When no plan fits every value: the template's analysis its queries are planned from.
    values: Option<Values>,
}

/// A template's analysis with its numeric literals (outside LIMIT / OFFSET) as parameters:
/// each query's plan is it with the query's values put in (`Analyzed::bind_values`), the
/// plan PostgreSQL makes of the query, without parse analysis.
struct Values {
    /// The literals it has as constants, with their texts.
    fixed: Vec<(u32, Box<str>)>,
    /// Its parameters: the literal (negated or not) each is, and its type.
    params: Vec<(u32, bool, DataType)>,
    analysis: Arc<Analyzed>,
    env: Env,
    /// Found to be the plan planning the query gives (None: not checked yet).
    ok: Option<bool>,
}

/// Templates a session keeps plans for, and plans per template.
const MAX_KEYS: usize = 256;
const MAX_PER_KEY: usize = 4;
const MAX_FAILURES: u32 = 2;

struct Kept {
    /// The literals it has as constants, with their texts.
    fixed: Vec<(u32, Box<str>)>,
    /// Its parameters: the literal (negated or not) each is, and its type.
    params: Vec<(u32, bool, DataType)>,
    /// The statement with those literals as parameters.
    generic: Node,
    bq: Arc<BoundQuery>,
    /// Its analysis, planned again when a size changes (`Analyzed::replannable`).
    unplanned: Option<Arc<Analyzed>>,
    env: Env,
    reads: Watched,
}

/// What a plan read (`SizeRead`), with a watch on each read's relation taken before the
/// reads were last found to read the same (`sizing::size_watch`): while every watch holds,
/// so do the reads, with no lock and nothing read again. (None: a size the model doesn't
/// answer, an estimate, compared again each time.)
#[derive(Clone, Default)]
pub(super) struct Watched {
    reads: Arc<[SizeRead]>,
    watches: Arc<[Option<crate::catalog::heapsim::SizeWatch>]>,
}

impl Watched {
    fn new(reads: Vec<SizeRead>) -> Self {
        Watched { reads: reads.into(), watches: Arc::new([]) }
    }

    /// Whether every read is watched and its watch holds.
    fn watched(&self) -> bool {
        self.watches.len() == self.reads.len() && self.watches.iter().all(|w| w.as_ref().is_some_and(|w| w.holds()))
    }
}

/// A binding, and its analysis when planning it again gives what binding it again would.
struct Bound {
    bq: BoundQuery,
    unplanned: Option<Arc<Analyzed>>,
}

/// What a binding depends on besides relation sizes.
#[derive(PartialEq)]
struct Env {
    version: u64,
    settings: u64,
    path: crate::catalog::names::SearchPath,
    who: (u32, bool),
}

impl Session {
    fn lit_env(&mut self) -> Env {
        self.ensure_txn();
        // (The catalog the statement will begin with: another session's DDL may have
        // committed since the last one began.)
        let cat = self.txn.as_ref().unwrap().borrow().next_catalog();
        let st = self.state.settings.borrow();
        let who = (self.state.current_role(&cat), st.get_str("row_security") != Some("off"));
        Env { version: cat.version, settings: st.generation(), path: self.state.shared_search_path(), who }
    }

    /// The plan kept for `p`'s template that fits it now, with the values of its parameters
    /// (not while auto_explain may show the plan: it has the literals as parameters).
    pub(super) fn lit_plan_for(&mut self, p: &Prepared, params: &[Value]) -> Option<(Arc<BoundQuery>, Vec<Value>)> {
        if !params.is_empty() || p.lit.is_none() || self.state.settings.borrow().module_loaded("auto_explain") {
            return None;
        }
        let r = self.lit_plan(p);
        self.lit_plans.uses += r.is_some() as u64;
        r
    }

    /// How many queries ran with a plan kept for their template.
    pub fn kept_plan_uses(&self) -> u64 {
        self.lit_plans.uses
    }

    fn lit_plan(&mut self, p: &Prepared) -> Option<(Arc<BoundQuery>, Vec<Value>)> {
        let l = p.lit.as_deref()?;
        if self.lit_plans.map.get(&l.scan.shape)?.plans.is_empty() {
            return None;
        }
        let env = self.lit_env();
        let kp = self.lit_plans.map.get(&l.scan.shape)?;
        let at = kp.plans.iter().position(|k| k.fixed.iter().all(|(i, t)| l.lit_text(*i) == &**t))?;
        let k = &kp.plans[at];
        if k.env != env {
            return None;
        }
        let mut vals = Vec::with_capacity(k.params.len());
        for (i, neg, ty) in &k.params {
            let (v, t) = l.value(*i, *neg)?;
            if t != *ty {
                return None;
            }
            vals.push(v);
        }
        if k.reads.watched() {
            return Some((k.bq.clone(), vals));
        }
        let (bq, mut reads) = (k.bq.clone(), k.reads.clone());
        if self.reads_hold(&mut reads) {
            self.lit_plans.map.get_mut(&l.scan.shape)?.plans[at].reads = reads;
            return Some((bq, vals));
        }
        // A size changed: the plan another session made since, while what it read holds.
        let shared = self.shared_env(&env);
        let k = &self.lit_plans.map.get(&l.scan.shape)?.plans[at];
        if let Some(mut s) = shared.as_ref().and_then(|e| shared_plan(&l.scan.shape, e, k))
            && self.reads_hold(&mut s.2)
        {
            let k = &mut self.lit_plans.map.get_mut(&l.scan.shape)?.plans[at];
            (k.bq, k.unplanned, k.reads) = (s.0.clone(), s.1, s.2);
            return Some((s.0, vals));
        }
        // A relation's size changed: the template's binding planned again, as the query would
        // be (its plan doesn't depend on the values, whatever the sizes); a data-modifying
        // statement's analysis is kept, and only its access paths are planned again.
        let k = &self.lit_plans.map.get(&l.scan.shape)?.plans[at];
        let (generic, types) = (k.generic.clone(), k.params.iter().map(|p| p.2.clone()).collect());
        let (bound, reads) = match k.unplanned.clone() {
            Some(u) => {
                let (b, reads) = self.lit_replan(&u, &env.path);
                // (Debug builds: what binding it again gives.)
                #[cfg(debug_assertions)]
                {
                    let again = self.lit_bind(&generic, types, &env.path).0;
                    match (&b, again) {
                        (Some(b), Some(again)) => assert!(same_plan(&again.bq, b, &[], &[]), "re-planned {:?}", p.sql),
                        (None, None) => {}
                        (b, again) => panic!("re-planned {:?}: {} where binding gives {}", p.sql, b.is_some(), again.is_some()),
                    }
                }
                (b.map(|bq| Bound { bq, unplanned: Some(u) }), reads)
            }
            None => self.lit_bind(&generic, types, &env.path),
        };
        let kp = self.lit_plans.map.get_mut(&l.scan.shape)?;
        match bound {
            Some(b) => {
                let k = &mut kp.plans[at];
                k.bq = Arc::new(b.bq);
                k.unplanned = b.unplanned;
                k.reads = Watched::new(reads);
                if let Some(e) = shared {
                    share_plan(&l.scan.shape, e, k);
                }
                Some((k.bq.clone(), vals))
            }
            None => {
                kp.plans.remove(at);
                None
            }
        }
    }

    /// Bind `generic` (a template with parameters of `types`), and what the planner read
    /// (None when it failed, or read a column's data).
    fn lit_bind(&mut self, generic: &Node, types: Vec<DataType>, path: &crate::catalog::names::SearchPath) -> (Option<Bound>, Vec<SizeRead>) {
        let (bound, reads) = crate::sql::literals::recording(|| {
            self.with_ctx_path(&[], Some(path.clone()), |ctx| {
                let (bq, unplanned) = analyzed_bound(Binder::new(ctx, types).analyze_statement(generic)?, ctx)?;
                Ok(Bound { bq, unplanned })
            })
        });
        (bound.ok().filter(|_| !reads.contains(&SizeRead::Data)), reads)
    }

    /// `u`'s statement planned again, and what the planner read.
    fn lit_replan(&mut self, u: &Analyzed, path: &crate::catalog::names::SearchPath) -> (Option<BoundQuery>, Vec<SizeRead>) {
        let (bound, reads) =
            crate::sql::literals::recording(|| self.with_ctx_path(&[], Some(path.clone()), |ctx| u.plan(ctx)));
        (bound.ok().filter(|_| !reads.contains(&SizeRead::Data)), reads)
    }

    /// After `p` (read through a template) was planned the usual way as `planned`: keep a plan
    /// of its template for the queries that follow, when one fits every value (see above).
    pub(super) fn lit_learn(&mut self, p: &Prepared, stmt: &Node, planned: &BoundQuery) {
        let Some(l) = p.lit.as_deref() else { return };
        // (Planning runs its PL/pgSQL calls each time: a kept plan wouldn't.)
        if planned.procs.folded {
            return;
        }
        if self.state.settings.borrow().module_loaded("auto_explain") {
            return;
        }
        // (Not the first time the session sees the template: a query sent once isn't bound
        // twice.)
        let full = self.lit_plans.map.len() >= MAX_KEYS;
        match self.lit_plans.map.get(&l.scan.shape) {
            None if !full => {
                self.lit_plans.map.insert(l.scan.shape.clone(), KeyPlans::default());
                return;
            }
            Some(kp) if kp.failures >= MAX_FAILURES => return,
            _ => {}
        }
        let kept = self.lit_make(l, stmt, planned);
        if self.lit_plans.map.len() >= MAX_KEYS && !self.lit_plans.map.contains_key(&l.scan.shape) {
            self.lit_plans.map.clear();
        }
        let kp = self.lit_plans.map.entry(l.scan.shape.clone()).or_default();
        match kept {
            Some(k) => {
                kp.plans.retain(|o| o.fixed != k.fixed);
                if kp.plans.len() >= MAX_PER_KEY {
                    kp.plans.remove(0);
                }
                kp.plans.push(k);
            }
            None => {
                kp.failures += 1;
                if kp.values.is_none() {
                    let v = self.lit_values(l, stmt);
                    if let Some(kp) = self.lit_plans.map.get_mut(&l.scan.shape) {
                        kp.values = v;
                    }
                }
            }
        }
    }

    fn lit_make(&mut self, l: &LitStmt, stmt: &Node, planned: &BoundQuery) -> Option<Kept> {
        let (cat, table) = planned_table(self, planned)?;
        let cands = candidates(stmt, &cat, &table)?;
        // The statement with the candidate literals' A_Consts made parameters.
        let mut generic = stmt.clone();
        let (mut j, mut ok) = (0, true);
        let mut params: Vec<(u32, bool, DataType)> = Vec::new();
        let mut vals = Vec::new();
        let mut kept_const = vec![false; l.scan.lits.len()];
        crate::sql::nodewalk::node(&mut generic, &mut |n| match &n.node {
            Some(NodeEnum::SubLink(_)) => ok = false,
            Some(NodeEnum::FuncCall(f)) if f.over.is_some() => ok = false,
            Some(NodeEnum::AConst(c)) => {
                let slot = l.tpl.consts.get(j).copied().flatten();
                j += 1;
                let Some((k, neg)) = slot else { return };
                let value = cands.contains(&Slot::Const(c.location)).then(|| l.value(k, neg)).flatten();
                let Some((v, ty)) = value else {
                    kept_const[k as usize] = true;
                    return;
                };
                params.push((k, neg, ty));
                vals.push(v);
                let location = c.location;
                n.node = Some(NodeEnum::ParamRef(pb::ParamRef { number: params.len() as i32, location }));
            }
            _ => {}
        });
        if !ok || j != l.tpl.consts.len() || params.is_empty() {
            return None;
        }
        let env = self.lit_env();
        let types: Vec<DataType> = params.iter().map(|p| p.2.clone()).collect();
        let (bound, reads) = self.lit_bind(&generic, types.clone(), &env.path);
        let Bound { bq, unplanned } = bound?;
        if !same_plan(planned, &bq, &types, &vals) {
            return None;
        }
        let fixed = (0..l.scan.lits.len() as u32)
            .filter(|&k| kept_const[k as usize] || !params.iter().any(|p| p.0 == k))
            .map(|k| (k, Box::from(l.lit_text(k))))
            .collect();
        Some(Kept { fixed, params, generic, bq: Arc::new(bq), unplanned, env, reads: Watched::new(reads) })
    }

    /// The analysis of a SELECT template whose plan depends on its values (`Values`).
    fn lit_values(&mut self, l: &LitStmt, stmt: &Node) -> Option<Values> {
        let Some(NodeEnum::SelectStmt(s)) = &stmt.node else { return None };
        // (A LIMIT's or OFFSET's literal stays a constant: `push_limit` decides by it.)
        let mut limit_locs = Vec::new();
        for n in [s.limit_count.as_deref(), s.limit_offset.as_deref()].into_iter().flatten() {
            crate::sql::nodewalk::node(&mut n.clone(), &mut |x| {
                if let Some(NodeEnum::AConst(c)) = &x.node {
                    limit_locs.push(c.location);
                }
            });
        }
        let mut generic = stmt.clone();
        let (mut j, mut ok) = (0, true);
        let mut params: Vec<(u32, bool, DataType)> = Vec::new();
        let mut kept_const = vec![false; l.scan.lits.len()];
        crate::sql::nodewalk::node(&mut generic, &mut |n| match &n.node {
            Some(NodeEnum::SubLink(_)) => ok = false,
            Some(NodeEnum::AConst(c)) => {
                let slot = l.tpl.consts.get(j).copied().flatten();
                j += 1;
                let Some((k, neg)) = slot else { return };
                let value = (!limit_locs.contains(&c.location)).then(|| l.value(k, neg)).flatten();
                let Some((_, ty)) = value else {
                    kept_const[k as usize] = true;
                    return;
                };
                params.push((k, neg, ty));
                let location = c.location;
                n.node = Some(NodeEnum::ParamRef(pb::ParamRef { number: params.len() as i32, location }));
            }
            _ => {}
        });
        if !ok || j != l.tpl.consts.len() {
            return None;
        }
        let env = self.lit_env();
        let types: Vec<DataType> = params.iter().map(|p| p.2.clone()).collect();
        // (An analysis that raised a notice, folding a constant, isn't kept: PostgreSQL
        // raises it each time it plans the query. Its notices were the query's already.)
        let notices = self.notices.len();
        let a = crate::exec::collected_notices(|| {
            self.with_ctx_path(&[], Some(env.path.clone()), |ctx| {
                let mut b = Binder::new(ctx, types);
                b.keep_site = true;
                b.analyze_statement(&generic)
            })
        });
        if self.notices.len() > notices {
            self.notices.truncate(notices);
            return None;
        }
        let a = a.ok()?;
        if !a.substitutes() {
            return None;
        }
        let fixed = (0..l.scan.lits.len() as u32)
            .filter(|&k| kept_const[k as usize] || !params.iter().any(|p| p.0 == k))
            .map(|k| (k, Box::from(l.lit_text(k))))
            .collect();
        Some(Values { fixed, params, analysis: Arc::new(a), env, ok: None })
    }

    /// A simple query's plan from its template's analysis with its values put in (`Values`):
    /// the first one checked against planning it the usual way (each one in debug builds).
    pub(super) fn lit_values_plan(&mut self, p: &Prepared, params: &[Value]) -> Option<Arc<BoundQuery>> {
        if !params.is_empty() || self.state.settings.borrow().module_loaded("auto_explain") {
            return None;
        }
        let l = p.lit.as_deref()?;
        let env = self.lit_env();
        let v = self.lit_plans.map.get(&l.scan.shape)?.values.as_ref()?;
        if v.env != env || v.ok == Some(false) || !v.fixed.iter().all(|(i, t)| l.lit_text(*i) == &**t) {
            return None;
        }
        let mut vals = Vec::with_capacity(v.params.len());
        for (i, neg, ty) in &v.params {
            let (val, t) = l.value(*i, *neg)?;
            if t != *ty {
                return None;
            }
            vals.push(val);
        }
        let (a, checked) = (v.analysis.clone(), v.ok == Some(true));
        let made = self.with_ctx_path(&[], Some(env.path.clone()), |ctx| Ok(a.bind_values(&vals, ctx))).ok()?;
        let mut bq = match made {
            Custom::Plan(Ok(bq)) => bq,
            Custom::Plan(Err(_)) | Custom::Rebind => return None,
            Custom::Never => {
                self.lit_plans.map.get_mut(&l.scan.shape)?.values = None;
                return None;
            }
        };
        bq.params.clear();
        if checked && !cfg!(debug_assertions) {
            self.lit_plans.from_values += 1;
            return Some(Arc::new(bq));
        }
        // (Planned the usual way to check it: the notices folding raises are the plan's
        // already.)
        let notices = self.notices.len();
        let own = crate::exec::collected_notices(|| match p.stmt.as_ref() {
            Some(stmt) => self.planned(p, stmt, &[]).map(|r| r.0),
            None => Err(crate::error::internal("a query without a tree")),
        });
        self.notices.truncate(notices);
        let own = own.ok()?;
        let same = same_plan(&own, &bq, &[], &[]);
        debug_assert!(same, "plan from the analysis of {:?}:\n{:?}\n{:?}", p.sql, own.plan, bq.plan);
        if let Some(v) = self.lit_plans.map.get_mut(&l.scan.shape)?.values.as_mut() {
            v.ok = Some(same);
        }
        self.lit_plans.from_values += u64::from(same);
        Some(own)
    }

    /// How many simple queries were planned from their template's analysis (`Values`).
    pub fn analysis_query_uses(&self) -> u64 {
        self.lit_plans.from_values
    }

    /// What a plan kept now may be shared under (`SHARED`): None in a session with
    /// temporary objects, which its plans may name where another session's would name its own.
    fn shared_env(&self, env: &Env) -> Option<SharedEnv> {
        let cat = self.txn.as_ref()?.borrow().next_catalog();
        if cat.schema_oid(&self.state.temp_schema()).is_ok() {
            return None;
        }
        Some(SharedEnv {
            db: self.state.db.instance,
            version: env.version,
            values: self.state.settings.borrow().values_key(),
            path: env.path.clone(),
            who: env.who,
        })
    }
}

/// Plans of templates as a size change left them, shared by the process's sessions: the
/// first session to see a relation's size change plans the template again, and the others
/// take that plan while what it read reads the same. By shape, at most
/// `MAX_SHARED_KEYS` shapes and `MAX_SHARED_PER_KEY` plans each.
static SHARED: std::sync::LazyLock<std::sync::Mutex<HashMap<Box<str>, Vec<SharedKept>>>> =
    std::sync::LazyLock::new(Default::default);
const MAX_SHARED_KEYS: usize = 128;
const MAX_SHARED_PER_KEY: usize = 2;

/// What a shared plan was made under: a session's `Env` in its database, settings by value.
#[derive(PartialEq)]
struct SharedEnv {
    db: u64,
    version: u64,
    values: Arc<crate::session::settings::ValuesKey>,
    path: crate::catalog::names::SearchPath,
    who: (u32, bool),
}

struct SharedKept {
    env: SharedEnv,
    fixed: Vec<(u32, Box<str>)>,
    params: Vec<(u32, bool, DataType)>,
    bq: Arc<BoundQuery>,
    unplanned: Option<Arc<Analyzed>>,
    reads: Watched,
}

type SharedPlan = (Arc<BoundQuery>, Option<Arc<Analyzed>>, Watched);

/// The plan of `shape` shared under `env` for `k`'s literals.
fn shared_plan(shape: &str, env: &SharedEnv, k: &Kept) -> Option<SharedPlan> {
    let map = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    let s = map.get(shape)?.iter().find(|s| s.env == *env && s.fixed == k.fixed && s.params == k.params)?;
    Some((s.bq.clone(), s.unplanned.clone(), s.reads.clone()))
}

/// Share `k`, planned again under `env`.
fn share_plan(shape: &str, env: SharedEnv, k: &Kept) {
    let mut map = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    // (What goes is dropped once the lock is let go.)
    let mut gone = Vec::new();
    if map.len() >= MAX_SHARED_KEYS && !map.contains_key(shape) {
        gone.extend(map.drain().flat_map(|(_, v)| v));
    }
    let v = map.entry(shape.into()).or_default();
    // (Plans of older catalogs of the database go: an analysis holds its catalog.)
    let (old, keep): (Vec<_>, Vec<_>) =
        std::mem::take(v).into_iter().partition(|s| s.env.db == env.db && s.env.version < env.version);
    (*v, gone) = (keep, old.into_iter().chain(gone).collect());
    if let Some(at) = v.iter().position(|s| s.env == env && s.fixed == k.fixed && s.params == k.params) {
        gone.push(v.remove(at));
    } else if v.len() >= MAX_SHARED_PER_KEY {
        gone.push(v.remove(0));
    }
    let (bq, unplanned, reads) = (k.bq.clone(), k.unplanned.clone(), k.reads.clone());
    v.push(SharedKept { env, fixed: k.fixed.clone(), params: k.params.clone(), bq, unplanned, reads });
    drop(map);
    drop(gone);
}

/// Extended-protocol statements' bindings by text and the Parse message's parameter types.
///
/// A Parse binds its query (the generic plan, which reads relation sizes); the binding is
/// kept for the next Parse of the same text while what it depends on reads the same, so an
/// unnamed statement sent again isn't bound again. And PostgreSQL plans an unnamed statement,
/// and a named one's first five executions, afresh with the parameters' values as constants
/// (custom plans): a statement whose plan can't depend on those values (`candidates` over
/// its parameters, checked once against a custom plan with `same_plan`) runs the generic
/// binding instead, counted and costed as the custom plan it is.
#[derive(Default)]
pub(crate) struct ParamPlans {
    map: HashMap<Box<str>, Vec<ParamEntry>>,
    /// Custom plans run with the generic binding (tests).
    uses: u64,
    /// Custom plans made from their Parse's analysis with the values put in (tests).
    from_analysis: u64,
    /// Of those, the last custom plan with its scan chosen again (tests).
    rechosen: u64,
}

struct ParamEntry {
    oids: Box<[u32]>,
    env: Env,
    reads: Watched,
    parsed: Parsed,
    /// Its analysis, planned again when a size changes (`Analyzed::replannable`).
    unplanned: Option<Arc<Analyzed>>,
    /// Whether its custom plans are its binding (None: not checked yet).
    custom_is_generic: Option<bool>,
    /// A custom plan's cost, planning charged.
    custom_cost: Option<f64>,
    /// Whether its analysis with a custom plan's values put in (`Analyzed::bind_values`) was
    /// found to be the custom plan binding them gives (None: not checked yet).
    values_ok: Option<bool>,
    /// The last custom plan made from its analysis, for the next values to choose its scan
    /// again (`Analyzed::rechosen`).
    kept: Option<KeptCustom>,
}

/// A custom plan of a lone scan kept for the next execution: the choice of its scan's path
/// (`paths::ScanChoice`) and the sizes planning read, which must read the same.
struct KeptCustom {
    bq: Arc<BoundQuery>,
    choice: Arc<crate::sql::paths::ScanChoice>,
    reads: Watched,
    /// The custom plan made last (`bq`'s, or the last one rechosen): the next rechosen one
    /// takes its executor tree (`Session::kept_tree`).
    last: Arc<crate::sql::plan::Plan>,
}

/// What a Parse of a query found.
#[derive(Clone)]
pub(super) struct Parsed {
    pub param_types: Vec<DataType>,
    pub param_oids: Vec<(usize, u32)>,
    pub fixed_result: Option<Vec<(String, u32, i32)>>,
    pub bq: Arc<BoundQuery>,
}

const MAX_PARAM_KEYS: usize = 256;
/// Longer statements aren't kept: their trees are big, and seldom sent again and again.
const MAX_PARAM_SQL: usize = 8192;

impl Session {
    /// How many custom plans ran with their statement's generic binding.
    pub fn generic_custom_uses(&self) -> u64 {
        self.param_plans.uses
    }

    /// How many custom plans were their Parse's analysis with the values put in
    /// (`Analyzed::bind_values`).
    pub fn analysis_custom_uses(&self) -> u64 {
        self.param_plans.from_analysis
    }

    /// How many custom plans were the one before with their scan chosen again for the
    /// values (`Analyzed::rechosen`).
    pub fn rechosen_custom_uses(&self) -> u64 {
        self.param_plans.rechosen
    }

    fn param_entry(&mut self, sql: &str, oids: &[u32]) -> Option<&mut ParamEntry> {
        self.param_plans.map.get_mut(sql)?.iter_mut().find(|e| &*e.oids == oids)
    }

    /// Whether `w`'s reads read now what they read: while each watch holds, they do; else
    /// they are read again (and watched again when they read the same).
    fn reads_hold(&mut self, w: &mut Watched) -> bool {
        if w.watched() {
            return true;
        }
        // (Watched before they are read: a change after the read is one the watch sees.)
        let cat = self.txn.as_ref().unwrap().borrow().next_catalog();
        let watches: Vec<_> = w.reads.iter().map(|r| r.watch(&cat, &self.state.db)).collect();
        if !self.values_hold(&w.reads) {
            return false;
        }
        w.watches = watches.into();
        true
    }

    /// Whether `reads` read now what they read (the size model's answers first, without a
    /// statement).
    fn values_hold(&mut self, reads: &[SizeRead]) -> bool {
        if reads.is_empty() {
            return true;
        }
        let cat = self.txn.as_ref().unwrap().borrow().next_catalog();
        let mut unknown = false;
        for r in reads {
            match r.holds_known(&cat, &self.state.db) {
                Some(false) => return false,
                Some(true) => {}
                None => unknown = true,
            }
        }
        !unknown || self.with_ctx(&[], |ctx| Ok(reads.iter().all(|r| r.holds(ctx)))).unwrap_or(false)
    }

    /// What a Parse of `sql` with `oids` found before, with the binding's catalog version,
    /// search_path and role, when binding it now would find the same; and whether its custom
    /// plans are that binding.
    pub(super) fn param_parsed(&mut self, sql: &str, oids: &[u32]) -> Option<(Parsed, u64, crate::catalog::names::SearchPath, (u32, bool), bool)> {
        self.param_plans.map.get(sql)?;
        let env = self.lit_env();
        let e = self.param_entry(sql, oids)?;
        if e.env != env {
            return None;
        }
        let custom = e.custom_is_generic == Some(true);
        if e.reads.watched() {
            return Some((e.parsed.clone(), env.version, env.path, env.who, custom));
        }
        let (parsed, mut reads) = (e.parsed.clone(), e.reads.clone());
        if self.reads_hold(&mut reads) {
            self.param_entry(sql, oids)?.reads = reads;
            return Some((parsed, env.version, env.path, env.who, custom));
        }
        // A size changed: its analysis planned again (`Analyzed::replannable`).
        let u = self.param_entry(sql, oids)?.unplanned.clone()?;
        let (bq, reads) = self.lit_replan(&u, &env.path);
        let parsed = Parsed { bq: Arc::new(bq?), ..parsed };
        let e = self.param_entry(sql, oids)?;
        (e.parsed, e.reads, e.custom_cost, e.kept) = (parsed.clone(), Watched::new(reads), None, None);
        Some((parsed, env.version, env.path, env.who, custom))
    }

    /// Keep what a Parse of `sql` with `oids` found, its binding having read `reads`; whether
    /// its custom plans are known to be that binding.
    pub(super) fn param_keep(
        &mut self,
        sql: &str,
        oids: &[u32],
        parsed: Parsed,
        reads: Vec<SizeRead>,
        unplanned: Option<Arc<Analyzed>>,
    ) -> bool {
        if sql.len() > MAX_PARAM_SQL || reads.contains(&SizeRead::Data) {
            return false;
        }
        let env = self.lit_env();
        if let Some(e) = self.param_entry(sql, oids) {
            // (The same catalog and settings: whether custom plans are the binding stands.)
            if e.env != env {
                e.custom_is_generic = None;
                e.values_ok = None;
            }
            (e.env, e.reads, e.parsed, e.unplanned, e.custom_cost, e.kept) = (env, Watched::new(reads), parsed, unplanned, None, None);
            return e.custom_is_generic == Some(true);
        }
        if self.param_plans.map.len() >= MAX_PARAM_KEYS {
            self.param_plans.map.clear();
        }
        let reads = Watched::new(reads);
        let e = ParamEntry { oids: oids.into(), env, reads, parsed, unplanned, custom_is_generic: None, custom_cost: None, values_ok: None, kept: None };
        self.param_plans.map.entry(sql.into()).or_default().push(e);
        false
    }

    /// Whether a custom plan of `p` with `params` may be its generic binding: values not
    /// null, no domain parameters, no auto_explain (which would show `$n`); the Parse's types.
    fn param_custom_key(&self, p: &Prepared, params: &[Value]) -> Option<Box<[u32]>> {
        let plans = p.plans.lock();
        let oids = plans.parse_oids.clone()?;
        let plain = plans.param_oids.is_empty()
            && !params.is_empty()
            && !params.iter().any(Value::is_null)
            && !self.state.settings.borrow().module_loaded("auto_explain");
        plain.then_some(oids)
    }

    /// A custom plan of `p` with `params` when it is the statement's generic binding: that
    /// binding, and the custom plan's cost (None for the unnamed statement: found when a later
    /// execution needs it, `PlanUses::uncosted`). `fresh`: the Bind right after a Parse that
    /// found the binding to be its custom plan (`PlanUses::fresh_generic`).
    pub(super) fn param_custom(&mut self, p: &Prepared, params: &[Value], fresh: bool) -> Option<(Arc<BoundQuery>, Option<f64>)> {
        let (unnamed, procs) = {
            let plans = p.plans.lock();
            (plans.unnamed, plans.procs)
        };
        if procs {
            return None;
        }
        let oids = self.param_custom_key(p, params)?;
        if fresh
            && unnamed
            && let Some(bq) = p.bound.lock().as_ref().map(|b| b.3.clone())
        {
            self.param_plans.uses += 1;
            return Some((bq, None));
        }
        // (Custom plans not known to be the binding need no environment read: the caller
        // binds them, starting the transaction itself.)
        if self.param_entry(&p.sql, &oids)?.custom_is_generic != Some(true) {
            return None;
        }
        let env = self.lit_env();
        let e = self.param_entry(&p.sql, &oids)?;
        if e.env != env || e.parsed.param_types != p.param_types {
            return None;
        }
        let (bq, mut reads, cost) = (e.parsed.bq.clone(), e.reads.clone(), e.custom_cost);
        if !self.reads_hold(&mut reads) {
            return None;
        }
        self.param_entry(&p.sql, &oids)?.reads = reads;
        let cost = match cost {
            _ if unnamed => None,
            Some(c) => Some(c),
            None => {
                let c = self.plan_cost(&bq, params, true);
                self.param_entry(&p.sql, &oids)?.custom_cost = Some(c);
                Some(c)
            }
        };
        self.param_plans.uses += 1;
        Some((bq, cost))
    }

    /// Whether `param_learn` may note something of `p` (its text has generic bindings kept):
    /// then its custom plan's cost is needed as the plan is made.
    pub(super) fn param_learns(&self, p: &Prepared) -> bool {
        self.param_plans.map.contains_key(&*p.sql)
    }

    /// After a custom plan `custom` (costing `cost`) of `p` with `params` was made the usual
    /// way: note whether its custom plans are its generic binding, when not known yet.
    pub(super) fn param_learn(&mut self, p: &Prepared, stmt: &Node, custom: &BoundQuery, params: &[Value], cost: f64) {
        let Some(oids) = self.param_custom_key(p, params) else { return };
        if !self.param_plans.map.contains_key(&*p.sql) {
            return;
        }
        let env = self.lit_env();
        let Some(e) = self.param_entry(&p.sql, &oids) else { return };
        if e.custom_is_generic.is_some() || e.env != env || e.parsed.param_types != p.param_types {
            return;
        }
        let (bq, mut reads) = (e.parsed.bq.clone(), e.reads.clone());
        // (Bound over other sizes, it could differ by its estimates alone: checked later.)
        if !self.reads_hold(&mut reads) {
            return;
        }
        let ok = params_free(self, stmt, custom) && same_plan(custom, &bq, &p.param_types, params);
        if let Some(e) = self.param_entry(&p.sql, &oids) {
            e.custom_is_generic = Some(ok);
            e.custom_cost = ok.then_some(cost);
        }
    }
}

impl Session {
    /// A custom plan of `p` with `params` made from its Parse's analysis with the values put
    /// in (`Analyzed::bind_values`), PostgreSQL's custom plan of its cached query tree. The
    /// first one of an entry is checked against binding the values afresh (and every one in
    /// debug builds); None: bind afresh. `reuse`: when the custom plan before is a lone scan
    /// whose sizes read the same, its scan's path chosen again for these values
    /// (`Analyzed::rechosen`, planning's result) instead (not for EXPLAIN EXECUTE).
    pub(super) fn values_custom(&mut self, p: &Prepared, stmt: &Node, params: &[Value], reuse: bool) -> Option<Arc<BoundQuery>> {
        let oids = {
            let plans = p.plans.lock();
            if plans.procs || !plans.param_oids.is_empty() {
                return None;
            }
            plans.parse_oids.clone()?
        };
        self.param_plans.map.get(&*p.sql)?;
        let env = self.lit_env();
        let e = self.param_entry(&p.sql, &oids)?;
        if e.env != env || e.parsed.param_types != p.param_types || e.values_ok == Some(false) {
            return None;
        }
        let (a, checked) = (e.unplanned.clone()?, e.values_ok == Some(true));
        let kept = e.kept.as_ref().filter(|_| reuse && checked).map(|k| (k.bq.clone(), k.choice.clone(), k.reads.clone()));
        if let Some((kb, choice, mut reads)) = kept
            && self.reads_hold(&mut reads)
            && let Some(bq) = self.with_ctx(params, |ctx| Ok(a.rechosen(&kb, &choice, params, ctx))).ok().flatten()
        {
            // (Debug builds: it is the plan planning the values gives, costing the same.)
            #[cfg(debug_assertions)]
            if let Ok(Custom::Plan(Ok(own))) = self.with_ctx(params, |ctx| Ok(a.bind_values(params, ctx))) {
                assert!(same_plan(&own, &bq, &[], &[]), "rechosen custom plan of {:?} with {params:?}:\n{:?}\n{:?}", p.sql, own.plan, bq.plan);
                let (c, r) = (self.plan_cost(&own, params, true), self.plan_cost(&bq, params, true));
                assert!((c - r).abs() <= 1e-9 * c.abs().max(1.0), "rechosen custom cost of {:?}: {c} {r}", p.sql);
            }
            let bq = Arc::new(bq);
            if let Some(k) = self.param_entry(&p.sql, &oids).and_then(|e| e.kept.as_mut()) {
                k.reads = reads;
                let last = std::mem::replace(&mut k.last, bq.plan.clone());
                self.rechosen_after = Some((bq.plan.clone(), last));
            }
            self.param_plans.from_analysis += 1;
            self.param_plans.rechosen += 1;
            return Some(bq);
        }
        let ((made, choice), mut reads) = crate::sql::literals::recording(|| {
            crate::sql::paths::keeping_choice(|| self.with_ctx(params, |ctx| Ok(a.bind_values(params, ctx))))
        });
        for r in &reads {
            crate::sql::literals::note_read(|| r.clone());
        }
        let bq = match made.ok()? {
            Custom::Plan(Ok(bq)) => Arc::new(bq),
            Custom::Plan(Err(_)) | Custom::Rebind => return None,
            Custom::Never => {
                self.param_entry(&p.sql, &oids)?.values_ok = Some(false);
                return None;
            }
        };
        // (A scan's estimates read from its data are made again as its path is chosen.)
        reads.retain(|r| *r != SizeRead::Data);
        let kept = choice.filter(|_| a.rechoosable()).map(|choice| KeptCustom { last: bq.plan.clone(), bq: bq.clone(), choice, reads: Watched::new(reads) });
        if checked && !cfg!(debug_assertions) {
            if let Some(e) = self.param_entry(&p.sql, &oids) {
                e.kept = kept;
            }
            self.param_plans.from_analysis += 1;
            return Some(bq);
        }
        let notices = self.notices.len();
        let own = crate::exec::collected_notices(|| self.bound_custom_afresh(p, stmt, params));
        self.notices.truncate(notices);
        let own = own.ok()?;
        let same = same_plan(&own, &bq, &[], &[]);
        debug_assert!(same, "custom plan from the analysis of {:?} with {params:?}:\n{:?}\n{:?}", p.sql, own.plan, bq.plan);
        if let Some(e) = self.param_entry(&p.sql, &oids) {
            e.values_ok = Some(same);
            e.kept = kept.filter(|_| same);
        }
        self.param_plans.from_analysis += u64::from(same);
        Some(Arc::new(own))
    }
}

/// Whether every parameter of `stmt` (planned as `planned`) is one whose value can't change
/// its plan (`candidates`), with no sub-select or window function.
fn params_free(s: &Session, stmt: &Node, planned: &BoundQuery) -> bool {
    let Some((cat, table)) = planned_table(s, planned) else { return false };
    let Some(cands) = candidates(stmt, &cat, &table) else { return false };
    let mut ok = true;
    crate::sql::nodewalk::node(&mut stmt.clone(), &mut |n| match &n.node {
        Some(NodeEnum::SubLink(_)) => ok = false,
        Some(NodeEnum::FuncCall(f)) if f.over.is_some() => ok = false,
        Some(NodeEnum::ParamRef(r)) => ok &= cands.contains(&Slot::Param(r.number)),
        _ => {}
    });
    ok
}

/// `a` planned, with `a` itself when planning it again gives what analyzing and planning it
/// again would (`Analyzed::replannable`).
pub(super) fn analyzed_bound(a: Analyzed, ctx: &crate::exec::ExecCtx) -> crate::error::Result<(BoundQuery, Option<Arc<Analyzed>>)> {
    if a.replannable() {
        Ok((a.plan(ctx)?, Some(Arc::new(a))))
    } else {
        Ok((a.into_bound(ctx)?, None))
    }
}

/// Whether `generic` with `vals` for its parameters is the plan `planned` is.
pub(super) fn same_plan(planned: &BoundQuery, generic: &BoundQuery, types: &[DataType], vals: &[Value]) -> bool {
    if generic.hidden_rtes != planned.hidden_rtes || format!("{:?}", generic.cols) != format!("{:?}", planned.cols) {
        return false;
    }
    let mut g = format!("{:?}", generic.plan);
    for (i, (t, v)) in types.iter().zip(vals).enumerate() {
        let param = format!("{:?}", Expr::Param(i, t.clone()));
        g = g.replace(&param, &format!("{:?}", Expr::Const(v.clone(), t.clone())));
    }
    g == format!("{:?}", planned.plan)
}

/// The one plain table a planned statement reads or writes (its only relation, with no
/// rules, row security, inheritance or partitions).
fn planned_table(s: &Session, planned: &BoundQuery) -> Option<(Arc<Cat>, Arc<crate::catalog::Table>)> {
    let [rel] = &planned.perms.rels[..] else { return None };
    if rel.as_user.is_some() {
        return None;
    }
    let cat = s.txn.as_ref()?.borrow().catalog();
    let t = cat.table(rel.oid)?.clone();
    let plain = t.oid >= crate::catalog::FIRST_USER_OID
        && !t.rls
        && t.inherits.is_empty()
        && t.partkey.is_none()
        && t.partbound.is_none()
        && t.foreign.is_none()
        && cat.children(t.oid).is_empty()
        && !cat.class_has_subclass.contains(&t.oid)
        && !cat.class_has_rules.contains(&t.oid);
    plain.then_some((cat, t))
}

/// The literals and parameters of `stmt` (on `table`) whose values may be parameters of a
/// plan kept for every value (see above); None when the statement keeps no plan.
pub(crate) fn candidates(stmt: &Node, cat: &Cat, table: &crate::catalog::Table) -> Option<Vec<Slot>> {
    let mut out = Vec::new();
    let rel = match stmt.node.as_ref()? {
        NodeEnum::UpdateStmt(u) => {
            if !u.from_clause.is_empty() || u.with_clause.is_some() {
                return None;
            }
            for t in &u.target_list {
                let Some(NodeEnum::ResTarget(r)) = &t.node else { return None };
                if !r.indirection.is_empty() {
                    return None;
                }
                stored(r.val.as_deref()?, &mut out);
            }
            quals(u.where_clause.as_deref()?, u.relation.as_ref()?, cat, table, &mut out)?;
            u.relation.as_ref()?
        }
        NodeEnum::DeleteStmt(d) => {
            if !d.using_clause.is_empty() || d.with_clause.is_some() {
                return None;
            }
            quals(d.where_clause.as_deref()?, d.relation.as_ref()?, cat, table, &mut out)?;
            d.relation.as_ref()?
        }
        NodeEnum::InsertStmt(i) => {
            if i.on_conflict_clause.is_some() || i.with_clause.is_some() {
                return None;
            }
            let Some(NodeEnum::SelectStmt(s)) = i.select_stmt.as_deref()?.node.as_ref() else { return None };
            let [row] = &s.values_lists[..] else { return None };
            let Some(NodeEnum::List(row)) = &row.node else { return None };
            for v in &row.items {
                out.extend(v.node.as_ref().and_then(slot));
            }
            i.relation.as_ref()?
        }
        NodeEnum::SelectStmt(s) => {
            let plain = s.distinct_clause.is_empty()
                && s.into_clause.is_none()
                && s.group_clause.is_empty()
                && s.having_clause.is_none()
                && s.window_clause.is_empty()
                && s.values_lists.is_empty()
                && s.sort_clause.is_empty()
                && s.limit_offset.is_none()
                && s.limit_count.is_none()
                && s.with_clause.is_none()
                && s.op == pb::SetOperation::SetopNone as i32;
            let [from] = &s.from_clause[..] else { return None };
            let Some(NodeEnum::RangeVar(rv)) = &from.node else { return None };
            if !plain {
                return None;
            }
            quals(s.where_clause.as_deref()?, rv, cat, table, &mut out)?;
            rv
        }
        _ => return None,
    };
    (rel.relname == table.name).then_some(out)
}

/// A literal (its A_Const's location) or a parameter (its number) of a statement.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Slot {
    Const(i32),
    Param(i32),
}

fn slot(n: &NodeEnum) -> Option<Slot> {
    match n {
        NodeEnum::AConst(c) => Some(Slot::Const(c.location)),
        NodeEnum::ParamRef(p) => Some(Slot::Param(p.number)),
        _ => None,
    }
}

/// A value stored (a literal or a parameter), or combined with a column by `+`, `-` or `*`.
fn stored(n: &Node, out: &mut Vec<Slot>) {
    match n.node.as_ref() {
        Some(NodeEnum::AExpr(e)) if e.kind == pb::AExprKind::AexprOp as i32 && matches!(op_name(e), Some("+" | "-" | "*")) => {
            match (e.lexpr.as_deref().and_then(|x| x.node.as_ref()), e.rexpr.as_deref().and_then(|x| x.node.as_ref())) {
                (Some(NodeEnum::ColumnRef(_)), Some(v)) | (Some(v), Some(NodeEnum::ColumnRef(_))) => out.extend(slot(v)),
                _ => {}
            }
        }
        Some(v) => out.extend(slot(v)),
        None => {}
    }
}

fn op_name(e: &pb::AExpr) -> Option<&str> {
    let [n] = &e.name[..] else { return None };
    match &n.node {
        Some(NodeEnum::String(s)) => Some(&s.sval),
        _ => None,
    }
}

/// A WHERE that is a conjunction of `column = constant` over columns of `table` a unique
/// single-column index covers: their A_Consts.
fn quals(
    n: &Node,
    rel: &pb::RangeVar,
    cat: &Cat,
    table: &crate::catalog::Table,
    out: &mut Vec<Slot>,
) -> Option<()> {
    match n.node.as_ref()? {
        NodeEnum::BoolExpr(b) if b.boolop == pb::BoolExprType::AndExpr as i32 => {
            for a in &b.args {
                quals(a, rel, cat, table, out)?;
            }
            Some(())
        }
        NodeEnum::AExpr(e) if e.kind == pb::AExprKind::AexprOp as i32 && op_name(e) == Some("=") => {
            let (col, c) = match (e.lexpr.as_deref()?.node.as_ref()?, e.rexpr.as_deref()?.node.as_ref()?) {
                (NodeEnum::ColumnRef(r), v) | (v, NodeEnum::ColumnRef(r)) => (r, slot(v)?),
                _ => return None,
            };
            let names: Vec<&str> = col
                .fields
                .iter()
                .map(|f| match &f.node {
                    Some(NodeEnum::String(s)) => Some(s.sval.as_str()),
                    _ => None,
                })
                .collect::<Option<_>>()?;
            let name = match names[..] {
                [c] => c,
                [q, c] if q == rel.alias.as_ref().map_or(rel.relname.as_str(), |a| a.aliasname.as_str()) => c,
                _ => return None,
            };
            value_free_column(cat, table, name)?;
            out.push(c);
            Some(())
        }
        _ => None,
    }
}

/// Column `name` of `table`, when what the planner estimates for `name = value` can't depend
/// on the value: a valid unique btree index on it alone (no predicate) covers it (1 / rows),
/// or it has no most-common-values list and no extended statistics (var_eq_const's estimate
/// is then the same for every value: the fraction not null over the distinct values).
fn value_free_column(cat: &Cat, table: &crate::catalog::Table, name: &str) -> Option<()> {
    let col = table.columns.iter().position(|c| !c.dropped && c.name == name)?;
    if unique_column(cat, table, col) {
        return Some(());
    }
    if cat.statistics.contains_key(&table.oid) {
        return None;
    }
    let stats = cat.colstats.get(&table.oid);
    let mcv = stats.is_some_and(|all| {
        all.iter()
            .filter(|s| s.attnum as usize == col + 1)
            .any(|s| s.slots.iter().any(|sl| sl.kind == 1))
    }) || crate::sql::costs::col_stats(cat, table.oid, col).is_some_and(|s| s.slots.iter().any(|sl| sl.kind == 1));
    (!mcv).then_some(())
}

/// Whether a valid unique btree index on column `col` of `table` alone (no predicate)
/// covers it.
fn unique_column(cat: &Cat, table: &crate::catalog::Table, col: usize) -> bool {
    table
        .indexes
        .iter()
        .chain(&table.pkey_index)
        .filter_map(|o| cat.indexes.get(o))
        .any(|ix| {
            ix.unique
                && ix.valid
                && ix.method == "btree"
                && ix.predicate.is_none()
                && ix.columns.len() == 1
                && matches!(ix.columns[0].key, crate::catalog::IndexKey::Column(c) if c == col)
        })
}
