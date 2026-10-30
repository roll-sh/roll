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
use crate::sql::binder::{Binder, BoundQuery};
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

/// A session's plans of templates, by template key.
#[derive(Default)]
pub(crate) struct LitPlans {
    map: HashMap<Box<str>, KeyPlans>,
    /// Queries run with a kept plan (tests).
    uses: u64,
}

#[derive(Default)]
struct KeyPlans {
    /// Times a plan couldn't be kept for the template: it stops trying after a few.
    failures: u32,
    plans: Vec<Kept>,
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
    env: Env,
    reads: Vec<SizeRead>,
}

/// What a binding depends on besides relation sizes.
#[derive(PartialEq)]
struct Env {
    version: u64,
    settings: u64,
    path: Vec<String>,
    who: (u32, bool),
}

impl Session {
    fn lit_env(&mut self) -> Env {
        self.ensure_txn();
        let cat = self.txn.as_ref().unwrap().borrow().catalog();
        let st = self.state.settings.borrow();
        let who = (self.state.current_role(&cat), st.get_str("row_security") != Some("off"));
        Env { version: cat.version, settings: st.generation(), path: self.state.search_path(), who }
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
        if self.lit_plans.map.get(&l.scan.key)?.plans.is_empty() {
            return None;
        }
        let env = self.lit_env();
        let kp = self.lit_plans.map.get(&l.scan.key)?;
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
        let (bq, reads) = (k.bq.clone(), k.reads.clone());
        let fresh = self.with_ctx(&[], |ctx| Ok(reads.iter().all(|r| r.holds(ctx)))).unwrap_or(false);
        if fresh {
            return Some((bq, vals));
        }
        // A relation's size changed: the template's binding planned again, as the query would
        // be (its plan doesn't depend on the values, whatever the sizes).
        let k = &self.lit_plans.map.get(&l.scan.key)?.plans[at];
        let (generic, types) = (k.generic.clone(), k.params.iter().map(|p| p.2.clone()).collect());
        let (bound, reads) = self.lit_bind(&generic, types, &env.path);
        let kp = self.lit_plans.map.get_mut(&l.scan.key)?;
        match bound {
            Some(bq) => {
                let k = &mut kp.plans[at];
                k.bq = Arc::new(bq);
                k.reads = reads;
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
    fn lit_bind(&mut self, generic: &Node, types: Vec<DataType>, path: &[String]) -> (Option<BoundQuery>, Vec<SizeRead>) {
        let (bound, reads) = crate::sql::literals::recording(|| {
            self.with_ctx_path(&[], Some(path.to_vec()), |ctx| Binder::new(ctx, types).bind_statement(generic))
        });
        (bound.ok().filter(|_| !reads.contains(&SizeRead::Data)), reads)
    }

    /// After `p` (read through a template) was planned the usual way as `planned`: keep a plan
    /// of its template for the queries that follow, when one fits every value (see above).
    pub(super) fn lit_learn(&mut self, p: &Prepared, stmt: &Node, planned: &BoundQuery) {
        let Some(l) = p.lit.as_deref() else { return };
        if self.state.settings.borrow().module_loaded("auto_explain") {
            return;
        }
        // (Not the first time the session sees the template: a query sent once isn't bound
        // twice.)
        let full = self.lit_plans.map.len() >= MAX_KEYS;
        match self.lit_plans.map.get(&l.scan.key) {
            None if !full => {
                self.lit_plans.map.insert(l.scan.key.clone(), KeyPlans::default());
                return;
            }
            Some(kp) if kp.failures >= MAX_FAILURES => return,
            _ => {}
        }
        let kept = self.lit_make(l, stmt, planned);
        if self.lit_plans.map.len() >= MAX_KEYS && !self.lit_plans.map.contains_key(&l.scan.key) {
            self.lit_plans.map.clear();
        }
        let kp = self.lit_plans.map.entry(l.scan.key.clone()).or_default();
        match kept {
            Some(k) => {
                kp.plans.retain(|o| o.fixed != k.fixed);
                if kp.plans.len() >= MAX_PER_KEY {
                    kp.plans.remove(0);
                }
                kp.plans.push(k);
            }
            None => kp.failures += 1,
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
                let value = cands.contains(&c.location).then(|| l.value(k, neg)).flatten();
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
        let bq = bound?;
        if !same_plan(planned, &bq, &types, &vals) {
            return None;
        }
        let fixed = (0..l.scan.lits.len() as u32)
            .filter(|&k| kept_const[k as usize] || !params.iter().any(|p| p.0 == k))
            .map(|k| (k, Box::from(l.lit_text(k))))
            .collect();
        Some(Kept { fixed, params, generic, bq: Arc::new(bq), env, reads })
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

/// The locations of the A_Consts of `stmt` (on `table`) whose values may be parameters of
/// a plan kept for every value (see above); None when the statement keeps no plan.
fn candidates(stmt: &Node, cat: &Cat, table: &crate::catalog::Table) -> Option<Vec<i32>> {
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
                if let Some(NodeEnum::AConst(c)) = &v.node {
                    out.push(c.location);
                }
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

/// A value stored (an A_Const), or combined with a column by `+`, `-` or `*`: its A_Const.
fn stored(n: &Node, out: &mut Vec<i32>) {
    match n.node.as_ref() {
        Some(NodeEnum::AConst(c)) => out.push(c.location),
        Some(NodeEnum::AExpr(e)) if e.kind == pb::AExprKind::AexprOp as i32 && matches!(op_name(e), Some("+" | "-" | "*")) => {
            match (e.lexpr.as_deref().and_then(|x| x.node.as_ref()), e.rexpr.as_deref().and_then(|x| x.node.as_ref())) {
                (Some(NodeEnum::ColumnRef(_)), Some(NodeEnum::AConst(c)))
                | (Some(NodeEnum::AConst(c)), Some(NodeEnum::ColumnRef(_))) => out.push(c.location),
                _ => {}
            }
        }
        _ => {}
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
    out: &mut Vec<i32>,
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
                (NodeEnum::ColumnRef(r), NodeEnum::AConst(c)) | (NodeEnum::AConst(c), NodeEnum::ColumnRef(r)) => (r, c),
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
            unique_column(cat, table, name)?;
            out.push(c.location);
            Some(())
        }
        _ => None,
    }
}

/// Column `name` of `table`, when a valid unique btree index on it alone (no predicate)
/// covers it.
fn unique_column(cat: &Cat, table: &crate::catalog::Table, name: &str) -> Option<()> {
    let col = table.columns.iter().position(|c| !c.dropped && c.name == name)?;
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
        .then_some(())
}
