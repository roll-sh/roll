//! Parse with a parameter type oid no type has: where PostgreSQL's parse analysis first
//! looks the type up, and so which routine's cache miss fails the Parse, or that nothing
//! does (the parameter keeps its oid and the Bind fails, getTypeInputInfo).
//!
//! transformSelectStmt's order: the WITH clause, FROM, the target list, WHERE, HAVING, ORDER
//! BY, GROUP BY, DISTINCT, OFFSET, LIMIT, the window definitions (the WINDOW clause's, then
//! those written in OVER); the result row's TupleDescInitEntry after it all. A set
//! operation analyzes its leaves left to right, then per column the common type of its
//! sides and, unless it is UNION ALL, the column's grouping operators; then its ORDER BY and
//! LIMIT. INSERT analyzes its source, the coercions to the target columns, ON CONFLICT and
//! RETURNING; UPDATE its FROM, WHERE, RETURNING, then SET; DELETE its USING, WHERE and
//! RETURNING; MERGE its source, ON, RETURNING, then each WHEN's condition and action.
//!
//! A parameter of the missing type is looked up by:
//! - the projection check of a one-argument, undecorated, unqualified function call
//!   (ParseFuncOrColumn's ISCOMPLEX: typeOrDomainTypeRelid), and by `($n).field`;
//! - a sort, group, DISTINCT, PARTITION BY or set-operation key (get_sort_group_operators:
//!   lookup_type_cache, 42704, at the key);
//! - an ARRAY of it alone (the array type's lookup fails naming it: format_type_extended),
//!   and a named function argument (the candidates' search names it the same way);
//! - any coercion or operator (getBaseTypeAndTypmod): casts, operators, IN, BETWEEN,
//!   NULLIF, boolean contexts, CASE, a COALESCE, ARRAY, VALUES or set operation mixing it
//!   with other types, a function's arguments (at the call), LIMIT / OFFSET, a window
//!   frame's offsets, a value stored into a column (INSERT, UPDATE, MERGE).
//!
//! IS [NOT] NULL, ROW(...), a whole-row reference, EXISTS's target list, an output of a
//! sub-select or CTE nothing reads, count's argument and the arguments of a VARIADIC "any"
//! function (concat, format, ...) don't look it up; nor does a COALESCE of it alone, a
//! window function passing its argument's type on (lag, lead, first_value, last_value,
//! nth_value), or a column reference, which keep the type for what reads them.

use crate::error::{Error, code};
use crate::sql::binder::ast::{inner, location};
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
    /// A missing type (its oid): the parameters given it are of one type.
    Missing(u32),
    Other,
}

/// An output column of a query level: its name, type and where its expression starts.
#[derive(Clone)]
struct Out {
    name: String,
    ty: Ty,
    loc: i32,
}

/// A FROM item of a query level: its name and columns (None: a table's, which are never a
/// missing type).
struct Rel {
    name: String,
    cols: Option<Vec<Out>>,
}

struct Walk<'a> {
    missing: &'a [(usize, u32)],
    /// The first lookup that fails: (type oid, routine, location).
    event: Option<(u32, &'static str, i32)>,
    unmodelled: bool,
    /// Per query level, innermost last: its FROM items as far as bound.
    scope: Vec<Vec<Rel>>,
    /// Per query level: the CTEs its WITH defines, with their columns.
    ctes: Vec<Vec<(String, Vec<Out>)>>,
}

/// What analysis of `stmt` does with its parameters of missing types (`missing`: index and
/// oid, of the Parse's list).
pub(super) fn verdict(stmt: &Node, missing: &[(usize, u32)]) -> Verdict {
    let mut w = Walk { missing, event: None, unmodelled: false, scope: Vec::new(), ctes: Vec::new() };
    let cols = match inner(stmt) {
        Some(NodeEnum::SelectStmt(s)) => w.select(s),
        Some(NodeEnum::InsertStmt(s)) => w.insert(s),
        Some(NodeEnum::UpdateStmt(s)) => w.update(s),
        Some(NodeEnum::DeleteStmt(s)) => w.delete(s),
        Some(NodeEnum::MergeStmt(s)) => w.merge(s),
        _ => return Verdict::Unmodelled,
    };
    if w.event.is_none()
        && let Some(oid) = cols.iter().find_map(|c| match c.ty {
            Ty::Missing(o) => Some(o),
            Ty::Other => None,
        })
    {
        w.event = Some((oid, "TupleDescInitEntry", -1));
    }
    if w.unmodelled {
        return Verdict::Unmodelled;
    }
    match w.event {
        Some((oid, routine, loc)) => {
            let e = match routine {
                "lookup_type_cache" => {
                    Error::new(code::UNDEFINED_OBJECT, format!("type with OID {oid} does not exist")).routine(routine)
                }
                r => Error::new(code::INTERNAL_ERROR, format!("cache lookup failed for type {oid}")).routine(r),
            };
            Verdict::Fails(crate::sql::binder::ast::with_pos(e, loc))
        }
        None => Verdict::Accepted,
    }
}

const LOOKUP: &str = "getBaseTypeAndTypmod";

impl Walk<'_> {
    /// A lookup of `t`'s type by `routine` (at `loc`, -1: no position).
    fn note(&mut self, t: Ty, routine: &'static str, loc: i32) {
        if let (Ty::Missing(o), None) = (t, self.event) {
            self.event = Some((o, routine, loc));
        }
    }

    /// A query level's WITH clause: its CTEs analyzed, visible to the level by name.
    fn with(&mut self, wc: Option<&pb::WithClause>) {
        let mut defs = Vec::new();
        if let Some(wc) = wc {
            if wc.recursive {
                self.unmodelled = true;
            }
            for c in &wc.ctes {
                let Some(NodeEnum::CommonTableExpr(c)) = inner(c) else {
                    self.unmodelled = true;
                    continue;
                };
                // (A CTE is analyzed whether or not it is read; its columns are looked up
                // where they are read.)
                self.ctes.push(std::mem::take(&mut defs));
                let mut cols = match c.ctequery.as_deref().and_then(inner) {
                    Some(NodeEnum::SelectStmt(q)) => self.select(q),
                    Some(NodeEnum::InsertStmt(q)) => self.insert(q),
                    Some(NodeEnum::UpdateStmt(q)) => self.update(q),
                    Some(NodeEnum::DeleteStmt(q)) => self.delete(q),
                    Some(NodeEnum::MergeStmt(q)) => self.merge(q),
                    _ => {
                        self.unmodelled = true;
                        Vec::new()
                    }
                };
                defs = self.ctes.pop().unwrap_or_default();
                rename(&mut cols, &c.aliascolnames);
                defs.push((c.ctename.clone(), cols));
            }
        }
        self.ctes.push(defs);
    }

    /// A SELECT's output columns.
    fn select(&mut self, s: &pb::SelectStmt) -> Vec<Out> {
        crate::stack::grow(|| {
            self.with(s.with_clause.as_ref());
            let cols = if s.op != pb::SetOperation::SetopNone as i32 {
                self.setop(s)
            } else {
                self.scope.push(Vec::new());
                for f in &s.from_clause {
                    self.range_item(f);
                }
                let cols = self.select_rest(s);
                self.scope.pop();
                cols
            };
            self.ctes.pop();
            cols
        })
    }

    /// A set operation's columns (transformSetOperationStmt).
    fn setop(&mut self, s: &pb::SelectStmt) -> Vec<Out> {
        let cols = self.setop_tree(s);
        self.scope.push(Vec::new());
        self.order_by(&s.sort_clause, &cols);
        for l in [&s.limit_offset, &s.limit_count].into_iter().flatten() {
            let t = self.expr(l);
            self.note(t, LOOKUP, -1);
        }
        self.scope.pop();
        if !s.locking_clause.is_empty() || s.into_clause.is_some() {
            self.unmodelled = true;
        }
        cols
    }

    /// A set operation's tree, leaves left to right; each node's columns their sides'
    /// common types, then (but for UNION ALL) their grouping operators.
    fn setop_tree(&mut self, s: &pb::SelectStmt) -> Vec<Out> {
        if s.op == pb::SetOperation::SetopNone as i32 {
            return self.select(s);
        }
        let (Some(l), Some(r)) = (s.larg.as_deref(), s.rarg.as_deref()) else {
            self.unmodelled = true;
            return Vec::new();
        };
        let leaf = |w: &mut Self, x: &pb::SelectStmt| {
            // (A leaf with its own WITH, ORDER BY or LIMIT is a query of its own.)
            if x.op == pb::SetOperation::SetopNone as i32
                || x.with_clause.is_some()
                || !x.sort_clause.is_empty()
                || x.limit_count.is_some()
                || x.limit_offset.is_some()
            {
                w.select(x)
            } else {
                w.setop_tree(x)
            }
        };
        let left = leaf(self, l);
        let right = leaf(self, r);
        if left.len() != right.len() {
            self.unmodelled = true;
        }
        let grouped = !(s.op == pb::SetOperation::SetopUnion as i32 && s.all);
        left.into_iter()
            .zip(right)
            .map(|(a, b)| {
                let ty = if a.ty == b.ty {
                    a.ty
                } else {
                    self.note(a.ty, LOOKUP, -1);
                    self.note(b.ty, LOOKUP, -1);
                    Ty::Other
                };
                if grouped {
                    self.note(ty, "lookup_type_cache", a.loc);
                }
                Out { ty, ..a }
            })
            .collect()
    }

    /// `select` past its FROM clause.
    fn select_rest(&mut self, s: &pb::SelectStmt) -> Vec<Out> {
        if !s.values_lists.is_empty() {
            let mut cols: Vec<Out> = Vec::new();
            for row in &s.values_lists {
                let Some(NodeEnum::List(l)) = inner(row) else { continue };
                for (j, e) in l.items.iter().enumerate() {
                    let t = self.expr(e);
                    if cols.len() <= j {
                        cols.push(Out { name: format!("column{}", j + 1), ty: t, loc: location(e) });
                    } else if cols[j].ty != t {
                        // (select_common_type over different types.)
                        let prev = cols[j].ty;
                        self.note(prev, LOOKUP, -1);
                        self.note(t, LOOKUP, -1);
                        cols[j].ty = Ty::Other;
                    }
                }
            }
            return cols;
        }
        let mut cols = Vec::new();
        for t in &s.target_list {
            let Some(NodeEnum::ResTarget(r)) = inner(t) else { continue };
            let Some(v) = r.val.as_deref() else { continue };
            if let Some(NodeEnum::ColumnRef(c)) = inner(v)
                && matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))
            {
                cols.extend(self.star(&c.fields));
                continue;
            }
            let ty = self.expr(v);
            let name = match (&r.name, inner(v)) {
                (n, _) if !n.is_empty() => n.clone(),
                (_, Some(NodeEnum::ColumnRef(c))) => {
                    c.fields.last().and_then(crate::sql::binder::ast::const_str).unwrap_or_default()
                }
                _ => String::new(),
            };
            cols.push(Out { name, ty, loc: location(v) });
        }
        for q in [&s.where_clause, &s.having_clause].into_iter().flatten() {
            let t = self.expr(q);
            self.note(t, LOOKUP, -1);
        }
        self.order_by(&s.sort_clause, &cols);
        for g in &s.group_clause {
            match inner(g) {
                Some(NodeEnum::GroupingSet(_)) => self.unmodelled = true,
                _ => self.key(g, &cols),
            }
        }
        if s.distinct_clause.len() == 1 && s.distinct_clause[0].node.is_none() {
            for c in cols.clone() {
                self.note(c.ty, "lookup_type_cache", c.loc);
            }
        } else {
            for d in &s.distinct_clause {
                self.key(d, &cols);
            }
        }
        for l in [&s.limit_offset, &s.limit_count].into_iter().flatten() {
            let t = self.expr(l);
            self.note(t, LOOKUP, -1);
        }
        // transformWindowDefinitions: the WINDOW clause's, then those written in OVER.
        let mut defs: Vec<&pb::WindowDef> = Vec::new();
        for w in &s.window_clause {
            match inner(w) {
                Some(NodeEnum::WindowDef(d)) => defs.push(d),
                _ => self.unmodelled = true,
            }
        }
        for t in &s.target_list {
            if let Some(NodeEnum::ResTarget(r)) = inner(t)
                && let Some(v) = r.val.as_deref()
            {
                overs(v, &mut defs);
            }
        }
        for d in defs {
            for p in &d.partition_clause {
                self.key(p, &[]);
            }
            for o in &d.order_clause {
                match inner(o) {
                    Some(NodeEnum::SortBy(sb)) => {
                        if let Some(n) = sb.node.as_deref() {
                            self.key(n, &[]);
                        }
                    }
                    _ => self.unmodelled = true,
                }
            }
            for x in [&d.start_offset, &d.end_offset].into_iter().flatten() {
                let t = self.expr(x);
                self.note(t, LOOKUP, -1);
            }
        }
        if !s.locking_clause.is_empty() || s.into_clause.is_some() {
            self.unmodelled = true;
        }
        cols
    }

    /// ORDER BY: each key's sort operators.
    fn order_by(&mut self, sort: &[Node], cols: &[Out]) {
        for k in sort {
            match inner(k) {
                Some(NodeEnum::SortBy(sb)) => {
                    if let Some(n) = sb.node.as_deref() {
                        self.key(n, cols);
                    }
                }
                _ => self.unmodelled = true,
            }
        }
    }

    /// A sort or group key (an output column's number or name, else an expression): its
    /// operators' lookup, at the key.
    fn key(&mut self, n: &Node, cols: &[Out]) {
        let t = match inner(n) {
            Some(NodeEnum::AConst(c)) => match &c.val {
                Some(pb::a_const::Val::Ival(i)) if i.ival >= 1 => {
                    cols.get(i.ival as usize - 1).map_or(Ty::Other, |c| c.ty)
                }
                _ => Ty::Other,
            },
            Some(NodeEnum::ColumnRef(c)) if c.fields.len() == 1 => {
                let name = c.fields.first().and_then(crate::sql::binder::ast::const_str);
                match cols.iter().find(|x| Some(&x.name) == name.as_ref()) {
                    Some(x) => x.ty,
                    None => self.expr(n),
                }
            }
            _ => self.expr(n),
        };
        self.note(t, "lookup_type_cache", location(n));
    }

    /// `*` or `rel.*`: the columns of the FROM items it names.
    fn star(&mut self, fields: &[Node]) -> Vec<Out> {
        let Some(level) = self.scope.last() else { return Vec::new() };
        let rel = (fields.len() == 2).then(|| fields.first().and_then(crate::sql::binder::ast::const_str)).flatten();
        if fields.len() > 2 {
            self.unmodelled = true;
        }
        let mut out = Vec::new();
        for r in level.iter().filter(|r| rel.as_ref().is_none_or(|n| *n == r.name)) {
            match &r.cols {
                Some(c) => out.extend(c.iter().cloned()),
                None => out.push(Out { name: String::new(), ty: Ty::Other, loc: -1 }),
            }
        }
        out
    }

    /// A FROM item: its columns go to the level being bound (a LATERAL item sees those
    /// before it).
    fn range_item(&mut self, f: &Node) {
        let rel = match inner(f) {
            Some(NodeEnum::RangeVar(v)) => {
                let cte = (v.schemaname.is_empty())
                    .then(|| self.ctes.iter().rev().flat_map(|l| l.iter().rev()).find(|(n, _)| *n == v.relname))
                    .flatten()
                    .map(|(_, c)| c.clone());
                let name = v.alias.as_ref().map_or_else(|| v.relname.clone(), |a| a.aliasname.clone());
                let mut cols = cte;
                if let (Some(c), Some(a)) = (&mut cols, &v.alias) {
                    rename(c, &a.colnames);
                }
                Rel { name, cols }
            }
            Some(NodeEnum::RangeSubselect(r)) => {
                let mut cols = match r.subquery.as_deref().and_then(inner) {
                    Some(NodeEnum::SelectStmt(q)) => self.select(q),
                    _ => {
                        self.unmodelled = true;
                        Vec::new()
                    }
                };
                if let Some(a) = &r.alias {
                    rename(&mut cols, &a.colnames);
                }
                Rel { name: r.alias.as_ref().map(|a| a.aliasname.clone()).unwrap_or_default(), cols: Some(cols) }
            }
            Some(NodeEnum::RangeFunction(r)) => {
                for f in &r.functions {
                    if let Some(NodeEnum::List(l)) = inner(f)
                        && let Some(call) = l.items.first()
                    {
                        self.expr(call);
                    }
                }
                Rel { name: r.alias.as_ref().map(|a| a.aliasname.clone()).unwrap_or_default(), cols: None }
            }
            Some(NodeEnum::JoinExpr(j)) => {
                let start = self.scope.last().map_or(0, Vec::len);
                for a in [&j.larg, &j.rarg].into_iter().flatten() {
                    self.range_item(a);
                }
                let level = self.scope.last_mut().map(|l| l.split_off(start)).unwrap_or_default();
                let missing_cols = level.iter().any(|r| r.cols.as_ref().is_some_and(|c| c.iter().any(|x| x.ty != Ty::Other)));
                if missing_cols && (j.is_natural || j.alias.is_some()) {
                    self.unmodelled = true;
                }
                // (USING compares the sides' columns of each name: a coercion to their common
                // type.)
                for u in &j.using_clause {
                    let Some(name) = crate::sql::binder::ast::const_str(u) else { continue };
                    let tys: Vec<Ty> = level
                        .iter()
                        .filter_map(|r| r.cols.as_ref()?.iter().find(|c| c.name == name).map(|c| c.ty))
                        .collect();
                    match tys.as_slice() {
                        [a, b] if a == b && *a != Ty::Other => self.unmodelled = true,
                        _ => {
                            for t in tys {
                                self.note(t, LOOKUP, -1);
                            }
                        }
                    }
                }
                if let Some(l) = self.scope.last_mut() {
                    l.extend(level);
                }
                if let Some(q) = &j.quals {
                    let t = self.expr(q);
                    self.note(t, LOOKUP, -1);
                }
                return;
            }
            _ => {
                self.unmodelled = true;
                return;
            }
        };
        if let Some(l) = self.scope.last_mut() {
            l.push(rel);
        }
    }

    fn param(&self, n: i32) -> Ty {
        let i = n as usize - 1;
        self.missing.iter().find(|m| m.0 == i).map_or(Ty::Other, |m| Ty::Missing(m.1))
    }

    /// A column reference's type: a FROM item's column, innermost level first (a table's
    /// columns, a whole-row reference and anything not found aren't a missing type).
    fn column(&self, c: &pb::ColumnRef) -> Ty {
        let names: Vec<Option<String>> = c.fields.iter().map(crate::sql::binder::ast::const_str).collect();
        let (rel, col) = match names.as_slice() {
            [Some(c)] => (None, c),
            [Some(r), Some(c)] => (Some(r), c),
            _ => return Ty::Other,
        };
        for level in self.scope.iter().rev() {
            for r in level.iter().filter(|r| rel.is_none_or(|n| *n == r.name)) {
                if let Some(x) = r.cols.as_ref().and_then(|cs| cs.iter().find(|x| x.name == *col)) {
                    return x.ty;
                }
            }
            if rel.is_some_and(|n| level.iter().any(|r| r.name == *n)) {
                return Ty::Other;
            }
        }
        Ty::Other
    }

    /// An expression's type; the lookups analyzing it makes go to `event`.
    fn expr(&mut self, n: &Node) -> Ty {
        crate::stack::grow(|| self.expr_here(n))
    }

    fn expr_here(&mut self, n: &Node) -> Ty {
        let Some(e) = inner(n) else { return Ty::Other };
        match e {
            NodeEnum::ParamRef(p) => self.param(p.number),
            NodeEnum::ColumnRef(c) => self.column(c),
            NodeEnum::AConst(_) | NodeEnum::SqlvalueFunction(_) | NodeEnum::SetToDefault(_) => Ty::Other,
            NodeEnum::TypeCast(c) => {
                let t = c.arg.as_deref().map_or(Ty::Other, |a| self.expr(a));
                self.note(t, LOOKUP, -1);
                Ty::Other
            }
            NodeEnum::AExpr(a) => {
                for x in [&a.lexpr, &a.rexpr].into_iter().flatten() {
                    let t = match inner(x) {
                        Some(NodeEnum::List(l)) => {
                            for i in &l.items {
                                let t = self.expr(i);
                                self.note(t, LOOKUP, -1);
                            }
                            Ty::Other
                        }
                        _ => self.expr(x),
                    };
                    self.note(t, LOOKUP, -1);
                }
                Ty::Other
            }
            NodeEnum::BoolExpr(b) => {
                for a in &b.args {
                    let t = self.expr(a);
                    self.note(t, LOOKUP, -1);
                }
                Ty::Other
            }
            NodeEnum::BooleanTest(b) => {
                let t = b.arg.as_deref().map_or(Ty::Other, |a| self.expr(a));
                self.note(t, LOOKUP, -1);
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
                            self.note(t, LOOKUP, -1);
                        }
                        Ty::Other
                    }
                }
            }
            NodeEnum::CaseExpr(c) => {
                if let Some(a) = c.arg.as_deref() {
                    let t = self.expr(a);
                    self.note(t, LOOKUP, -1);
                }
                let mut results = Vec::new();
                for w in &c.args {
                    let Some(NodeEnum::CaseWhen(w)) = inner(w) else { continue };
                    if let Some(x) = w.expr.as_deref() {
                        let t = self.expr(x);
                        self.note(t, LOOKUP, -1);
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
                    self.note(t, LOOKUP, -1);
                }
                Ty::Other
            }
            NodeEnum::AArrayExpr(a) => {
                let ts: Vec<Ty> = a.elements.iter().map(|x| self.expr(x)).collect();
                match ts.first() {
                    Some(first) if ts.iter().all(|t| t == first) => self.note(*first, "format_type_extended", -1),
                    _ => {
                        for t in ts {
                            self.note(t, LOOKUP, -1);
                        }
                    }
                }
                Ty::Other
            }
            NodeEnum::AIndirection(a) => {
                let t = a.arg.as_deref().map_or(Ty::Other, |x| self.expr(x));
                self.note(t, "typeOrDomainTypeRelid", -1);
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
                let first = cols.first().map_or(Ty::Other, |c| c.ty);
                match pb::SubLinkType::try_from(l.sub_link_type) {
                    Ok(pb::SubLinkType::ExistsSublink) => Ty::Other,
                    Ok(pb::SubLinkType::ExprSublink) => first,
                    Ok(pb::SubLinkType::AnySublink | pb::SubLinkType::AllSublink) => {
                        self.note(test.unwrap_or(Ty::Other), LOOKUP, -1);
                        self.note(first, LOOKUP, -1);
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
        // (Window functions whose first argument's type is their result's.)
        const PASS_FIRST: &[&str] = &["lag", "lead", "first_value", "last_value", "nth_value"];
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
                self.note(t, "lookup_type_cache", location(n));
            }
        }
        if let Some(x) = f.agg_filter.as_deref() {
            let t = self.expr(x);
            self.note(t, LOOKUP, -1);
        }
        let name = crate::sql::binder::ast::names(&f.funcname);
        let bare = |n: &[&str]| name.len() == 1 && n.contains(&name[0].as_str());
        if f.over.is_some() {
            if !named.is_empty() {
                self.unmodelled = true;
            }
            if bare(&["count"]) {
                return Ty::Other;
            }
            if bare(PASS_FIRST) && !ts.is_empty() {
                for &t in &ts[1..] {
                    self.note(t, LOOKUP, f.location);
                }
                return ts[0];
            }
            for t in ts {
                self.note(t, LOOKUP, f.location);
            }
            return Ty::Other;
        }
        // ParseFuncOrColumn's check for a column projection `f(x)` = `x.f`.
        let plain = ts.len() == 1
            && named.is_empty()
            && f.agg_order.is_empty()
            && f.agg_filter.is_none()
            && !f.agg_star
            && !f.agg_distinct
            && !f.func_variadic
            && name.len() == 1;
        if plain {
            self.note(ts[0], "typeOrDomainTypeRelid", -1);
        } else if !named.is_empty() {
            for t in named {
                self.note(t, "format_type_extended", -1);
            }
        } else if !bare(VARIADIC_ANY) {
            for t in ts {
                self.note(t, LOOKUP, f.location);
            }
        }
        Ty::Other
    }

    /// RETURNING: its expressions, the statement's result row.
    fn returning(&mut self, r: Option<&pb::ReturningClause>) -> Vec<Out> {
        let mut out = Vec::new();
        for t in r.map_or(&[][..], |r| &r.exprs[..]) {
            let Some(NodeEnum::ResTarget(r)) = inner(t) else { continue };
            let Some(v) = r.val.as_deref() else { continue };
            if let Some(NodeEnum::ColumnRef(c)) = inner(v)
                && matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))
            {
                out.extend(self.star(&c.fields));
                continue;
            }
            let ty = self.expr(v);
            out.push(Out { name: r.name.clone(), ty, loc: location(v) });
        }
        out
    }

    /// A target relation: its columns are a table's.
    fn target(&mut self, r: Option<&pb::RangeVar>) {
        let name = r.map(|r| r.alias.as_ref().map_or_else(|| r.relname.clone(), |a| a.aliasname.clone())).unwrap_or_default();
        if let Some(l) = self.scope.last_mut() {
            l.push(Rel { name, cols: None });
        }
    }

    /// A value stored into a target column: coerced to its type.
    fn stored(&mut self, n: &Node) {
        let t = match inner(n) {
            Some(NodeEnum::MultiAssignRef(m)) => match m.source.as_deref().and_then(inner) {
                Some(NodeEnum::RowExpr(r)) => match r.args.get(m.colno as usize - 1) {
                    Some(a) if m.colno == 1 => {
                        // (The row's other values: each its own column's, after this one.)
                        let t = self.expr(a);
                        self.note(t, LOOKUP, -1);
                        for b in &r.args[1..] {
                            let t = self.expr(b);
                            self.note(t, LOOKUP, -1);
                        }
                        return;
                    }
                    _ => return,
                },
                _ => {
                    self.unmodelled = true;
                    return;
                }
            },
            _ => self.expr(n),
        };
        self.note(t, LOOKUP, -1);
    }

    fn insert(&mut self, s: &pb::InsertStmt) -> Vec<Out> {
        self.with(s.with_clause.as_ref());
        self.scope.push(Vec::new());
        match s.select_stmt.as_deref().and_then(inner) {
            None => {}
            // (A VALUES list: each row's values, coerced to the columns.)
            Some(NodeEnum::SelectStmt(q))
                if !q.values_lists.is_empty()
                    && q.op == pb::SetOperation::SetopNone as i32
                    && q.with_clause.is_none()
                    && q.sort_clause.is_empty()
                    && q.limit_count.is_none()
                    && q.limit_offset.is_none() =>
            {
                if q.values_lists.len() == 1 {
                    if let Some(NodeEnum::List(l)) = inner(&q.values_lists[0]) {
                        for e in &l.items {
                            self.stored(e);
                        }
                    }
                } else {
                    for c in self.select_rest(q) {
                        self.note(c.ty, LOOKUP, -1);
                    }
                }
            }
            Some(NodeEnum::SelectStmt(q)) => {
                for c in self.select(q) {
                    self.note(c.ty, LOOKUP, -1);
                }
            }
            _ => self.unmodelled = true,
        }
        self.target(s.relation.as_ref());
        if let Some(oc) = s.on_conflict_clause.as_deref() {
            if let Some(i) = oc.infer.as_deref()
                && i.where_clause.is_some()
            {
                self.unmodelled = true;
            }
            for t in &oc.target_list {
                if let Some(NodeEnum::ResTarget(r)) = inner(t)
                    && let Some(v) = r.val.as_deref()
                {
                    self.stored(v);
                }
            }
            if let Some(q) = oc.where_clause.as_deref() {
                let t = self.expr(q);
                self.note(t, LOOKUP, -1);
            }
        }
        let out = self.returning(s.returning_clause.as_ref());
        self.scope.pop();
        self.ctes.pop();
        out
    }

    fn update(&mut self, s: &pb::UpdateStmt) -> Vec<Out> {
        self.with(s.with_clause.as_ref());
        self.scope.push(Vec::new());
        self.target(s.relation.as_ref());
        for f in &s.from_clause {
            self.range_item(f);
        }
        if let Some(q) = s.where_clause.as_deref() {
            let t = self.expr(q);
            self.note(t, LOOKUP, -1);
        }
        let out = self.returning(s.returning_clause.as_ref());
        for t in &s.target_list {
            if let Some(NodeEnum::ResTarget(r)) = inner(t)
                && let Some(v) = r.val.as_deref()
            {
                if !r.indirection.is_empty() {
                    self.unmodelled = true;
                }
                self.stored(v);
            }
        }
        self.scope.pop();
        self.ctes.pop();
        out
    }

    fn delete(&mut self, s: &pb::DeleteStmt) -> Vec<Out> {
        self.with(s.with_clause.as_ref());
        self.scope.push(Vec::new());
        self.target(s.relation.as_ref());
        for f in &s.using_clause {
            self.range_item(f);
        }
        if let Some(q) = s.where_clause.as_deref() {
            let t = self.expr(q);
            self.note(t, LOOKUP, -1);
        }
        let out = self.returning(s.returning_clause.as_ref());
        self.scope.pop();
        self.ctes.pop();
        out
    }

    fn merge(&mut self, s: &pb::MergeStmt) -> Vec<Out> {
        self.with(s.with_clause.as_ref());
        self.scope.push(Vec::new());
        if let Some(src) = s.source_relation.as_deref() {
            self.range_item(src);
        }
        self.target(s.relation.as_ref());
        if let Some(q) = s.join_condition.as_deref() {
            let t = self.expr(q);
            self.note(t, LOOKUP, -1);
        }
        let out = self.returning(s.returning_clause.as_ref());
        for w in &s.merge_when_clauses {
            let Some(NodeEnum::MergeWhenClause(w)) = inner(w) else {
                self.unmodelled = true;
                continue;
            };
            if let Some(c) = w.condition.as_deref() {
                let t = self.expr(c);
                self.note(t, LOOKUP, -1);
            }
            for t in &w.target_list {
                if let Some(NodeEnum::ResTarget(r)) = inner(t)
                    && let Some(v) = r.val.as_deref()
                {
                    self.stored(v);
                }
            }
            for v in &w.values {
                self.stored(v);
            }
        }
        self.scope.pop();
        self.ctes.pop();
        out
    }
}

/// The OVER clauses written in an expression's window function calls, in order.
fn overs<'a>(n: &'a Node, out: &mut Vec<&'a pb::WindowDef>) {
    let Some(e) = inner(n) else { return };
    if let NodeEnum::FuncCall(f) = e {
        for a in &f.args {
            overs(a, out);
        }
        if let Some(w) = f.over.as_deref()
            && w.name.is_empty()
            && w.refname.is_empty()
        {
            out.push(w);
        }
        return;
    }
    if let NodeEnum::TypeCast(c) = e
        && let Some(a) = c.arg.as_deref()
    {
        overs(a, out);
    }
    if let NodeEnum::AExpr(a) = e {
        for x in [&a.lexpr, &a.rexpr].into_iter().flatten() {
            overs(x, out);
        }
    }
}

/// Column aliases over a query's columns.
fn rename(cols: &mut [Out], names: &[Node]) {
    for (c, n) in cols.iter_mut().zip(names) {
        if let Some(s) = crate::sql::binder::ast::const_str(n) {
            c.name = s;
        }
    }
}
