//! Parse with a parameter type oid no type has: where PostgreSQL's parse analysis first
//! looks the type up, and so which routine's cache miss fails the Parse, or that nothing
//! does (the parameter keeps its oid and the Bind fails, getTypeInputInfo).
//!
//! transformSelectStmt's order: the WITH clause, FROM, the target list, WHERE, HAVING, ORDER
//! BY, GROUP BY, DISTINCT, OFFSET, LIMIT; the result row's TupleDescInitEntry after it all.
//! A parameter of the missing type is looked up by:
//! - the projection check of a one-argument, undecorated, unqualified function call
//!   (ParseFuncOrColumn's ISCOMPLEX: typeOrDomainTypeRelid), and by `($n).field`;
//! - a sort or group key (get_sort_group_operators: lookup_type_cache, 42704);
//! - an ARRAY of it alone (the array type's lookup fails naming it: format_type_extended),
//!   and a named function argument (the candidates' search names it the same way);
//! - any coercion or operator (getBaseTypeAndTypmod): casts, operators, IN, BETWEEN,
//!   NULLIF, boolean contexts, CASE, a COALESCE or ARRAY mixing it with other types, a
//!   function's other arguments, LIMIT / OFFSET.
//!
//! IS [NOT] NULL, ROW(...), EXISTS's target list, a CTE nothing reads and the arguments of
//! a VARIADIC "any" function (concat, format, ...) don't look it up; nor does a COALESCE of
//! it alone, which keeps its type for what reads it.

use crate::error::{Error, code};
use crate::sql::binder::ast::inner;
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};

/// What analysis does with the statement's parameters of missing types.
pub(super) enum Verdict {
    /// The first lookup fails: the Parse's error.
    Fails(Error),
    /// Nothing looks them up.
    Accepted,
    /// A statement or expression this model doesn't follow.
    Unmodelled,
}

/// An expression's type, as far as the model needs it.
#[derive(Clone, Copy, PartialEq)]
enum Ty {
    /// The type of missing parameter `i` (0-based).
    Missing(usize),
    Other,
}

struct Walk<'a> {
    missing: &'a [(usize, u32)],
    /// The first lookup that fails: (parameter, routine).
    event: Option<(usize, &'static str)>,
    unmodelled: bool,
    /// Per query level: its FROM has a column of a missing type (a column reference's type
    /// is then more than this model follows).
    scope: Vec<bool>,
}

/// What analysis of `stmt` does with its parameters of missing types (`missing`: index and
/// oid, of the Parse's list).
pub(super) fn verdict(stmt: &Node, missing: &[(usize, u32)]) -> Verdict {
    let Some(NodeEnum::SelectStmt(s)) = inner(stmt) else {
        return Verdict::Unmodelled;
    };
    let mut w = Walk { missing, event: None, unmodelled: false, scope: Vec::new() };
    let cols = w.select(s);
    if w.event.is_none()
        && let Some(i) = cols.iter().find_map(|t| match t {
            Ty::Missing(i) => Some(*i),
            Ty::Other => None,
        })
    {
        w.event = Some((i, "TupleDescInitEntry"));
    }
    if w.unmodelled {
        return Verdict::Unmodelled;
    }
    match w.event {
        Some((i, routine)) => {
            let oid = missing.iter().find(|m| m.0 == i).map_or(0, |m| m.1);
            Verdict::Fails(match routine {
                "lookup_type_cache" => {
                    Error::new(code::UNDEFINED_OBJECT, format!("type with OID {oid} does not exist")).routine(routine)
                }
                r => Error::new(code::INTERNAL_ERROR, format!("cache lookup failed for type {oid}")).routine(r),
            })
        }
        None => Verdict::Accepted,
    }
}

impl Walk<'_> {
    fn note(&mut self, t: Ty, routine: &'static str) {
        if let (Ty::Missing(i), None) = (t, self.event) {
            self.event = Some((i, routine));
        }
    }

    /// A SELECT's output columns.
    fn select(&mut self, s: &pb::SelectStmt) -> Vec<Ty> {
        if s.op != pb::SetOperation::SetopNone as i32 || !s.window_clause.is_empty() {
            self.unmodelled = true;
            return Vec::new();
        }
        // (A CTE is analyzed whether or not it is read; its columns are looked up only where
        // they are read, which this model doesn't follow.)
        if let Some(wc) = &s.with_clause {
            for c in &wc.ctes {
                if let Some(NodeEnum::CommonTableExpr(c)) = inner(c)
                    && let Some(q) = c.ctequery.as_deref()
                {
                    match inner(q) {
                        Some(NodeEnum::SelectStmt(q)) => {
                            self.select(q);
                        }
                        _ => self.unmodelled = true,
                    }
                }
            }
        }
        // FROM: a sub-select's columns as `*` reads them.
        let mut from = Vec::new();
        for f in &s.from_clause {
            self.range_item(f, &mut from);
        }
        self.scope.push(from.iter().any(|t| matches!(t, Ty::Missing(_))));
        let cols = self.select_rest(s, &from);
        self.scope.pop();
        cols
    }

    /// `select` past its FROM clause (`from`: its columns).
    fn select_rest(&mut self, s: &pb::SelectStmt, from: &[Ty]) -> Vec<Ty> {
        if !s.values_lists.is_empty() {
            let mut cols: Vec<Option<Ty>> = Vec::new();
            for row in &s.values_lists {
                let Some(NodeEnum::List(l)) = inner(row) else { continue };
                for (j, e) in l.items.iter().enumerate() {
                    let t = self.expr(e);
                    if cols.len() <= j {
                        cols.push(Some(t));
                    } else if cols[j] != Some(t) {
                        // (select_common_type over different types.)
                        let prev = cols[j].unwrap_or(Ty::Other);
                        self.note(prev, "getBaseTypeAndTypmod");
                        self.note(t, "getBaseTypeAndTypmod");
                        cols[j] = Some(Ty::Other);
                    }
                }
            }
            return cols.into_iter().map(|t| t.unwrap_or(Ty::Other)).collect();
        }
        let mut cols = Vec::new();
        let mut names = Vec::new();
        for t in &s.target_list {
            let Some(NodeEnum::ResTarget(r)) = inner(t) else { continue };
            let Some(v) = r.val.as_deref() else { continue };
            if let Some(NodeEnum::ColumnRef(c)) = inner(v)
                && matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))
            {
                cols.extend(from.iter().copied());
                names.extend(from.iter().map(|_| String::new()));
                continue;
            }
            cols.push(self.expr(v));
            names.push(r.name.clone());
        }
        for q in [&s.where_clause, &s.having_clause].into_iter().flatten() {
            let t = self.expr(q);
            self.note(t, "getBaseTypeAndTypmod");
        }
        // ORDER BY, GROUP BY, DISTINCT ON: a key's sort / group operators.
        let key = |w: &mut Self, n: &Node, cols: &[Ty]| {
            let t = match inner(n) {
                Some(NodeEnum::AConst(c)) => match &c.val {
                    Some(pb::a_const::Val::Ival(i)) if i.ival >= 1 => {
                        cols.get(i.ival as usize - 1).copied().unwrap_or(Ty::Other)
                    }
                    _ => Ty::Other,
                },
                Some(NodeEnum::ColumnRef(c)) if c.fields.len() == 1 => {
                    let name = c.fields.first().and_then(crate::sql::binder::ast::const_str);
                    match names.iter().position(|x| Some(x) == name.as_ref()) {
                        Some(j) => cols[j],
                        None => w.expr(n),
                    }
                }
                _ => w.expr(n),
            };
            w.note(t, "lookup_type_cache");
        };
        for k in &s.sort_clause {
            match inner(k) {
                Some(NodeEnum::SortBy(sb)) => {
                    if let Some(n) = sb.node.as_deref() {
                        key(self, n, &cols);
                    }
                }
                _ => self.unmodelled = true,
            }
        }
        for g in &s.group_clause {
            match inner(g) {
                Some(NodeEnum::GroupingSet(_)) => self.unmodelled = true,
                _ => key(self, g, &cols),
            }
        }
        if s.distinct_clause.len() == 1 && s.distinct_clause[0].node.is_none() {
            for t in cols.clone() {
                self.note(t, "lookup_type_cache");
            }
        } else {
            for d in &s.distinct_clause {
                key(self, d, &cols);
            }
        }
        for l in [&s.limit_offset, &s.limit_count].into_iter().flatten() {
            let t = self.expr(l);
            self.note(t, "getBaseTypeAndTypmod");
        }
        if !s.locking_clause.is_empty() || s.into_clause.is_some() {
            self.unmodelled = true;
        }
        cols
    }

    fn range_item(&mut self, f: &Node, cols: &mut Vec<Ty>) {
        match inner(f) {
            Some(NodeEnum::RangeVar(_)) => cols.push(Ty::Other),
            Some(NodeEnum::RangeSubselect(r)) => match r.subquery.as_deref().and_then(inner) {
                Some(NodeEnum::SelectStmt(q)) => cols.extend(self.select(q)),
                _ => self.unmodelled = true,
            },
            Some(NodeEnum::RangeFunction(r)) => {
                for f in &r.functions {
                    if let Some(NodeEnum::List(l)) = inner(f)
                        && let Some(call) = l.items.first()
                    {
                        self.expr(call);
                    }
                }
                cols.push(Ty::Other);
            }
            Some(NodeEnum::JoinExpr(j)) => {
                for a in [&j.larg, &j.rarg].into_iter().flatten() {
                    self.range_item(a, cols);
                }
                if let Some(q) = &j.quals {
                    let t = self.expr(q);
                    self.note(t, "getBaseTypeAndTypmod");
                }
            }
            _ => self.unmodelled = true,
        }
    }

    fn param(&self, n: i32) -> Ty {
        let i = n as usize - 1;
        if self.missing.iter().any(|m| m.0 == i) { Ty::Missing(i) } else { Ty::Other }
    }

    /// An expression's type; the lookups analyzing it makes go to `event`.
    fn expr(&mut self, n: &Node) -> Ty {
        crate::stack::grow(|| self.expr_here(n))
    }

    fn expr_here(&mut self, n: &Node) -> Ty {
        const LOOKUP: &str = "getBaseTypeAndTypmod";
        let Some(e) = inner(n) else { return Ty::Other };
        match e {
            NodeEnum::ParamRef(p) => self.param(p.number),
            NodeEnum::ColumnRef(_) => {
                if self.scope.last() == Some(&true) {
                    self.unmodelled = true;
                }
                Ty::Other
            }
            NodeEnum::AConst(_) | NodeEnum::SqlvalueFunction(_) => Ty::Other,
            NodeEnum::TypeCast(c) => {
                let t = c.arg.as_deref().map_or(Ty::Other, |a| self.expr(a));
                self.note(t, LOOKUP);
                Ty::Other
            }
            NodeEnum::AExpr(a) => {
                for x in [&a.lexpr, &a.rexpr].into_iter().flatten() {
                    let t = match inner(x) {
                        Some(NodeEnum::List(l)) => {
                            for i in &l.items {
                                let t = self.expr(i);
                                self.note(t, LOOKUP);
                            }
                            Ty::Other
                        }
                        _ => self.expr(x),
                    };
                    self.note(t, LOOKUP);
                }
                Ty::Other
            }
            NodeEnum::BoolExpr(b) => {
                for a in &b.args {
                    let t = self.expr(a);
                    self.note(t, LOOKUP);
                }
                Ty::Other
            }
            NodeEnum::BooleanTest(b) => {
                let t = b.arg.as_deref().map_or(Ty::Other, |a| self.expr(a));
                self.note(t, LOOKUP);
                Ty::Other
            }
            NodeEnum::NullTest(t) => {
                if let Some(a) = t.arg.as_deref() {
                    self.expr(a);
                }
                Ty::Other
            }
            NodeEnum::RowExpr(r) => {
                for a in &r.args {
                    self.expr(a);
                }
                Ty::Other
            }
            NodeEnum::CoalesceExpr(_) | NodeEnum::MinMaxExpr(_) => {
                let args = match e {
                    NodeEnum::CoalesceExpr(c) => &c.args,
                    NodeEnum::MinMaxExpr(m) => &m.args,
                    _ => unreachable!(),
                };
                let ts: Vec<Ty> = args.iter().map(|a| self.expr(a)).collect();
                match ts.first() {
                    Some(first) if ts.iter().all(|t| t == first) => *first,
                    _ => {
                        for t in ts {
                            self.note(t, LOOKUP);
                        }
                        Ty::Other
                    }
                }
            }
            NodeEnum::CaseExpr(c) => {
                if let Some(a) = c.arg.as_deref() {
                    let t = self.expr(a);
                    self.note(t, LOOKUP);
                }
                let mut results = Vec::new();
                for w in &c.args {
                    let Some(NodeEnum::CaseWhen(w)) = inner(w) else { continue };
                    if let Some(x) = w.expr.as_deref() {
                        let t = self.expr(x);
                        self.note(t, LOOKUP);
                    }
                    if let Some(r) = w.result.as_deref() {
                        results.push(self.expr(r));
                    }
                }
                if let Some(d) = c.defresult.as_deref() {
                    results.push(self.expr(d));
                }
                // (The results' common type, with the ELSE NULL's unknown when there is none.)
                for t in results {
                    self.note(t, LOOKUP);
                }
                Ty::Other
            }
            NodeEnum::AArrayExpr(a) => {
                let ts: Vec<Ty> = a.elements.iter().map(|x| self.expr(x)).collect();
                match ts.first() {
                    Some(first) if ts.iter().all(|t| t == first) => self.note(*first, "format_type_extended"),
                    _ => {
                        for t in ts {
                            self.note(t, LOOKUP);
                        }
                    }
                }
                Ty::Other
            }
            NodeEnum::AIndirection(a) => {
                let t = a.arg.as_deref().map_or(Ty::Other, |x| self.expr(x));
                self.note(t, "typeOrDomainTypeRelid");
                Ty::Other
            }
            NodeEnum::SubLink(l) => {
                let q = match l.subselect.as_deref().and_then(inner) {
                    Some(NodeEnum::SelectStmt(q)) => q,
                    _ => {
                        self.unmodelled = true;
                        return Ty::Other;
                    }
                };
                let test = l.testexpr.as_deref().map(|t| self.expr(t));
                let cols = self.select(q);
                match pb::SubLinkType::try_from(l.sub_link_type) {
                    Ok(pb::SubLinkType::ExistsSublink) => Ty::Other,
                    Ok(pb::SubLinkType::ExprSublink) => cols.first().copied().unwrap_or(Ty::Other),
                    Ok(pb::SubLinkType::AnySublink | pb::SubLinkType::AllSublink) => {
                        self.note(test.unwrap_or(Ty::Other), LOOKUP);
                        self.note(cols.first().copied().unwrap_or(Ty::Other), LOOKUP);
                        Ty::Other
                    }
                    _ => {
                        self.unmodelled = true;
                        Ty::Other
                    }
                }
            }
            NodeEnum::FuncCall(f) => self.call(f),
            _ => {
                self.unmodelled = true;
                Ty::Other
            }
        }
    }

    fn call(&mut self, f: &pb::FuncCall) -> Ty {
        const VARIADIC_ANY: &[&str] = &[
            "concat", "concat_ws", "format", "json_build_array", "json_build_object", "jsonb_build_array",
            "jsonb_build_object", "num_nulls", "num_nonnulls",
        ];
        let mut ts = Vec::new();
        let mut named = Vec::new();
        for a in &f.args {
            match inner(a) {
                Some(NodeEnum::NamedArgExpr(n)) => {
                    let t = n.arg.as_deref().map_or(Ty::Other, |x| self.expr(x));
                    named.push(t);
                    ts.push(t);
                }
                _ => ts.push(self.expr(a)),
            }
        }
        for o in &f.agg_order {
            if let Some(NodeEnum::SortBy(sb)) = inner(o)
                && let Some(n) = sb.node.as_deref()
            {
                let t = self.expr(n);
                self.note(t, "lookup_type_cache");
            }
        }
        if let Some(x) = f.agg_filter.as_deref() {
            let t = self.expr(x);
            self.note(t, "getBaseTypeAndTypmod");
        }
        if f.over.is_some() {
            self.unmodelled = true;
        }
        let name = crate::sql::binder::ast::names(&f.funcname);
        // ParseFuncOrColumn's check for a column projection `f(x)` = `x.f`.
        let plain = ts.len() == 1
            && named.is_empty()
            && f.agg_order.is_empty()
            && f.agg_filter.is_none()
            && f.over.is_none()
            && !f.agg_star
            && !f.agg_distinct
            && !f.func_variadic
            && name.len() == 1;
        if plain {
            self.note(ts[0], "typeOrDomainTypeRelid");
        } else if !named.is_empty() {
            for t in named {
                self.note(t, "format_type_extended");
            }
        } else if !(name.len() == 1 && VARIADIC_ANY.contains(&name[0].as_str())) {
            for t in ts {
                self.note(t, "getBaseTypeAndTypmod");
            }
        }
        Ty::Other
    }
}
