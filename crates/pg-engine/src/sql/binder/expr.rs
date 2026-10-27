//! Expressions: names, constants, operators, functions, aggregates, window functions,
//! subqueries.

use super::ast::{self, inner, inner_box, names, sval, with_pos};
use super::{Binder, Clause, ScopeRel};
use crate::error::{Error, Result, SqlFnAt, code};
use crate::exec::agg::agg_registry;
use crate::sql::expr::{BoolTest, Expr, Subquery, SubqueryKind, eval};
use crate::sql::func::{
    self, FuncDef, Resolved, resolve_binop, resolve_function, resolve_prefix, select_candidate,
};
use crate::sql::plan::{
    AggCall, Frame, FrameBound, FrameExclude, FrameUnit, OutCol, Plan, SortKey, SrfCall, WinFunc,
    WinKind, WindowCall,
};
use crate::types::cast::CastContext;
use crate::types::numeric::Numeric;
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};
use std::sync::Arc;

/// A field in a column reference.
enum Field<'n> {
    Name(&'n str),
    Star,
}

fn fields(c: &pb::ColumnRef) -> Vec<Field<'_>> {
    c.fields
        .iter()
        .map(|f| match inner(f) {
            Some(NodeEnum::String(s)) => Field::Name(&s.sval),
            _ => Field::Star,
        })
        .collect()
}

pub(crate) fn not_allowed(what: &str, clause: Clause) -> Error {
    match what {
        "aggregate" => Error::new(
            code::GROUPING_ERROR,
            format!("aggregate functions are not allowed in {}", clause.name()),
        ),
        "window" => Error::new(
            code::WINDOWING_ERROR,
            format!("window functions are not allowed in {}", clause.name()),
        ),
        _ => Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!(
                "set-returning functions are not allowed in {}",
                clause.name()
            ),
        ),
    }
}

thread_local! {
    /// The domain of the CASE operand a WHEN comparison's CaseTestExpr stands for.
    static CASE_TEST_DOMAIN: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// The flags not at rest (`scoped::unsettled_for_test`).
pub(super) fn unsettled(out: &mut Vec<&'static str>) {
    if CASE_TEST_DOMAIN.get().is_some() {
        out.push("expr::CASE_TEST_DOMAIN");
    }
}

impl Binder<'_, '_> {
    pub fn bind_expr(&mut self, n: &Node) -> Result<Expr> {
        if self.levels.last().is_none_or(|l| l.st.pass_marks.is_empty()) {
            return self.bind_expr_unmarked(n);
        }
        // (A grouping key's occurrence inside a select-list item or HAVING, not the whole
        // item: `SelectState::pass_marks`. Marked as `SqlSyntax(SqlSyntax(_))`, which is
        // never built otherwise and which nothing folds through. Not a bare literal, whose
        // type comes from its context (`post_agg` matches the constant it becomes), nor
        // inside aggregate arguments or a window's definition.)
        let lv = self.level();
        let mark = lv.st.pass_depth > 0
            && !lv.st.in_agg
            && !matches!(lv.clause, Clause::Window)
            && !matches!(inner(n), Some(NodeEnum::AConst(_)))
            && lv.st.pass_marks.contains(&super::select::strip_locations(&format!("{n:?}")));
        self.level().st.pass_depth += 1;
        let r = self.bind_expr_unmarked(n);
        self.level().st.pass_depth -= 1;
        let e = r?;
        Ok(if mark { Expr::SqlSyntax(Box::new(Expr::SqlSyntax(Box::new(e)))) } else { e })
    }

    fn bind_expr_unmarked(&mut self, n: &Node) -> Result<Expr> {
        // Collations are checked once per expression, when the whole of it is bound
        // (assign_query_collations), not for each subexpression again: that was quadratic
        // in the expression's depth.
        let lvl = self.levels.len();
        if self.expr_top == lvl {
            return self.bind_node(n);
        }
        let outer = std::mem::replace(&mut self.expr_top, lvl);
        let r = self.bind_node(n);
        self.expr_top = outer;
        let e = r?;
        // Different explicit collations meeting is an error (assign_collations).
        let mut explicit = 0;
        e.walk(&mut |x| explicit += matches!(x, Expr::Collate(..)) as usize);
        if explicit > 1 && !matches!(e, Expr::Collate(..)) {
            self.collation_of(&e)?;
        }
        Ok(e)
    }

    fn bind_node(&mut self, n: &Node) -> Result<Expr> {
        // (transformExprRecurse's check_stack_depth.)
        crate::stack::guard(|| self.bind_node_here(n))
    }

    fn bind_node_here(&mut self, n: &Node) -> Result<Expr> {
        let Some(e) = inner(n) else {
            return Err(crate::error::internal("empty expression node"));
        };
        match e {
            NodeEnum::ColumnRef(c) => self.bind_column_ref(c),
            NodeEnum::ParamRef(p) => self.bind_param(p.number, p.location),
            NodeEnum::AConst(c) => Ok(self.bind_const(c)?),
            NodeEnum::TypeCast(tc) => self.bind_type_cast(tc),
            NodeEnum::AExpr(a) => self.bind_aexpr(a),
            NodeEnum::BoolExpr(b) => self.bind_bool_expr(b),
            NodeEnum::NullTest(t) => {
                self.whole_var_ref = false;
                let arg = self.bind_expr(t.arg.as_deref().unwrap())?;
                if let Expr::Param(i, DataType::Unknown) = &arg {
                    self.untyped_params.push(*i);
                }
                let whole_var = std::mem::take(&mut self.whole_var_ref);
                let not = t.nulltesttype == pb::NullTestType::IsNotNull as i32;
                // Postgres's tree keeps `row IS NULL` as written; a whole-row reference's
                // test stays whole when planned too (only a ROW() constructor is expanded).
                // (A constant's test is folded when planned, not stored folded.)
                // (One of a VALUES list or subquery, an anonymous row: its ROW()
                // expanded, each field tested as a scalar — a composite one's test is
                // IS [NOT] DISTINCT FROM NULL then.)
                // (Not one of a relation the planner keeps whole, a CTE's: `(r.* IS NULL)`.)
                if !self.deparse
                    && !whole_var
                    && is_whole_row(&arg)
                    && matches!(arg.ty(), DataType::Row(ref rt) if rt.oid == 0)
                    && let Expr::Row(items, _) = &arg
                {
                    let mut parts = Vec::new();
                    for i in items {
                        let t = i.ty();
                        if matches!(t, DataType::Row(_) | DataType::Record) {
                            let eq = crate::sql::func::resolve_binop("=", &t, &t)?.def;
                            parts.push(Expr::IsDistinct(
                                Box::new(i.clone()),
                                Box::new(Expr::Const(Value::Null, t.clone())),
                                !not,
                                eq,
                            ));
                        } else {
                            parts.push(Expr::IsNull(Box::new(i.clone()), not));
                        }
                    }
                    return Ok(match parts.len() {
                        1 => parts.pop().unwrap(),
                        _ => Expr::And(parts),
                    });
                }
                if (self.deparse && matches!(arg, Expr::Row(..) | Expr::Const(..)))
                    || is_whole_row(&arg)
                {
                    return Ok(Expr::IsNull(Box::new(arg), not));
                }
                Ok(null_test(arg, not))
            }
            NodeEnum::BooleanTest(t) => {
                let (k, what) = match pb::BoolTestType::try_from(t.booltesttype)
                    .unwrap_or(pb::BoolTestType::IsTrue)
                {
                    pb::BoolTestType::IsTrue => (BoolTest::IsTrue, "IS TRUE"),
                    pb::BoolTestType::IsNotTrue => (BoolTest::IsNotTrue, "IS NOT TRUE"),
                    pb::BoolTestType::IsFalse => (BoolTest::IsFalse, "IS FALSE"),
                    pb::BoolTestType::IsNotFalse => (BoolTest::IsNotFalse, "IS NOT FALSE"),
                    pb::BoolTestType::IsUnknown => (BoolTest::IsUnknown, "IS UNKNOWN"),
                    _ => (BoolTest::IsNotUnknown, "IS NOT UNKNOWN"),
                };
                let arg = self.bind_expr(t.arg.as_deref().unwrap())?;
                let arg = self.coerce_bool_at(arg, what, t.arg.as_deref().unwrap())?;
                // eval_const_expressions: a test of a constant is its result.
                if !self.deparse
                    && !self.unplanned
                    && let Expr::Const(v, _) = &arg
                {
                    let b = match v {
                        Value::Bool(b) => Some(*b),
                        _ => None,
                    };
                    let r = match k {
                        BoolTest::IsTrue => b == Some(true),
                        BoolTest::IsNotTrue => b != Some(true),
                        BoolTest::IsFalse => b == Some(false),
                        BoolTest::IsNotFalse => b != Some(false),
                        BoolTest::IsUnknown => b.is_none(),
                        BoolTest::IsNotUnknown => b.is_some(),
                    };
                    return Ok(Expr::Const(Value::Bool(r), DataType::Bool));
                }
                Ok(Expr::BoolTest(Box::new(arg), k))
            }
            NodeEnum::SubLink(s) => self.bind_sublink(s),
            NodeEnum::CaseExpr(c) => self.no_srfs_in("CASE", |b| b.bind_case(c)),
            NodeEnum::CoalesceExpr(c) => {
                let args = self.no_srfs_in("COALESCE", |b| {
                    c.args
                        .iter()
                        .map(|a| b.bind_expr(a))
                        .collect::<Result<Vec<_>>>()
                })?;
                let (args, ty) = self.unify(args, "COALESCE")?;
                if self.deparse || self.unplanned {
                    return Ok(Expr::Coalesce(args, ty));
                }
                // eval_const_expressions: NULL constants go, a constant ends the list (one
                // first is the result).
                let mut kept = Vec::new();
                for a in args {
                    match &a {
                        Expr::Const(v, _) if v.is_null() => {}
                        Expr::Const(..) => {
                            if kept.is_empty() {
                                return Ok(a);
                            }
                            kept.push(a);
                            break;
                        }
                        _ => kept.push(a),
                    }
                }
                Ok(if kept.is_empty() {
                    Expr::Const(Value::Null, ty)
                } else {
                    Expr::Coalesce(kept, ty)
                })
            }
            NodeEnum::MinMaxExpr(m) => {
                let greatest = m.op == pb::MinMaxOp::IsGreatest as i32;
                let name = if greatest { "GREATEST" } else { "LEAST" };
                let args = m
                    .args
                    .iter()
                    .map(|a| self.bind_expr(a))
                    .collect::<Result<Vec<_>>>()?;
                let (args, ty) = self.unify(args, name)?;
                let op = resolve_binop(if greatest { ">" } else { "<" }, &ty, &ty)?;
                let def = self.collated(op.def, &args.iter().collect::<Vec<_>>());
                Ok(Expr::MinMax(args, greatest, ty, def))
            }
            NodeEnum::SqlvalueFunction(f) => self.bind_sql_value_function(f),
            NodeEnum::MergeSupportFunc(m) => match self.merge_action {
                Some((lvl, col)) => Ok(self.lift(lvl, Expr::Col(col, DataType::Text))),
                None => Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        "MERGE_ACTION() can only be used in the RETURNING list of a MERGE command",
                    ),
                    m.location,
                )),
            },
            NodeEnum::AArrayExpr(a) => self.bind_array_expr(a, None),
            NodeEnum::RowExpr(r) => {
                // `t.*` / `(x).*` inside ROW() expand to their columns.
                let mut args = Vec::new();
                for a in &r.args {
                    let starred = match inner(a) {
                        Some(NodeEnum::ColumnRef(c)) => {
                            matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))
                        }
                        Some(NodeEnum::AIndirection(ind)) => matches!(
                            ind.indirection.last().and_then(inner),
                            Some(NodeEnum::AStar(_))
                        ),
                        _ => false,
                    };
                    if starred {
                        let list = [pb::Node {
                            node: Some(NodeEnum::ResTarget(Box::new(pb::ResTarget {
                                val: Some(Box::new(a.clone())),
                                ..Default::default()
                            }))),
                        }];
                        args.extend(self.bind_targets(&list)?.into_iter().map(|t| t.expr));
                    } else {
                        let e = self.bind_expr(a)?;
                        if let Expr::Param(i, DataType::Unknown) = &e {
                            self.untyped_params.push(*i);
                        }
                        args.push(e);
                    }
                }
                // transformRowExpr: MaxTupleAttributeNumber.
                if args.len() > 1664 {
                    return Err(with_pos(
                        Error::new(code::TOO_MANY_COLUMNS, "ROW expressions can have at most 1664 entries"),
                        r.location,
                    ));
                }
                let fields: Vec<(String, DataType)> = args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (format!("f{}", i + 1), a.ty()))
                    .collect();
                let ty = self.with_field_colls(DataType::row(fields), &args);
                Ok(Expr::Row(args, ty))
            }
            NodeEnum::AIndirection(ind) => self.bind_indirection(ind),
            NodeEnum::CollateClause(c) => {
                let coll = names(&c.collname);
                let arg = self.bind_expr(c.arg.as_deref().unwrap())?;
                let ty = arg.ty();
                if !ty.collatable() && ty != DataType::Unknown {
                    return Err(with_pos(
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            format!("collations are not supported by type {}", ty.display()),
                        ),
                        c.location,
                    ));
                }
                let Some(def) = crate::types::collation::resolve(&self.cat, &self.ctx.search_path, &coll) else {
                    return Err(with_pos(
                        Error::new(
                            code::UNDEFINED_OBJECT,
                            format!(
                                "collation \"{}\" for encoding \"UTF8\" does not exist",
                                coll.join(".")
                            ),
                        ),
                        c.location,
                    ));
                };
                if self.cat.collations.contains_key(&def.oid) && coll.len() == 1 {
                    self.note_coll_ref(c.location, def.oid);
                }
                // find_expr_references: a CollateExpr depends on its collation.
                if let Some(d) = self.deps.as_mut()
                    && self.cat.collations.contains_key(&def.oid)
                    && !d.contains(&(crate::catalog::depend::COLLATION, def.oid))
                {
                    d.push((crate::catalog::depend::COLLATION, def.oid));
                }
                Ok(Expr::Collate(
                    Box::new(arg),
                    coll.last().cloned().unwrap_or_default(),
                    c.location,
                ))
            }
            NodeEnum::FuncCall(f) => match self.bind_func_call(f) {
                Ok(e) => Ok(self.stored_column_call(e)),
                // ParseFuncOrColumn: `name(x)` with no such function is x's column `name`
                // when x is a composite that has one.
                Err(err)
                    if err.code == code::UNDEFINED_FUNCTION
                        && f.args.len() == 1
                        && f.funcname.len() == 1
                        && !f.agg_star
                        && !f.agg_distinct
                        && f.agg_order.is_empty()
                        && f.agg_filter.is_none()
                        && f.over.is_none()
                        && !f.func_variadic
                        && !matches!(inner(&f.args[0]), Some(NodeEnum::NamedArgExpr(_))) =>
                {
                    let name = names(&f.funcname).pop().unwrap_or_default();
                    let arg = self.bind_expr(&f.args[0])?;
                    if matches!(arg.ty(), DataType::Row(r) if r.fields.iter().any(|(n, _)| *n == name))
                    {
                        return self.field_select(arg, &name, f.location);
                    }
                    Err(err)
                }
                Err(err) => Err(err),
            },
            NodeEnum::GroupingFunc(g) => self.bind_grouping(g),
            NodeEnum::SetToDefault(_) => Ok(Expr::Default(DataType::Unknown)),
            NodeEnum::NamedArgExpr(_) => Err(Error::new(
                code::SYNTAX_ERROR,
                "named arguments are only allowed in function calls",
            )),
            NodeEnum::List(_) => Err(Error::new(code::SYNTAX_ERROR, "unexpected list")),
            NodeEnum::AStar(_) => Err(Error::new(
                code::SYNTAX_ERROR,
                "row expansion via \"*\" is not supported here",
            )),
            NodeEnum::JsonObjectConstructor(_)
            | NodeEnum::JsonArrayConstructor(_)
            | NodeEnum::JsonArrayQueryConstructor(_)
            | NodeEnum::JsonArrayAgg(_)
            | NodeEnum::JsonObjectAgg(_)
            | NodeEnum::JsonParseExpr(_)
            | NodeEnum::JsonScalarExpr(_)
            | NodeEnum::JsonSerializeExpr(_)
            | NodeEnum::JsonIsPredicate(_)
            | NodeEnum::JsonFuncExpr(_) => self.bind_sqljson(e),
            NodeEnum::XmlExpr(x) => self.bind_xml_expr(x),
            NodeEnum::XmlSerialize(x) => self.bind_xml_serialize(x),
            // (Only a view's rule puts one inside other conditions.)
            NodeEnum::CurrentOfExpr(_) => Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "WHERE CURRENT OF on a view is not implemented",
            )),
            other => Err(crate::error::unsupported(format!(
                "expression {:?}",
                std::mem::discriminant(other)
            ))),
        }
    }

    // ── names ──

    fn bind_column_ref(&mut self, c: &pb::ColumnRef) -> Result<Expr> {
        let fs = fields(c);
        let loc = c.location;
        let names: Vec<&str> = fs
            .iter()
            .filter_map(|f| {
                if let Field::Name(n) = f {
                    Some(*n)
                } else {
                    None
                }
            })
            .collect();
        let star = matches!(fs.last(), Some(Field::Star));
        if star {
            // `t.*` inside an expression: the whole row.
            if names.is_empty() {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        "row expansion via \"*\" is not supported here",
                    ),
                    loc,
                ));
            }
            if let Some(e) = self.find_rel_row(&names, loc)? {
                return Ok(e);
            }
            // A PL/pgSQL row or record variable: `new.*` is the variable itself.
            if let [var] = names.as_slice() {
                let is_var = self.levels.iter().any(|l| {
                    l.fn_args.as_ref().is_some_and(|a| {
                        a.iter()
                            .any(|(n, t)| n.as_deref() == Some(*var) && matches!(t, DataType::Row(_) | DataType::Record))
                    })
                });
                if is_var && let Some(e) = self.find_column(None, var, loc)? {
                    return Ok(e);
                }
            }
            // `fname.var.*`: a SQL function's composite argument qualified by its name.
            if let [q, var] = names.as_slice() {
                let is_var = self.levels.iter().any(|l| {
                    l.fn_name == *q
                        && l.fn_args.as_ref().is_some_and(|a| {
                            a.iter().any(|(n, t)| {
                                n.as_deref() == Some(*var)
                                    && matches!(t, DataType::Row(_) | DataType::Record)
                            })
                        })
                });
                if is_var && let Some(e) = self.find_column(Some(&[q]), var, loc)? {
                    return Ok(e);
                }
            }
            return Err(self.missing_rel(names.last().unwrap(), loc));
        }
        match names.as_slice() {
            [col] => {
                if let Some(e) = self.find_column(None, col, loc)? {
                    return Ok(e);
                }
                // A bare relation name is a whole-row reference.
                if let Some(e) = self.find_rel_row(&[col], loc)? {
                    return Ok(e);
                }
                Err(self.missing_column(None, col, loc))
            }
            [rel, col] => {
                if let Some(e) = self.find_column(Some(&[rel]), col, loc)? {
                    return Ok(e);
                }
                // As in Postgres, `a.b` is never `column.field` (that's `(a).b` or `t.a.b`).
                if self.rel_exists(&[rel]) {
                    // ... but it may be a function of the row: `p.f` is `f(p)`.
                    if self.cat.functions.values().any(|f| f.name == *col && f.args.len() == 1) {
                        let s = |v: &str| pb::Node {
                            node: Some(NodeEnum::String(pb::String { sval: v.to_string() })),
                        };
                        let call = pb::FuncCall {
                            funcname: vec![s(col)],
                            args: vec![pb::Node {
                                node: Some(NodeEnum::ColumnRef(pb::ColumnRef {
                                    fields: vec![
                                        s(rel),
                                        pb::Node {
                                            node: Some(NodeEnum::AStar(pb::AStar {})),
                                        },
                                    ],
                                    location: loc,
                                })),
                            }],
                            funcformat: pb::CoercionForm::CoerceExplicitCall as i32,
                            location: loc,
                            ..Default::default()
                        };
                        match self.bind_func_call(&call) {
                            Ok(e) => return Ok(self.stored_column_call(e)),
                            Err(e) if e.code != code::UNDEFINED_FUNCTION => return Err(e),
                            Err(_) => {}
                        }
                    }
                    return Err(self.missing_column(Some(rel), col, loc));
                }
                Err(self.missing_rel(rel, loc))
            }
            [schema, rel, col] => {
                if let Some(e) = self.find_column(Some(&[schema, rel]), col, loc)? {
                    return Ok(e);
                }
                if let Some(base) = self.find_column(Some(&[schema]), rel, loc)? {
                    return self.field_select(base, col, loc);
                }
                if self.rel_exists(&[schema, rel]) {
                    return Err(with_pos(
                        Error::new(
                            code::UNDEFINED_COLUMN,
                            format!("column {schema}.{rel}.{col} does not exist"),
                        ),
                        loc,
                    ));
                }
                Err(self.missing_rel(rel, loc))
            }
            [db, schema, rel, col] => {
                if *db != self.ctx.sess.database {
                    return Err(with_pos(
                        Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!(
                                "cross-database references are not implemented: {db}.{schema}.{rel}.{col}"
                            ),
                        ),
                        loc,
                    ));
                }
                match self.find_column(Some(&[schema, rel]), col, loc)? {
                    Some(e) => Ok(e),
                    None => Err(self.missing_rel(rel, loc)),
                }
            }
            _ => Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    format!(
                        "improper qualified name (too many dotted names): {}",
                        names.join(".")
                    ),
                ),
                loc,
            )),
        }
    }

    /// errorMissingColumn: no column `col` (of relation `rel`) here; what the range tables
    /// have instead (searchRangeTableForCol): an exact match that can't be referenced from
    /// here, or the column names closest to it.
    pub(crate) fn missing_column(&self, rel: Option<&str>, col: &str, loc: i32) -> Error {
        const MAX_FUZZY: usize = 3;
        let mut distance = MAX_FUZZY + 1;
        let mut first: Option<(String, String)> = None;
        let mut second: Option<(String, String)> = None;
        let mut exact: Vec<&ScopeRel> = Vec::new();
        let mut rels: Vec<&ScopeRel> = Vec::new();
        for (n, l) in self.visible_levels().enumerate() {
            rels.extend(self.levels[l].scope.rels.iter());
            if n == 0 {
                rels.extend(self.unreachable.iter());
            }
        }
        for r in rels {
            // (Nor a set operation's or VALUES list's output, which has no range-table
            // entry: its `*SELECT* n` / `*VALUES*` relations are among the unreachable.)
            if r.anonymous
                || r.join_rte
                || r.exists_col.is_some()
                || (r.alias.is_empty() && r.table.is_none() && r.rel.is_none())
            {
                continue;
            }
            let penalty = rel.map_or(0, |a| levenshtein(a, &r.alias).min(MAX_FUZZY + 1));
            let mut is_exact = false;
            for c in r.cols.iter().filter(|c| !c.hidden) {
                if c.name == col {
                    is_exact = true;
                }
                // updateFuzzyAttrMatchState.
                if penalty > distance {
                    continue;
                }
                let d = levenshtein(&c.name, col);
                if d > col.len() / 2 {
                    continue;
                }
                let d = d + penalty;
                if d < distance {
                    distance = d;
                    first = Some((r.alias.clone(), c.name.clone()));
                    second = None;
                } else if d == distance {
                    if second.is_some() {
                        first = None;
                        second = None;
                    } else if first.is_some() {
                        second = Some((r.alias.clone(), c.name.clone()));
                    }
                }
            }
            // A relation's system columns (not EXCLUDED's: its tuple isn't stored).
            if !is_exact
                && r.table.is_some()
                && !(self.in_conflict && r.alias == "excluded")
                && (col == "tableoid" || crate::exec::scan::SYS_COLS.iter().any(|(n, _)| *n == col))
            {
                is_exact = true;
            }
            if is_exact && penalty == 0 {
                exact.push(r);
            }
        }
        let msg = match rel {
            Some(r) => format!("column {r}.{col} does not exist"),
            None => format!("column \"{col}\" does not exist"),
        };
        let mut e = Error::new(code::UNDEFINED_COLUMN, msg);
        match exact.as_slice() {
            [_, _, ..] => {
                e = e.detail(format!("There are columns named \"{col}\", but they are in tables that cannot be referenced from this part of the query."));
                if rel.is_none() {
                    e = e.hint("Try using a table-qualified name.");
                }
            }
            [r] => {
                e = e.detail(format!("There is a column named \"{col}\" in table \"{}\", but it cannot be referenced from this part of the query.", r.alias));
                if r.lateral == super::LAT_SIBLING {
                    e = e.hint("To reference that column, you must mark this subquery with LATERAL.");
                } else if rel.is_none() && r.qualified_only {
                    e = e.hint("To reference that column, you must use a table-qualified name.");
                }
            }
            [] => match (first, second) {
                (Some((a, c)), None) => {
                    e = e.hint(format!("Perhaps you meant to reference the column \"{a}.{c}\"."));
                }
                (Some((a, c)), Some((a2, c2))) => {
                    e = e.hint(format!("Perhaps you meant to reference the column \"{a}.{c}\" or the column \"{a2}.{c2}\"."));
                }
                _ => {}
            },
        }
        with_pos(e, loc)
    }

    /// No relation `rel` in scope: one that can't be referenced from here, or a table
    /// referenced by its name instead of its alias, or none at all (errorMissingRTE).
    pub(crate) fn missing_rel(&self, rel: &str, loc: i32) -> Error {
        let invalid = || {
            Error::new(
                code::UNDEFINED_TABLE,
                format!("invalid reference to FROM-clause entry for table \"{rel}\""),
            )
        };
        if let Some(r) = self
            .unreachable
            .iter()
            .find(|r| !r.anonymous && r.alias == rel)
        {
            let mut e = invalid().detail(format!("There is an entry for table \"{rel}\", but it cannot be referenced from this part of the query."));
            if r.lateral == super::LAT_SIBLING {
                e = e.hint("To reference that table, you must mark this subquery with LATERAL.");
            }
            return with_pos(e, loc);
        }
        for l in self.visible_levels() {
            for r in &self.levels[l].scope.rels {
                if let Some(t) = &r.table
                    && t.name == rel
                    && r.alias != rel
                {
                    return with_pos(
                        invalid().hint(format!(
                            "Perhaps you meant to reference the table alias \"{}\".",
                            r.alias
                        )),
                        loc,
                    );
                }
            }
        }
        missing_rel(rel, loc)
    }

    fn rel_matches(r: &ScopeRel, qual: &[&str]) -> bool {
        if r.anonymous {
            return false;
        }
        match qual {
            [rel] => r.alias == *rel,
            [schema, rel] => r.alias == *rel && r.schema.as_deref() == Some(*schema),
            _ => false,
        }
    }

    fn rel_exists(&self, qual: &[&str]) -> bool {
        self.visible_levels().any(|l| {
            self.levels[l]
                .scope
                .rels
                .iter()
                .any(|r| Self::rel_matches(r, qual))
        })
    }

    /// Levels whose names are visible from the current one, innermost first (stops after a
    /// view or function body).
    pub(crate) fn visible_levels(&self) -> impl Iterator<Item = usize> + '_ {
        let top = self.levels.len() - 1;
        let stop = (0..=top)
            .rev()
            .find(|&l| self.levels[l].barrier)
            .unwrap_or(0);
        (stop..=top).rev()
    }

    /// The PL/pgSQL variable `qual.col` names, if any (`label.var`, `record.field`, `var`),
    /// at level `lvl`.
    fn pl_variable(&self, lvl: usize, qual: Option<&[&str]>, col: &str) -> Option<Expr> {
        let l = &self.levels[lvl];
        let args = l.fn_args.as_ref()?;
        match qual {
            None => l
                .fn_arg_index(col)
                .map(|i| Expr::Outer(1, i, args[i].1.clone())),
            Some([q]) => {
                // (PL/pgSQL's names under the function's name are its `fn_quals`.)
                if *q == l.fn_name
                    && l.var_conflict.is_none()
                    && let Some(i) = l.fn_arg_index(col)
                {
                    return Some(Expr::Outer(1, i, args[i].1.clone()));
                }
                if let Some(&(_, _, i)) = l.fn_quals.iter().find(|(lb, n, _)| lb == q && n == col) {
                    return Some(Expr::Outer(1, i, args[i].1.clone()));
                }
                let i = l.fn_arg_index(q)?;
                let ty = args[i].1.clone();
                let DataType::Row(fields) = &ty else {
                    return None;
                };
                let f = fields.iter().position(|(n, _)| n == col)?;
                let fty = fields[f].1.clone();
                Some(Expr::Field(Box::new(Expr::Outer(1, i, ty)), f, fty))
            }
            _ => None,
        }
    }

    /// Argument reference `e` of the SQL function body at level `lvl`, as the constant
    /// argument when the body is inlined over constants.
    fn inline_arg(&self, lvl: usize, e: &Expr) -> Option<Expr> {
        let l = &self.levels[lvl];
        match e {
            Expr::Outer(1, i, _) if l.inline_args && l.fn_args.is_some() => {
                l.params.get(*i).filter(|p| level_free(p)).cloned()
            }
            _ => None,
        }
    }

    /// Find a column, searching enclosing levels; lifts outer references into params.
    pub(crate) fn find_column(
        &mut self,
        qual: Option<&[&str]>,
        col: &str,
        loc: i32,
    ) -> Result<Option<Expr>> {
        // PL/pgSQL variables visible from here, and how they compete with columns.
        let pl = self
            .visible_levels()
            .find(|&l| self.levels[l].var_conflict.is_some());
        let conflict = pl.and_then(|l| self.levels[l].var_conflict);
        if let (Some(pl), Some(super::VarConflict::UseVariable)) = (pl, conflict)
            && let Some(e) = self.pl_variable(pl, qual, col)
        {
            return Ok(Some(self.lift(pl, e)));
        }
        for lvl in (0..self.levels.len()).rev() {
            let scope = &self.levels[lvl].scope;
            let mut found: Option<Expr> = None;
            let mut used = (0u32, 0i16);
            let mut rowtype_used: Option<(u32, i16)> = None;
            let mut hit: Option<(Option<i32>, String)> = None;
            let mut lat: u8 = 0;
            let mut rel_seen = false;
            for r in &scope.rels {
                match qual {
                    Some(q) => {
                        if !Self::rel_matches(r, q) {
                            continue;
                        }
                        if rel_seen {
                            return Err(with_pos(
                                Error::new(
                                    code::AMBIGUOUS_ALIAS,
                                    format!(
                                        "table reference \"{}\" is ambiguous",
                                        q.last().unwrap()
                                    ),
                                ),
                                loc,
                            ));
                        }
                        rel_seen = true;
                        // A join alias or subquery can have two columns of one name.
                        for c in r.cols.iter().filter(|c| !c.hidden && c.name == col) {
                            if found.is_some() {
                                return Err(with_pos(
                                    Error::new(
                                        code::AMBIGUOUS_COLUMN,
                                        format!("column reference \"{col}\" is ambiguous"),
                                    ),
                                    loc,
                                ));
                            }
                            found = Some(c.expr.clone());
                            used = (c.table, c.attnum);
                            rowtype_used = c.rowtype_col;
                            hit = Some((c.src, r.alias.clone()));
                            lat = r.lateral;
                        }
                    }
                    // RETURNING's old / new, and a rule action's, are only reached by name.
                    None if r.exists_col.is_some() || r.qualified_only => {}
                    None => {
                        for c in r.cols.iter().filter(|c| {
                            !c.hidden
                                && (!c.merged || c.attnum == super::AUX_ATTNUM)
                                && c.name == col
                        }) {
                            if found.is_some() {
                                return Err(with_pos(
                                    Error::new(
                                        code::AMBIGUOUS_COLUMN,
                                        format!("column reference \"{col}\" is ambiguous"),
                                    ),
                                    loc,
                                ));
                            }
                            found = Some(c.expr.clone());
                            used = (c.table, c.attnum);
                            rowtype_used = c.rowtype_col;
                            hit = Some((c.src, r.alias.clone()));
                            lat = r.lateral;
                        }
                    }
                }
            }
            // A stored view reads a function result's column by number and type: one
            // retyped or dropped since fails when it is evaluated (CheckVarSlotCompatibility).
            if let Some(vr) = self.view_rowtypes.clone() {
                match &found {
                    Some(e) => {
                        // (Printing the view shows the column as it is.)
                        if let Some((rel, attnum)) = rowtype_used
                            && !self.deparse
                            && let Some((.., was)) = vr.iter().find(|x| x.0 == rel && x.1 == attnum)
                            && was.oid() != e.ty().oid()
                        {
                            let err = Error::new(
                                code::DATATYPE_MISMATCH,
                                format!("attribute {attnum} of type record has wrong type"),
                            )
                            .detail(format!(
                                "Table has type {}, but query expects {}.",
                                e.ty().without_typmod().display(),
                                was.without_typmod().display()
                            ));
                            found = Some(crate::sql::expr::raising(err, was.clone()));
                        }
                    }
                    None => {
                        let dropped = |rel: u32, attnum: i16| {
                            self.cat.table(rel).is_some_and(|t| {
                                attnum > 0
                                    && t.columns.get(attnum as usize - 1).is_some_and(|c| c.dropped)
                            })
                        };
                        let gone = scope
                            .rels
                            .iter()
                            .filter(|r| match qual {
                                Some(q) => Self::rel_matches(r, q),
                                None => r.exists_col.is_none() && !r.qualified_only,
                            })
                            .find_map(|r| {
                                vr.iter()
                                    .find(|x| {
                                        x.2 == col
                                            && dropped(x.0, x.1)
                                            && r.cols.iter().any(|c| {
                                                c.rowtype_col.is_some_and(|(o, _)| o == x.0)
                                            })
                                    })
                                    .map(|x| (x, r))
                            });
                        if let Some(((_, attnum, _, was), r)) = gone {
                            // Printing the view: the column by the name Postgres gives a
                            // dropped one, qualified as its relation's others are.
                            let sibling = r
                                .cols
                                .iter()
                                .find(|c| !c.hidden && matches!(c.expr, Expr::Col(..)));
                            found = Some(match sibling {
                                Some(s) if self.deparse => crate::sql::expr::dropped_column(
                                    *attnum,
                                    s.expr.clone(),
                                    &s.name,
                                    was.clone(),
                                ),
                                // (EXPLAIN shows it by the view's name for it, qualified by
                                // its relation's.)
                                _ => crate::sql::expr::raising_shown(
                                    Error::new(
                                        code::UNDEFINED_COLUMN,
                                        format!(
                                            "attribute {attnum} of type record has been dropped"
                                        ),
                                    ),
                                    was.clone(),
                                    (!r.alias.is_empty()).then(|| {
                                        format!("{}.{}", crate::catalog::quote_ident(&r.alias), crate::catalog::quote_ident(col))
                                    }),
                                ),
                            });
                        }
                    }
                }
            }
            // A generation expression can't use another generated column.
            if found.is_some()
                && self.levels[lvl].clause == Clause::Generated
                && let Some(t) = scope
                    .rels
                    .iter()
                    .find_map(|r| r.table.as_ref().filter(|t| t.oid == used.0))
                && used.1 > 0
                && t.columns
                    .get(used.1 as usize - 1)
                    .is_some_and(|c| c.generated.is_some())
            {
                return Err(with_pos(
                    Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        format!(
                            "cannot use generated column \"{col}\" in column generation expression"
                        ),
                    )
                    .detail("A generated column cannot reference another generated column."),
                    loc,
                ));
            }
            // (scanNSItemForColumn: MERGE's WHEN conditions and CHECK constraints can't
            // read system columns, but tableoid.)
            let sys_clause = self.levels.last().map(|l| l.clause);
            if found.is_none()
                && col != "tableoid"
                && matches!(sys_clause, Some(Clause::MergeWhen | Clause::Check))
                && crate::exec::scan::SYS_COLS.iter().any(|(n, _)| *n == col)
                && scope.rels.iter().any(|r| {
                    r.table.is_some()
                        && qual.map_or(r.exists_col.is_none() && !r.qualified_only, |q| {
                            Self::rel_matches(r, q)
                        })
                })
            {
                let msg = if sys_clause == Some(Clause::Check) {
                    format!("system column \"{col}\" reference in check constraint is invalid")
                } else {
                    format!("cannot use system column \"{col}\" in MERGE WHEN condition")
                };
                return Err(with_pos(Error::new(code::INVALID_COLUMN_REFERENCE, msg), loc));
            }
            if found.is_none()
                && let Some((e, u)) = Self::system_column(scope, qual, col, loc)?
            {
                found = Some(e);
                used = u;
            }
            if found.is_some()
                && conflict == Some(super::VarConflict::Error)
                && let Some(pl) = pl
                && self.pl_variable(pl, qual, col).is_some()
            {
                let name = qual.map_or(col.to_string(), |q| format!("{}.{col}", q.join(".")));
                return Err(with_pos(
                    Error::new(
                        code::AMBIGUOUS_COLUMN,
                        format!("column reference \"{name}\" is ambiguous"),
                    )
                    .detail("It could refer to either a PL/pgSQL variable or a table column."),
                    loc,
                ));
            }
            if found.is_none()
                && let Some(args) = &self.levels[lvl].fn_args
            {
                // Function arguments by name (`fname.arg` too).
                let q_ok = match qual {
                    None => true,
                    Some([q]) => {
                        self.levels[lvl].fn_name == *q && self.levels[lvl].var_conflict.is_none()
                    }
                    _ => false,
                };
                if q_ok && let Some(i) = self.levels[lvl].fn_arg_index(col) {
                    found = Some(Expr::Outer(1, i, args[i].1.clone()));
                }
                if found.is_none()
                    && let Some([q]) = qual
                {
                    let lv = &self.levels[lvl];
                    if let Some(&(_, _, i)) =
                        lv.fn_quals.iter().find(|(l, n, _)| l == q && n == col)
                    {
                        // block_label.variable
                        found = Some(Expr::Outer(1, i, args[i].1.clone()));
                    } else if let Some(i) = lv.fn_arg_index(q) {
                        // record_variable.field
                        let ty = args[i].1.clone();
                        match &ty {
                            DataType::Row(fields) => {
                                match fields.iter().position(|(n, _)| n == col) {
                                    Some(f) => {
                                        found = Some(Expr::Field(
                                            Box::new(Expr::Outer(1, i, ty.clone())),
                                            f,
                                            fields[f].1.clone(),
                                        ))
                                    }
                                    None => {
                                        return Err(Error::new(
                                            code::UNDEFINED_COLUMN,
                                            format!("record \"{q}\" has no field \"{col}\""),
                                        ));
                                    }
                                }
                            }
                            DataType::Record => {
                                return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("record \"{q}\" is not assigned yet"))
                                    .detail("The tuple structure of a not-yet-assigned record is indeterminate."));
                            }
                            _ => {}
                        }
                    }
                }
            }
            if found.is_some()
                && matches!(lat, super::LAT_TARGET | super::LAT_JOIN)
                && let Some((_, refname)) = &hit
            {
                return Err(lateral_ref_error(refname, lat, loc));
            }
            if let Some(e) = found {
                self.note_col(used.0, used.1);
                self.note_rowtype_col(rowtype_used);
                if let Some((src, refname)) = hit
                    && loc >= 0
                    && let Some(log) = self.ref_log()
                {
                    log.cols.push(super::refs::ColRef {
                        loc,
                        rel: used.0,
                        attnum: used.1,
                        src,
                        quals: qual.map_or(0, <[&str]>::len),
                        name: col.to_string(),
                        refname,
                    });
                }
                if let Some(c) = self.inline_arg(lvl, &e) {
                    return Ok(Some(c));
                }
                return Ok(Some(self.lift(lvl, e)));
            }
            if rel_seen || self.levels[lvl].barrier {
                // The relation exists at this level but has no such column: don't look further out.
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// `tableoid`, `ctid`, `xmin`, … of a table in scope (user columns shadow them).
    fn system_column(
        scope: &super::Scope,
        qual: Option<&[&str]>,
        col: &str,
        loc: i32,
    ) -> Result<Option<(Expr, (u32, i16))>> {
        let sys = crate::exec::scan::SYS_COLS
            .iter()
            .position(|(n, _)| *n == col);
        if col != "tableoid" && sys.is_none() {
            return Ok(None);
        }
        let mut found = None;
        for r in &scope.rels {
            // (Unqualified, a relation inside a JOIN is reached through the join, which
            // has no system columns.)
            if qual.map_or(r.exists_col.is_some() || r.qualified_only || r.in_join, |q| {
                !Self::rel_matches(r, q)
            }) {
                continue;
            }
            let oid = match (&r.table, r.sys_table) {
                (Some(t), _) => t.oid,
                (None, Some(o)) => o,
                _ => continue,
            };
            let e = match (sys, r.sys_col) {
                (Some(i), Some(p)) if r.table.is_some() => {
                    Expr::Col(p + i, crate::exec::scan::SYS_COLS[i].1.clone())
                }
                _ if col == "tableoid" => Expr::Const(Value::Oid(oid), DataType::Oid),
                // A system catalog's rows: bootstrap-frozen, at fixed places.
                (Some(i), _) if r.table.is_none() && r.sys_table.is_some() => {
                    let v = match col {
                        "ctid" => Value::Tid(0, 1),
                        "xmin" => Value::Oid(1),
                        _ => Value::Oid(0),
                    };
                    Expr::Const(v, crate::exec::scan::SYS_COLS[i].1.clone())
                }
                _ => continue,
            };
            if found.is_some() {
                return Err(with_pos(
                    Error::new(
                        code::AMBIGUOUS_COLUMN,
                        format!("column reference \"{col}\" is ambiguous"),
                    ),
                    loc,
                ));
            }
            // (Its attnum: ctid -1 … tableoid -6.)
            let attnum = -(sys.unwrap_or(5) as i16 + 1);
            found = Some((e, (if r.table.is_some() { oid } else { 0 }, attnum)));
        }
        Ok(found)
    }

    /// Whole-row reference to a relation.
    fn find_rel_row(&mut self, qual: &[&str], loc: i32) -> Result<Option<Expr>> {
        for lvl in self.visible_levels().collect::<Vec<_>>() {
            let Some(r) = self.levels[lvl]
                .scope
                .rels
                .iter()
                .find(|r| Self::rel_matches(r, qual))
            else {
                continue;
            };
            // (A JOIN USING alias's columns are all merged ones, its row theirs.)
            let using_alias = !r.anonymous && !r.cols.is_empty() && r.cols.iter().all(|c| c.merged);
            let shown = |c: &&super::ScopeCol| !c.hidden && (using_alias || !c.merged);
            let used: Vec<(u32, i16)> = r
                .cols
                .iter()
                .filter(shown)
                .map(|c| (c.table, c.attnum))
                .collect();
            let cols: Vec<(String, Expr)> = r
                .cols
                .iter()
                .filter(shown)
                .map(|c| (c.name.clone(), c.expr.clone()))
                .collect();
            let table = r.table.as_ref().map(|t| t.oid).or(r.rel).or(r.sys_table);
            let base_table = r.table.as_ref().map(|t| t.oid);
            // A null-extended row (the nullable side of an outer join) is NULL as a whole:
            // known by a NOT NULL column being null, else by every column being null.
            let nulled = (r.nullable && !self.deparse && !r.cols.is_empty()).then(|| {
                let not_null = r.cols.iter().find(|c| {
                    !c.hidden
                        && !c.merged
                        && c.attnum > 0
                        && r.table.as_ref().is_some_and(|t| {
                            t.oid == c.table
                                && t.columns.get(c.attnum as usize - 1).is_some_and(|col| col.not_null)
                        })
                });
                match not_null {
                    Some(c) => Expr::IsNull(Box::new(c.expr.clone()), false),
                    None => Expr::And(
                        cols.iter()
                            .map(|(_, e)| Expr::IsNull(Box::new(e.clone()), false))
                            .collect(),
                    ),
                }
            });
            let exists = r.exists_col;
            let row_ty = r.row_ty.clone();
            if r.scalar_row
                && !self.deparse
                && let [(_, e)] = cols.as_slice()
            {
                let e = e.clone();
                for (t, a) in used {
                    self.note_col(t, a);
                }
                return Ok(Some(self.lift(lvl, e)));
            }
            self.whole_var_ref = r.whole_var;
            let row_ref = super::refs::RowRef {
                loc,
                rel: table.unwrap_or(0),
                src: r.src,
                quals: qual.len(),
                refname: r.alias.clone(),
            };
            if loc >= 0
                && let Some(log) = self.ref_log()
            {
                log.rows.push(row_ref);
            }
            // A table's whole-row Var reads the row as a whole (selectedCols' 0: SELECT on
            // every column, and a dependency on the table rather than on its columns).
            match base_table {
                Some(oid)
                    if oid >= crate::catalog::FIRST_USER_OID
                        && used.iter().all(|(t, _)| *t == oid) =>
                {
                    self.note_col(oid, 0)
                }
                _ => {
                    for (t, a) in used {
                        self.note_col(t, a);
                    }
                }
            }
            let fields: Vec<(String, DataType)> =
                cols.iter().map(|(n, e)| (n.clone(), e.ty())).collect();
            // A table's row has the table's row type (its column names, whatever aliases
            // the FROM item gave).
            let ty = match table.and_then(|t| self.cat.row_type_of(t)).or(row_ty) {
                Some(DataType::Row(rt)) if rt.oid != 0 && rt.fields.len() == fields.len() => {
                    let fields = rt
                        .fields
                        .iter()
                        .zip(fields)
                        .map(|((n, _), (_, t))| (n.clone(), t))
                        .collect();
                    DataType::Row(Arc::new(crate::types::RowType {
                        fields,
                        ..(*rt).clone()
                    }))
                }
                _ => {
                    let exprs: Vec<Expr> = cols.iter().map(|(_, e)| e.clone()).collect();
                    self.with_field_colls(DataType::row(fields), &exprs)
                }
            };
            let mut row = Expr::Row(cols.into_iter().map(|(_, e)| e).collect(), ty.clone());
            if let Some(test) = nulled.filter(|t| !matches!(t, Expr::And(v) if v.is_empty())) {
                row = Expr::Case {
                    operand: None,
                    whens: vec![(test, Expr::Const(Value::Null, ty.clone()))],
                    else_: Some(Box::new(row)),
                    ty: ty.clone(),
                };
            }
            // RETURNING old / new: NULL when there is no such row.
            if let Some(c) = exists {
                row = Expr::Case {
                    operand: None,
                    whens: vec![(Expr::Col(c, DataType::Bool), row)],
                    else_: None,
                    ty,
                };
            }
            return Ok(Some(self.lift(lvl, row)));
        }
        Ok(None)
    }

    /// find_expr_references: a FieldSelect of field `i` of `base` (a named composite)
    /// depends on the composite type's relation's column, noted while collecting
    /// dependencies. (A field of a relation's whole-row Var is that column's Var,
    /// ParseComplexProjection: noted as the column.)
    pub(crate) fn note_field_select(&mut self, base: &Expr, i: usize) {
        if self.deps.is_none() {
            return;
        }
        let DataType::Row(fields) = base.ty() else {
            return;
        };
        let rel_row = matches!(base, Expr::Row(items, _)
            if !items.is_empty()
                && items.iter().all(|x| matches!(x, Expr::Col(..) | Expr::Outer(..))))
            && !self.cat.composites.contains_key(&fields.oid);
        if fields.oid == 0 || rel_row || nullable_whole_row(base).is_some() {
            return;
        }
        let col = (self.rowtype_attnums(fields.oid))
            .and_then(|(rel, attnums)| Some((rel, *attnums.get(i)?)));
        self.note_rowtype_col(col);
    }

    fn field_select(&mut self, base: Expr, field: &str, loc: i32) -> Result<Expr> {
        match base.ty() {
            DataType::Row(fields) => match fields.iter().position(|(n, _)| n == field) {
                Some(i) => {
                    let ty = fields[i].1.clone();
                    self.note_field_select(&base, i);
                    if let Expr::Row(items, _) = base {
                        return Ok(items.into_iter().nth(i).unwrap());
                    }
                    // (eval_const_expressions: a field of a whole-row Var is the column, an
                    // outer join's null-extended row's included.)
                    if let Some(Expr::Row(items, _)) = nullable_whole_row(&base)
                        && items.get(i).is_some_and(|c| c.ty() == ty)
                    {
                        return Ok(items[i].clone());
                    }
                    Ok(Expr::Field(Box::new(base), i, ty))
                }
                None if fields.oid == 0 => Err(with_pos(
                    Error::new(
                        code::UNDEFINED_COLUMN,
                        format!("could not identify column \"{field}\" in record data type"),
                    ),
                    loc,
                )),
                None => Err(with_pos(
                    Error::new(
                        code::UNDEFINED_COLUMN,
                        format!(
                            "column \"{field}\" not found in data type {}",
                            base.ty().display()
                        ),
                    ),
                    loc,
                )),
            },
            DataType::Record => Err(with_pos(
                Error::new(
                    code::UNDEFINED_COLUMN,
                    format!("could not identify column \"{field}\" in record data type"),
                ),
                loc,
            )),
            t => Err(with_pos(
                Error::new(
                    code::UNDEFINED_TABLE,
                    format!(
                        "column notation .{field} applied to type {}, which is not a composite type",
                        t.display()
                    ),
                ),
                loc,
            )),
        }
    }

    pub(crate) fn bind_param(&mut self, number: i32, loc: i32) -> Result<Expr> {
        if number < 1 {
            return Err(with_pos(
                Error::new(
                    code::UNDEFINED_PARAMETER,
                    format!("there is no parameter ${number}"),
                ),
                loc,
            ));
        }
        let i = number as usize - 1;
        if let Some(&fl) = self.fn_levels.last() {
            let ty = match self.levels[fl].fn_args.as_ref().and_then(|a| a.get(i)) {
                Some((_, t)) => t.clone(),
                None => {
                    return Err(with_pos(
                        Error::new(
                            code::UNDEFINED_PARAMETER,
                            format!("there is no parameter ${number}"),
                        ),
                        loc,
                    ));
                }
            };
            let e = Expr::Outer(1, i, ty);
            if let Some(c) = self.inline_arg(fl, &e) {
                return Ok(c);
            }
            return Ok(self.lift(fl, e));
        }
        if self.params.len() <= i {
            if self.no_params {
                return Err(with_pos(
                    Error::new(
                        code::UNDEFINED_PARAMETER,
                        format!("there is no parameter ${number}"),
                    ),
                    loc,
                ));
            }
            self.params.resize(i + 1, DataType::Unknown);
        }
        // (A void parameter stays one: a function call drops it as an argument; so does one
        // declared as a domain, whose type the result keeps.)
        if self.param_consts
            && !matches!(self.params[i], DataType::Unknown | DataType::Void)
            && !self.param_domains.iter().any(|(j, _)| *j == i)
            && let Ok(v) = self.ctx.param(i)
        {
            return Ok(Expr::Const(v, self.params[i].clone()));
        }
        Ok(Expr::Param(i, self.params[i].clone()))
    }

    // ── constants ──

    fn bind_const(&mut self, c: &pb::AConst) -> Result<Expr> {
        if c.isnull {
            return Ok(Expr::Const(Value::Null, DataType::Unknown));
        }
        Ok(match &c.val {
            Some(pb::a_const::Val::Ival(i)) => Expr::Const(Value::Int4(i.ival), DataType::Int4),
            Some(pb::a_const::Val::Fval(f)) => numeric_const(&f.fval)?,
            Some(pb::a_const::Val::Boolval(b)) => {
                Expr::Const(Value::Bool(b.boolval), DataType::Bool)
            }
            Some(pb::a_const::Val::Sval(s)) => {
                let t: Arc<str> = Arc::from(s.sval.as_str());
                self.lit_locs.insert(t.as_ptr() as usize, c.location);
                Expr::Const(Value::Text(t), DataType::Unknown)
            }
            Some(pb::a_const::Val::Bsval(b)) => {
                let v = crate::types::text::parse_bits(&b.bsval, &DataType::Bit(-1))
                    .map_err(|e| with_pos(e, c.location))?;
                // bitin with no typmod: a bit string of its own length, typed plain `bit`.
                Expr::Const(v, DataType::Bit(-1))
            }
            None => Expr::Const(Value::Null, DataType::Unknown),
        })
    }

    fn bind_type_cast(&mut self, tc: &pb::TypeCast) -> Result<Expr> {
        let tn = tc.type_name.as_ref().unwrap();
        let (ty, domain) = super::resolve_type_name(&self.cat, &self.ctx.search_path, tn)?;
        if self.refs.is_some() {
            let named = match (&ty, domain) {
                // (`d[]` names the domain d: its array type has no name of its own here.)
                (_, Some(d)) => match self.cat.domain_type(d) {
                    Some((x, true)) if !tn.array_bounds.is_empty() => x.oid,
                    _ => d,
                },
                (DataType::Array(e), None) if !tn.array_bounds.is_empty() => {
                    crate::catalog::system::type_oid(&self.cat, e)
                }
                (t, None) => crate::catalog::system::type_oid(&self.cat, t),
            };
            self.note_type_ref(tn.location, named);
        }
        let arg = tc.arg.as_deref().unwrap();
        // ARRAY[]::int2vector / oidvector: an empty array isn't a valid vector.
        if let (Some(NodeEnum::AArrayExpr(a)), DataType::Int2Vector | DataType::OidVector) =
            (inner(arg), &ty)
            && a.elements.is_empty()
        {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("array is not a valid {}", ty.display()),
            ));
        }
        // ARRAY[...]::T[] coerces the elements first (so ARRAY[]::int[] works).
        if let (Some(NodeEnum::AArrayExpr(a)), DataType::Array(elem)) = (inner(arg), &ty) {
            let e = self.bind_array_expr(a, Some(elem))?;
            let out = self
                .cast_explicit(e, &ty)
                .map_err(|e| with_pos(e, tc.location))?;
            return match domain {
                // (An ArrayExpr of the domain's elements, each coerced to the domain: what
                // collects dependencies records the domain, not its array type.)
                Some(d) if self.deps.is_some() && self.cat.domain_type(d).is_some_and(|(_, a)| a) => {
                    let saved = self.deps.take();
                    let out = self.domain_check(out, d);
                    self.deps = saved;
                    let elem = self.cat.domain_type(d).map(|(x, _)| x.oid);
                    if let (Some(e), Some(deps)) = (elem, self.deps.as_mut())
                        && !deps.contains(&(1247, e))
                    {
                        deps.push((1247, e));
                    }
                    out
                }
                Some(d) => self.domain_check(out, d),
                None => Ok(out),
            };
        }
        // ARRAY[...]::oidvector / int2vector: the elements to oid / int2 (transformArrayExpr
        // takes the vector's element type), the array the vector.
        if let (Some(NodeEnum::AArrayExpr(a)), DataType::Int2Vector | DataType::OidVector) =
            (inner(arg), &ty)
        {
            let elem = if ty == DataType::OidVector { DataType::Oid } else { DataType::Int2 };
            let e = self.bind_array_expr(a, Some(&elem))?;
            let e = self
                .cast_explicit(e, &DataType::array_of(elem))
                .map_err(|e| with_pos(e, tc.location))?;
            return Ok(Expr::Relabel(Box::new(e), ty));
        }
        let e = self.bind_expr(arg)?;
        // coerce_type: an unknown NULL cast to anyarray / anyrange / ... is a constant of that
        // pseudo-type (anyelement and the other "any" types leave it unknown).
        if matches!(e, Expr::Const(Value::Null, DataType::Unknown))
            && matches!(
                ty,
                DataType::AnyArray
                    | DataType::AnyRange
                    | DataType::AnyMultirange
                    | DataType::AnyEnum
                    | DataType::AnyCompatibleArray
                    | DataType::AnyCompatibleRange
                    | DataType::AnyCompatibleMultirange
            )
        {
            return Ok(Expr::Const(Value::Null, ty));
        }
        if let (Expr::Param(i, DataType::Unknown), Some(d)) = (&e, domain) {
            self.param_domains.push((*i, d));
        }
        let lit = if matches!(e, Expr::Const(Value::Text(_), DataType::Unknown)) {
            Some(e.clone())
        } else {
            None
        };
        // "cannot cast" points at the operand, bad string-literal input at the literal; runtime
        // conversion errors (even when folded at bind time) carry no position.
        let is_str = matches!(inner(arg), Some(NodeEnum::AConst(c)) if matches!(c.val, Some(pb::a_const::Val::Sval(_))));
        // A domain value cast to its base type is no longer of the domain
        // (`pg_typeof(d::integer)`): Postgres's RelabelType.
        let relabel = domain.is_none()
            && !self.deparse
            && e.ty() == ty
            && !matches!(e, Expr::Relabel(..))
            && self.declared_domain(&e).is_some();
        if relabel {
            return Ok(Expr::Relabel(Box::new(e), ty));
        }
        let out = self.cast_explicit(e, &ty).map_err(|e| {
            if e.code == code::CANNOT_COERCE {
                with_pos(
                    e,
                    if tc.location >= 0 {
                        tc.location
                    } else {
                        ast::location(arg)
                    },
                )
            } else if is_str {
                with_pos(e, ast::location(arg))
            } else {
                e
            }
        })?;
        if let Some(lit) = lit
            && self
                .cat
                .domain_type(domain.unwrap_or(0))
                .is_some_and(|(_, array)| array)
        {
            return self.literal_domain_check(&lit, out, domain);
        }
        if let Some(d) = domain {
            return self.domain_check(out, d);
        }
        Ok(out)
    }

    /// Result column `i` of `n` from user function `f`, checked against the domain it was
    /// declared as (its result type, or an output parameter's).
    pub(crate) fn result_domain(
        &mut self,
        f: &crate::catalog::Function,
        i: usize,
        n: usize,
        e: Expr,
    ) -> Result<Expr> {
        let d = if !f.out_cols.is_empty() {
            f.out_domains.get(i).copied().flatten()
        } else if n == 1 {
            f.ret_domain
        } else {
            None
        };
        match d {
            Some(d) => self.domain_check(e, d),
            None => Ok(e),
        }
    }

    /// Apply a domain's NOT NULL / CHECK constraints to a cast result.
    pub(crate) fn domain_check(&mut self, e: Expr, domain: u32) -> Result<Expr> {
        if let Some(deps) = &mut self.deps
            && !deps.contains(&(1247, domain))
        {
            deps.push((1247, domain));
        }
        Ok(match self.domain_coerce(domain)? {
            Some(dc) => Expr::CoerceToDomain(Box::new(e), dc),
            None => e,
        })
    }

    /// A string literal read as an array of a domain: the array type's input checks the
    /// elements while parsing, errors pointing at the literal. `lit`: the expression before
    /// coercion; `out`: after.
    pub(crate) fn literal_domain_check(
        &mut self,
        lit: &Expr,
        out: Expr,
        domain: Option<u32>,
    ) -> Result<Expr> {
        let (Expr::Const(Value::Text(s), DataType::Unknown), Expr::Const(v, _), Some(d)) =
            (lit, &out, domain)
        else {
            return Ok(out);
        };
        if !self.cat.domain_type(d).is_some_and(|(_, array)| array) {
            return Ok(out);
        }
        let loc = self
            .lit_locs
            .get(&(s.as_ptr() as usize))
            .copied()
            .unwrap_or(-1);
        let Some(dc) = self.domain_coerce(d)? else {
            return Ok(out);
        };
        dc.check(v, self.ctx).map_err(|e| with_pos(e, loc))?;
        Ok(Expr::CoerceToDomain(Box::new(out), dc))
    }

    /// The constraints values of a domain (or of an array of one) must meet (none, maybe:
    /// the cast still makes a value of the domain); `None` when `oid` is not a domain.
    pub(crate) fn domain_coerce(
        &mut self,
        oid: u32,
    ) -> Result<Option<Arc<crate::sql::expr::DomainCoerce>>> {
        let Some((d, array)) = self.cat.domain_type(oid).map(|(d, a)| (d.clone(), a)) else {
            return Ok(None);
        };
        let (not_null, sqls) = self.cat.domain_constraints(&d);
        let mut checks = Vec::new();
        for (name, sql) in sqls {
            let node = ast::parse_expr(&sql)?;
            checks.push((name, self.bind_domain_check(&node, &d.base)?));
        }
        let shown =
            crate::sql::func::system::format_type_oid(d.oid, -1, &self.cat, &self.ctx.search_path)
                .unwrap_or_else(|| d.name.clone());
        // Over an array of a domain: the value is first that array type (its elements
        // checked).
        let base = match self.cat.domain_array_base(&d) {
            Some(ad) if ad != oid => self.domain_coerce(ad)?,
            _ => None,
        };
        Ok(Some(Arc::new(crate::sql::expr::DomainCoerce {
            oid,
            name: d.name.clone(),
            shown,
            schema: self.cat.schema_name(d.schema).to_string(),
            array,
            not_null,
            checks,
            tag: false,
            base,
        })))
    }

    /// `e` marked as being of the domain (or array-of-domain) type it's declared as, when
    /// that isn't apparent from it (a column, a function result, ...): `pg_typeof`'s
    /// argument.
    pub(crate) fn tag_declared_domain(&mut self, e: Expr) -> Result<Expr> {
        if matches!(e, Expr::CoerceToDomain(..)) {
            return Ok(e);
        }
        let Some(d) = self.declared_domain(&e) else {
            return Ok(e);
        };
        let Some(dc) = self.domain_coerce(d)? else {
            return Ok(e);
        };
        let dc = crate::sql::expr::DomainCoerce {
            oid: dc.oid,
            name: dc.name.clone(),
            shown: dc.shown.clone(),
            schema: dc.schema.clone(),
            array: dc.array,
            not_null: false,
            checks: Vec::new(),
            tag: true,
            base: None,
        };
        Ok(Expr::CoerceToDomain(Box::new(e), Arc::new(dc)))
    }

    /// An output value's domain (or array-of-domain) type: a cast to one, or a table column
    /// declared as one.
    pub(crate) fn expr_domain(&self, e: &Expr, table: u32, attnum: i16) -> Option<u32> {
        match e {
            Expr::CoerceToDomain(_, d) => Some(d.oid),
            Expr::Col(..) | Expr::Outer(..) if attnum > 0 => {
                self.cat
                    .tables
                    .get(&table)?
                    .columns
                    .get(attnum as usize - 1)?
                    .domain
            }
            _ => None,
        }
    }

    /// The user type clients see for a value of domain (or array-of-domain) type `domain`:
    /// an array of a domain is that array type, a domain its base type (getBaseType), which
    /// is one only for a domain over an array of a domain.
    pub(crate) fn domain_array(&self, domain: Option<u32>) -> Option<u32> {
        let d = domain?;
        match self.cat.domain_type(d)? {
            (_, true) => Some(d),
            (x, false) => self.cat.domain_array_base(x),
        }
    }

    /// Bind a domain CHECK expression; VALUE is Col(0).
    pub(crate) fn bind_domain_check(&mut self, n: &Node, base: &DataType) -> Result<Expr> {
        let rel = ScopeRel {
            alias: String::new(),
            schema: None,
            cols: vec![super::ScopeCol {
                name: "value".into(),
                expr: Expr::Col(0, base.clone()),
                hidden: false,
                merged: false,
                table: 0,
                attnum: 0,
                collation: None,
                src: None,
                rowtype_col: None,
                domain: None,
            }],
            table: None,
            key_col: None,
            anonymous: true,
            sys_col: None,
            sys_table: None,
            exists_col: None,
            qualified_only: false,
            src: None,
            shown: None,
            rel: None,
            nullable: false,
            lateral: 0,
            join_rte: false,
            in_join: false,
            whole_var: false,
            scalar_row: false,
            row_ty: None,
        };
        let mut lvl = super::Level::new();
        lvl.barrier = true;
        lvl.scope.rels.push(rel);
        self.levels.push(lvl);
        let r = self.in_clause(Clause::Check, |b| b.bind_expr(n));
        self.levels.pop();
        let e = r?;
        self.coerce_bool(e, "CHECK")
    }

    // ── operators ──

    fn bind_aexpr(&mut self, a: &pb::AExpr) -> Result<Expr> {
        let kind = pb::AExprKind::try_from(a.kind).unwrap_or(pb::AExprKind::AexprOp);
        let opname = names(&a.name).last().cloned().unwrap_or_default();
        let loc = a.location;
        match kind {
            pb::AExprKind::AexprOp => {
                let Some(l) = a.lexpr.as_deref() else {
                    let r = a.rexpr.as_deref().unwrap();
                    // Negative numeric literals fold into constants.
                    if opname == "-"
                        && let Some(NodeEnum::AConst(c)) = inner(r)
                    {
                        match &c.val {
                            Some(pb::a_const::Val::Ival(i)) => {
                                return Ok(Expr::Const(Value::Int4(-i.ival), DataType::Int4));
                            }
                            Some(pb::a_const::Val::Fval(f)) => {
                                return numeric_const(&format!("-{}", f.fval));
                            }
                            _ => {}
                        }
                    }
                    let r = self.bind_expr(r)?;
                    let parts = names(&a.name);
                    let schema = (parts.len() > 1).then(|| parts[parts.len() - 2].clone());
                    // (The operand is copied only when a user operator may take it: a copy at every
                    // level of a deep expression was quadratic.)
                    if !self.cat.operators.is_empty()
                        && let Some(e) =
                        self.user_operator(schema.as_deref(), &opname, None, r.clone(), loc)?
                    {
                        return Ok(e);
                    }
                    return self.prefix_op(&opname, r, loc);
                };
                // `ROW(...) op (SELECT a, b ...)`: a row comparison with the subquery's one
                // row (ROWCOMPARE_SUBLINK).
                if let Some(NodeEnum::RowExpr(_)) = inner(l)
                    && let Some(NodeEnum::SubLink(sl)) = a.rexpr.as_deref().and_then(inner)
                    && sl.sub_link_type == pb::SubLinkType::ExprSublink as i32
                {
                    return self.bind_sublink_in(sl, Some((&opname, l, loc)));
                }
                let l = self.bind_expr(l)?;
                let r = self.bind_expr(a.rexpr.as_deref().unwrap())?;
                let parts = names(&a.name);
                let schema = (parts.len() > 1).then(|| parts[parts.len() - 2].clone());
                self.binop_in(schema.as_deref(), &opname, l, r, loc)
            }
            pb::AExprKind::AexprOpAny | pb::AExprKind::AexprOpAll => {
                let l = self.bind_expr(a.lexpr.as_deref().unwrap())?;
                let r = self.bind_expr(a.rexpr.as_deref().unwrap())?;
                self.array_op(&opname, l, r, kind == pb::AExprKind::AexprOpAll, loc)
            }
            pb::AExprKind::AexprDistinct | pb::AExprKind::AexprNotDistinct => {
                let l = self.bind_expr(a.lexpr.as_deref().unwrap())?;
                let r = self.bind_expr(a.rexpr.as_deref().unwrap())?;
                self.distinct(l, r, kind == pb::AExprKind::AexprNotDistinct, loc)
            }
            pb::AExprKind::AexprNullif => {
                let l = self.bind_expr(a.lexpr.as_deref().unwrap())?;
                let r = self.bind_expr(a.rexpr.as_deref().unwrap())?;
                let (def, types) = self.equality_op(&l, &r, loc)?;
                let l = self.coerce_implicit(l, &types[0])?;
                let r = self.coerce_implicit(r, &types[1])?;
                let ty = l.ty();
                let def = self.collated(def, &[&l, &r]);
                let consts = l.is_const() && r.is_const() && def.volatility == 'i';
                let e = Expr::NullIf(Box::new(l), Box::new(r), def, ty.clone());
                // eval_const_expressions evaluates a NULLIF over constants.
                if consts && !self.deparse {
                    match eval(&e, &[], self.ctx) {
                        Ok(v) => return Ok(Expr::Const(v, ty)),
                        Err(err) => self.fold_failed(&e, err)?,
                    }
                }
                Ok(e)
            }
            pb::AExprKind::AexprIn => {
                let l = self.bind_expr(a.lexpr.as_deref().unwrap())?;
                let Some(NodeEnum::List(list)) = inner_box(&a.rexpr) else {
                    return Err(crate::error::internal("IN without a list"));
                };
                let items = list
                    .items
                    .iter()
                    .map(|i| self.bind_expr(i))
                    .collect::<Result<Vec<_>>>()?;
                self.in_list(l, items, opname == "<>", loc)
            }
            pb::AExprKind::AexprLike | pb::AExprKind::AexprIlike | pb::AExprKind::AexprSimilar => {
                let l = self.bind_expr(a.lexpr.as_deref().unwrap())?;
                let r = self.bind_expr(a.rexpr.as_deref().unwrap())?;
                self.binop(&opname, l, r, loc)
            }
            pb::AExprKind::AexprBetween
            | pb::AExprKind::AexprNotBetween
            | pb::AExprKind::AexprBetweenSym
            | pb::AExprKind::AexprNotBetweenSym => {
                let x = self.bind_expr(a.lexpr.as_deref().unwrap())?;
                let Some(NodeEnum::List(list)) = inner_box(&a.rexpr) else {
                    return Err(crate::error::internal("BETWEEN without bounds"));
                };
                let lo = self.bind_expr(&list.items[0])?;
                let hi = self.bind_expr(&list.items[1])?;
                let not = matches!(
                    kind,
                    pb::AExprKind::AexprNotBetween | pb::AExprKind::AexprNotBetweenSym
                );
                let sym = matches!(
                    kind,
                    pb::AExprKind::AexprBetweenSym | pb::AExprKind::AexprNotBetweenSym
                );
                let between = |b: &mut Self, lo: Expr, hi: Expr| -> Result<Expr> {
                    if not {
                        Ok(Expr::Or(vec![
                            b.binop("<", x.clone(), lo, loc)?,
                            b.binop(">", x.clone(), hi, loc)?,
                        ]))
                    } else {
                        Ok(Expr::And(vec![
                            b.binop(">=", x.clone(), lo, loc)?,
                            b.binop("<=", x.clone(), hi, loc)?,
                        ]))
                    }
                };
                let e1 = between(self, lo.clone(), hi.clone())?;
                if !sym {
                    return Ok(e1);
                }
                let e2 = between(self, hi, lo)?;
                Ok(if not {
                    Expr::And(vec![e1, e2])
                } else {
                    Expr::Or(vec![e1, e2])
                })
            }
            _ => Err(crate::error::unsupported("this operator form")),
        }
    }

    /// A built-in (pg_catalog) operator over `l` and `r`, whatever operator of that name the
    /// search path finds first: the planner's clauses made from an operator family's
    /// members (rebuilt hash keys, index conditions) are those, not looked up by name.
    pub(crate) fn builtin_binop(&mut self, op: &str, l: Expr, r: Expr) -> Result<Expr> {
        let res = resolve_binop(op, &l.ty(), &r.ty())?;
        self.call_resolved(res, vec![l, r])
    }

    pub(crate) fn binop(&mut self, op: &str, l: Expr, r: Expr, loc: i32) -> Result<Expr> {
        self.binop_in(None, op, l, r, loc)
    }

    /// `elem <@ range` / `range @> elem` over a constant range as bound comparisons (the
    /// range support functions' SupportRequestSimplify, done when planning). None: kept.
    fn range_containment_simplified(
        &mut self,
        op: &str,
        l: &Expr,
        r: &Expr,
    ) -> Result<Option<Expr>> {
        let (elem, range) = match op {
            "<@" => (l, r),
            "@>" => (r, l),
            _ => return Ok(None),
        };
        let Expr::Const(Value::Range(rv), DataType::Range(rt)) = range else {
            return Ok(None);
        };
        if !elem.ty().same_base(&rt.subtype) {
            return Ok(None);
        }
        // (The comparisons would run under the element's collation, not the range's: only
        // a range under the default collation is rewritten.)
        if rt.subtype.collatable() && rt.collation != 100 && rt.collation != 0 {
            return Ok(None);
        }
        if rv.empty {
            return Ok(Some(Expr::Const(Value::Bool(false), DataType::Bool)));
        }
        // The range's btree opfamily's operators (a pattern_ops class's are ~op~).
        let pattern = rt
            .subtype_opclass
            .as_deref()
            .is_some_and(|c| c.ends_with("_pattern_ops"));
        let op = |o: &'static str| -> &'static str {
            match (pattern, o) {
                (true, ">=") => "~>=~",
                (true, ">") => "~>~",
                (true, "<=") => "~<=~",
                (true, "<") => "~<~",
                (_, o) => o,
            }
        };
        let bounds = [
            (&rv.lower, op(if rv.lower_inc { ">=" } else { ">" })),
            (&rv.upper, op(if rv.upper_inc { "<=" } else { "<" })),
        ];
        let n = bounds.iter().filter(|(b, _)| b.is_some()).count();
        // A volatile element can't be evaluated twice.
        if n == 2 && elem.is_volatile() {
            return Ok(None);
        }
        let mut parts = Vec::new();
        for (b, cmp) in bounds {
            if let Some(v) = b {
                let bound = Expr::Const(v.clone(), rt.subtype.clone());
                parts.push(self.binop_in(None, cmp, elem.clone(), bound, -1)?);
            }
        }
        Ok(Some(match parts.len() {
            0 => Expr::Const(Value::Bool(true), DataType::Bool),
            1 => parts.pop().unwrap(),
            _ => Expr::And(parts),
        }))
    }

    /// A binary operator, `OPERATOR(schema.op)` when `schema` is given.
    fn binop_in(
        &mut self,
        schema: Option<&str>,
        op: &str,
        l: Expr,
        r: Expr,
        loc: i32,
    ) -> Result<Expr> {
        if schema.is_none()
            && !self.deparse
            && let Some(e) = self.range_containment_simplified(op, &l, &r)?
        {
            return Ok(e);
        }
        // Row comparisons (two ROW() constructors); a whole-row reference compares as a
        // record (record_eq …).
        if let (Expr::Row(ls, _), Expr::Row(rs, _)) = (&l, &r)
            && !is_whole_row(&l)
            && !is_whole_row(&r)
        {
            // Postgres's tree keeps an ordering comparison of rows whole (RowCompareExpr).
            if self.deparse
                && matches!(op, "<" | "<=" | ">" | ">=")
                && ls.len() == rs.len()
                && self.row_compare(op, ls.clone(), rs.clone(), loc).is_ok()
            {
                let def = func::display_op(op, vec![l.ty(), r.ty()], DataType::Bool);
                return Ok(Expr::Call(def, vec![l, r], DataType::Bool));
            }
            return self.row_compare(op, ls.clone(), rs.clone(), loc);
        }
        // (The operands are copied only when a user operator may take them: a copy at every
        // level of a deep expression was quadratic.)
        if !self.cat.operators.is_empty()
            && let Some(e) = self.user_operator(schema, op, Some(l.clone()), r.clone(), loc)?
        {
            return Ok(e);
        }
        let res = match (resolve_binop(op, &l.ty(), &r.ty()), schema) {
            (Ok(res), None | Some("pg_catalog")) => res,
            (Err(e), None) => return Err(with_pos(e, loc)),
            (_, Some(sc)) => {
                return Err(with_pos(
                    Error::new(code::UNDEFINED_FUNCTION, format!("operator does not exist: {} {sc}.{op} {}", l.ty().without_typmod().display(), r.ty().without_typmod().display()))
                        .hint("No operator matches the given name and argument types. You might need to add explicit type casts."),
                    loc,
                ));
            }
        };
        self.call_resolved(res, vec![l, r])
    }

    /// A user-defined operator (CREATE OPERATOR) for these operands, when one matches better
    /// than any built-in: a call of its function, shown as the operator.
    /// The user-defined operator `op` that operands of types `lt` and `rt` resolve to (ahead
    /// of the built-ins, as `oper` chooses): its call target and argument types.
    fn user_op_def(&mut self, op: &str, lt: &DataType, rt: &DataType, loc: i32) -> Result<Option<(Arc<FuncDef>, Vec<DataType>)>> {
        if self.cat.operators.is_empty() {
            return Ok(None);
        }
        let probe = |t: &DataType| Expr::Const(Value::Null, t.clone());
        Ok(match self.user_operator(None, op, Some(probe(lt)), probe(rt), loc)? {
            Some(Expr::Call(def, args, _)) if args.len() == 2 => {
                let types = args.iter().map(Expr::ty).collect();
                Some((def, types))
            }
            _ => None,
        })
    }

    fn user_operator(
        &mut self,
        schema: Option<&str>,
        op: &str,
        l: Option<Expr>,
        r: Expr,
        loc: i32,
    ) -> Result<Option<Expr>> {
        if self.cat.operators.is_empty() {
            return Ok(None);
        }
        let path: Vec<u32> = match schema {
            Some(s) => self
                .cat
                .schema_by_name(s)
                .map(|x| vec![x.oid])
                .unwrap_or_default(),
            None => self.cat.routine_path(&self.ctx.search_path),
        };
        // The first of each signature along the path.
        let mut ops: Vec<Arc<crate::catalog::Operator>> = Vec::new();
        for sc in &path {
            for o in self
                .cat
                .operators
                .values()
                .filter(|o| o.schema == *sc && o.name == op && o.left.is_some() == l.is_some())
            {
                if !ops.iter().any(|x| {
                    x.left.as_ref().map(|t| t.oid()) == o.left.as_ref().map(|t| t.oid())
                        && x.right.same_base(&o.right)
                }) {
                    ops.push(o.clone());
                }
            }
        }
        if ops.is_empty() {
            return Ok(None);
        }
        // OpernameGetCandidates: the built-ins are pg_catalog's, at its place in the path; an
        // operator of the same argument types in a schema searched earlier hides a later one.
        let builtin: Vec<Arc<FuncDef>> = if schema.is_none_or(|s| s == "pg_catalog") {
            let reg = func::registry();
            let b = if l.is_some() {
                reg.binops.get(op)
            } else {
                reg.prefix.get(op)
            };
            b.into_iter().flatten().cloned().collect()
        } else {
            Vec::new()
        };
        let pc = path.iter().position(|&s| s == crate::catalog::PG_CATALOG_OID);
        let at = |o: &crate::catalog::Operator| path.iter().position(|&s| s == o.schema);
        let sig = |o: &crate::catalog::Operator| -> Vec<DataType> { o.left.iter().cloned().chain([o.right.clone()]).collect() };
        let same = |a: &[DataType], b: &[DataType]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.same_base(y));
        let builtin: Vec<Arc<FuncDef>> = builtin
            .into_iter()
            .filter(|b| {
                !ops.iter().any(|o| pc.is_some_and(|pc| at(o).is_some_and(|i| i < pc)) && same(&sig(o), &b.args))
            })
            .collect();
        if pc.is_some() && !builtin.is_empty() {
            ops.retain(|o| !(at(o) > pc && builtin.iter().any(|b| same(&sig(o), &b.args))));
        }
        if ops.is_empty() {
            return Ok(None);
        }
        let inputs: Vec<DataType> = l.iter().map(|e| e.ty()).chain([r.ty()]).collect();
        let mut cands: Vec<Arc<FuncDef>> = ops
            .iter()
            .map(|o| {
                func::candidate_def(
                    o.oid,
                    o.left.iter().cloned().chain([o.right.clone()]).collect(),
                    o.result.clone().unwrap_or(DataType::Bool),
                )
            })
            .collect();
        cands.extend(builtin);
        // The exact match looks at the declared types before a domain input is reduced to
        // its base type (func_select_candidate): an operator over a domain isn't exact for
        // a plain value of the base type, nor a built-in for a value of the domain.
        let in_dom: Vec<Option<u32>> = l
            .iter()
            .chain([&r])
            .map(|e| match e {
                Expr::CaseTest(_) => CASE_TEST_DOMAIN.get(),
                e => self.declared_domain(e),
            })
            .collect();
        let user_doms: Vec<Vec<Option<u32>>> = ops
            .iter()
            .map(|o| {
                self.cat
                    .functions
                    .get(&o.func)
                    .map(|f| f.arg_domains.clone())
                    .unwrap_or_default()
            })
            .collect();
        if in_dom.iter().any(Option::is_some) || user_doms.iter().flatten().any(Option::is_some) {
            let exact = |params: &[DataType], doms: &[Option<u32>]| {
                params.len() == inputs.len()
                    && params.iter().zip(&inputs).enumerate().all(|(i, (p, t))| {
                        !matches!(t, DataType::Unknown)
                            && !p.is_polymorphic()
                            && p.same_base(t)
                            && doms.get(i).copied().flatten() == in_dom[i]
                    })
            };
            let found: Vec<Arc<FuncDef>> = cands
                .iter()
                .enumerate()
                .filter(|(i, c)| {
                    !c.variadic && exact(&c.args, user_doms.get(*i).map_or(&[], |d| d.as_slice()))
                })
                .map(|(_, c)| c.clone())
                .collect();
            if found.len() == 1 {
                cands = found;
            }
        }
        let res = match select_candidate(&cands, &inputs, l.is_some()) {
            Ok(Some(res)) => res,
            Ok(None) => return Ok(None),
            Err(_) => {
                let shown = match &l {
                    Some(l) => format!(
                        "{} {op} {}",
                        l.ty().without_typmod().display(),
                        r.ty().without_typmod().display()
                    ),
                    None => format!("{op} {}", r.ty().without_typmod().display()),
                };
                return Err(with_pos(
                    Error::new(code::AMBIGUOUS_FUNCTION, format!("operator is not unique: {shown}"))
                        .hint("Could not choose a best candidate operator. You might need to add explicit type casts."),
                    loc,
                ));
            }
        };
        let Some(o) = ops
            .iter()
            .find(|o| o.oid == res.def.oid && res.def.name.is_empty())
            .cloned()
        else {
            return Ok(None);
        };
        if o.func == 0 {
            let shown = match &l {
                Some(_) => format!("{} {op} {}", inputs[0].display(), inputs[1].display()),
                None => format!("{op} {}", inputs[0].display()),
            };
            return Err(with_pos(
                Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!("operator is only a shell: {shown}"),
                ),
                loc,
            ));
        }
        if let Some(deps) = &mut self.deps
            && !deps.contains(&(2617, o.oid))
        {
            deps.push((2617, o.oid));
        }
        if schema.is_none() {
            self.note_oper_ref(loc, o.oid);
        }
        let mut args = Vec::new();
        for (a, t) in l.into_iter().chain([r]).zip(&res.arg_types) {
            args.push(self.coerce_implicit(a, t)?);
        }
        let ret = match o.result.clone().unwrap_or(DataType::Bool) {
            // (A polymorphic result: resolved from the operands.)
            t if t.is_polymorphic() => res.ret.clone(),
            t => t,
        };
        // Shown as written: qualified when its schema isn't searched.
        let visible = self
            .cat
            .effective_path(&self.ctx.search_path)
            .contains(&o.schema);
        let display = if visible {
            func::intern(&o.name)
        } else {
            func::intern(&format!(
                "OPERATOR({}.{})",
                self.cat.schema_name(o.schema),
                o.name
            ))
        };
        if let Some(f) = self.cat.functions.get(&o.func).cloned() {
            if self.deparse {
                return Ok(Some(Expr::Call(
                    Arc::new(FuncDef {
                        name: display,
                        ..(*func::display_def(&f)).clone()
                    }),
                    args,
                    ret,
                )));
            }
            // An internal-language function runs the built-in, a C one its native code, shown
            // as the operator.
            if f.language == "internal" || (f.language == "c" && Self::native_def(&f).is_some() && !f.returns_set) {
                let call = if f.language == "c" {
                    self.native_call(&f, args, loc)?
                } else {
                    self.internal_call(&f, args, loc)?
                };
                return Ok(Some(match call {
                    Expr::Call(d, a, t) => Expr::Call(
                        Arc::new(FuncDef {
                            name: display,
                            ..(*d).clone()
                        }),
                        a,
                        t,
                    ),
                    e => e,
                }));
            }
            return self.user_call(f, args, loc).map(Some);
        }
        let def =
            func::def_by_oid(o.func).ok_or_else(|| crate::error::internal("operator function"))?;
        // A set-returning function behind the operator: a ProjectSet call, as the function's.
        if def.is_srf() && !self.deparse {
            self.check_srf_placement(loc)?;
            let def = self.input_collation_def(def, &args);
            let i = self.push_srf(SrfCall {
                func: def,
                args,
                ty: ret.clone(),
                ncols: 1,
                body: None,
            });
            return Ok(Some(Expr::Srf(i, ret)));
        }
        let e = Expr::Call(
            Arc::new(FuncDef {
                name: display,
                ..(*def).clone()
            }),
            args,
            ret,
        );
        self.fold(e).map(Some)
    }

    fn prefix_op(&mut self, op: &str, r: Expr, loc: i32) -> Result<Expr> {
        let res = resolve_prefix(op, &r.ty()).map_err(|e| with_pos(e, loc))?;
        self.call_resolved(res, vec![r])
    }

    /// Coerce arguments to a resolved function's parameter types and build the call,
    /// folding immutable calls over constants.
    pub(crate) fn call_resolved(&mut self, res: Resolved, args: Vec<Expr>) -> Result<Expr> {
        let mut out = Vec::with_capacity(args.len());
        for (a, t) in args.into_iter().zip(res.arg_types.iter()) {
            out.push(self.coerce_implicit(a, t)?);
        }
        let e = Expr::Call(res.def.clone(), out, res.ret);
        self.fold(e)
    }

    /// Evaluate immutable calls over constants now.
    pub(crate) fn fold(&mut self, e: Expr) -> Result<Expr> {
        let e = self.with_input_collation(e);
        self.fold_call(e)
    }

    fn fold_call(&mut self, e: Expr) -> Result<Expr> {
        // simplify_function: a strict function over a NULL constant is NULL, whatever its
        // other arguments.
        // (Its arguments were simplified first: one that failed to fold fails the
        // statement, so the call stays.)
        if let Expr::Call(def, args, ty) = &e
            && !self.deparse
            && !self.unplanned
            && def.strict
            && !def.is_srf()
            && args.iter().any(|a| matches!(a, Expr::Const(v, _) if v.is_null()))
            && !(self.folds_failed() && args.iter().any(|a| super::fold::fails(a, self.ctx, self.fold_err.as_deref().unwrap_or(&[]))))
        {
            return Ok(Expr::Const(crate::types::Value::Null, ty.clone()));
        }
        if let Expr::Call(def, args, ty) = &e
            && !self.deparse
            && def.volatility == 'i'
            && !def.is_srf()
            // (A constant under a COLLATE is one: eval_const_expressions makes the
            // CollateExpr a Const of that collation. Only for a result without a collation,
            // which a folded value couldn't carry on: `'a' = 'a' COLLATE "x"`.)
            && args.iter().all(|a| match a {
                Expr::Collate(x, ..) => matches!(**x, Expr::Const(..)) && !ty.collatable(),
                a => matches!(a, Expr::Const(..)),
            })
        {
            // (Explicit collations that conflict fail first, as assign_collations does.)
            if args.iter().any(|a| matches!(a, Expr::Collate(..))) {
                self.collation_of(&e)?;
            }
            let v = match eval(&e, &[], self.ctx) {
                Ok(v) => v,
                // Postgres folds when it plans: a failing call stays a call, whose error
                // is the statement's if planning still has it (`fold_failed`).
                Err(err) => {
                    self.fold_failed(&e, err)?;
                    return Ok(e);
                }
            };
            let folded = Expr::Const(v, ty.clone());
            self.note_const_coll(&e, &folded);
            return Ok(folded);
        }
        Ok(e)
    }

    /// Case mapping and ILIKE under the collation their arguments derive: one whose ctype is
    /// C (C, POSIX, ucs_basic) runs the ASCII-only implementation.
    fn with_input_collation(&self, e: Expr) -> Expr {
        let Expr::Call(def, args, ty) = e else {
            return e;
        };
        let mut def = self.input_collation_def(def, &args);
        // A range function or operator compares bounds under the range type's subtype
        // collation (`rng_collation`), not an input collation.
        if def.coll.is_none()
            && !self.deparse
            && let Some(c) = args
                .iter()
                .map(Expr::ty)
                .chain(std::iter::once(ty.clone()))
                .find_map(|t| match t {
                    // (A composite subtype: its fields' collations.)
                    DataType::Range(rt) | DataType::Multirange(rt) => rt
                        .coll()
                        .or_else(|| crate::types::collation::for_type(&self.cat, &rt.subtype)),
                    _ => None,
                })
        {
            def = Arc::new(FuncDef {
                coll: Some(c),
                ..(*def).clone()
            });
        }
        Expr::Call(def, args, ty)
    }

    /// A grouping key (GROUP BY, DISTINCT) that derives a nondeterministic collation carries
    /// it as a COLLATE, so grouping finds the values it deems equal equal.
    pub(crate) fn nondeterministic_key(&self, e: &Expr) -> Expr {
        if self.deparse || matches!(e, Expr::Collate(..)) || !e.ty().collatable() {
            return e.clone();
        }
        match self.collation_of(e) {
            Ok(super::Coll::Implicit(c))
                if crate::types::collation::by_name(&self.cat, &c)
                    .is_some_and(|k| !k.deterministic) =>
            {
                Expr::Collate(Box::new(e.clone()), c, -1)
            }
            _ => e.clone(),
        }
    }

    /// A GROUP BY / DISTINCT key as a hash table hashes it: under a nondeterministic
    /// collation it carries it (`nondeterministic_key`); without any collation, hashing a
    /// value fails, as hashtext does.
    pub(crate) fn hashed_key(&self, e: &Expr) -> Expr {
        let k = self.nondeterministic_key(e);
        if self.deparse || !e.ty().collatable() || matches!(e, Expr::Collate(..)) {
            return k;
        }
        match self.collation_of(e) {
            Ok(super::Coll::None) => unhashable_key(k),
            _ => k,
        }
    }

    /// A partitioning key under a collation that doesn't compare by bytes carries it as a
    /// COLLATE, so partitions group and order by it.
    pub(crate) fn collated_key(&self, e: &Expr) -> Expr {
        if self.deparse || matches!(e, Expr::Collate(..)) || !e.ty().collatable() {
            return e.clone();
        }
        match self.collation_of(e) {
            Ok(super::Coll::Implicit(c))
                if crate::types::collation::by_name(&self.cat, &c)
                    .is_some_and(|k| !k.bytewise()) =>
            {
                Expr::Collate(Box::new(e.clone()), c, -1)
            }
            _ => e.clone(),
        }
    }

    /// How values of `e`'s type compare when it holds composites (`collation::for_type`).
    pub(crate) fn record_coll(&self, e: &Expr) -> Option<Arc<crate::types::collation::Collation>> {
        let ty = e.ty();
        if self.deparse || !ty.holds_composite() {
            return None;
        }
        crate::types::collation::for_type(&self.cat, &ty)
    }

    /// An anonymous row type over `args` with the field collations they derive other than
    /// the default (`RowType::colls`).
    fn with_field_colls(&self, ty: DataType, args: &[Expr]) -> DataType {
        let DataType::Row(rt) = &ty else { return ty };
        let colls: Vec<Option<Arc<str>>> = args
            .iter()
            .map(|a| match a.ty().collatable().then(|| self.collation_of(a)) {
                Some(Ok(super::Coll::Implicit(c) | super::Coll::Explicit(c, _))) if c != "default" => {
                    Some(Arc::from(c.as_str()))
                }
                _ => None,
            })
            .collect();
        if self.deparse || colls.iter().all(Option::is_none) {
            return ty;
        }
        DataType::Row(Arc::new(crate::types::RowType {
            colls,
            ..(**rt).clone()
        }))
    }

    /// A comparison over composite values (or arrays or ranges of them) under their fields'
    /// collations: record_cmp's attcollation.
    fn record_compare_def(&self, def: Arc<FuncDef>, args: &[Expr]) -> Arc<FuncDef> {
        if def.coll.is_some()
            || !matches!(
                def.name,
                "=" | "<>" | "<" | "<=" | ">" | ">=" | "btrecordcmp" | "btarraycmp" | "array_sort"
            )
        {
            return def;
        }
        match args.iter().find_map(|a| self.record_coll(a)) {
            Some(c) => Arc::new(FuncDef {
                coll: Some(c),
                ..(*def).clone()
            }),
            None => def,
        }
    }

    /// A comparison's `def` as it runs over `args` (see `input_collation_def`).
    fn collated(&self, def: Arc<FuncDef>, args: &[&Expr]) -> Arc<FuncDef> {
        let args: Vec<Expr> = args.iter().map(|a| (*a).clone()).collect();
        self.input_collation_def(def, &args)
    }

    /// `def` as it runs under the collation `args` derive (`c_ctype_variant`).
    pub(crate) fn input_collation_def(
        &self,
        def: Arc<func::FuncDef>,
        args: &[Expr],
    ) -> Arc<func::FuncDef> {
        let c_def = func::string::c_ctype_variant(&def);
        let needs = needs_collation(&def, args);
        if c_def.is_none() && needs.is_none() && !crate::types::collation::sensitive(&def) {
            return self.record_compare_def(def, args);
        }
        let mut coll = super::Coll::None;
        for a in args {
            match self.collation_of(a).and_then(|c| coll.clone().merge(c)) {
                Ok(c) => coll = c,
                Err(_) => return def,
            }
        }
        // A collation that behaves unlike the default (ICU, PG_UNICODE_FAST, a libc locale)
        // goes with the call.
        if !self.deparse
            && let super::Coll::Implicit(c) | super::Coll::Explicit(c, _) = &coll
            && let Some(d) = crate::types::collation::apply(&def, c, &self.cat, c_def.as_ref())
        {
            return d;
        }
        if let super::Coll::Implicit(c) | super::Coll::Explicit(c, _) = &coll
            && matches!(c.as_str(), "C" | "POSIX" | "ucs_basic")
            && let Some(c_def) = c_def
        {
            return Arc::new(c_def);
        }
        // Inputs whose implicit collations conflict, or that have none: the call fails
        // when it runs.
        if matches!(coll, super::Coll::Conflict(..) | super::Coll::None)
            && let Some(imp) = needs
        {
            return Arc::new(func::FuncDef {
                imp,
                srf: None,
                ..(*def).clone()
            });
        }
        def
    }

    fn row_compare(&mut self, op: &str, ls: Vec<Expr>, rs: Vec<Expr>, loc: i32) -> Result<Expr> {
        if ls.is_empty() && rs.is_empty() {
            return Err(with_pos(
                Error::new(code::SYNTAX_ERROR, "cannot compare rows of zero length"),
                loc,
            ));
        }
        if ls.len() != rs.len() {
            return Err(with_pos(
                Error::new(
                    code::SYNTAX_ERROR,
                    "unequal number of entries in row expressions",
                ),
                loc,
            ));
        }
        match op {
            "=" | "<>" => {
                let parts = ls
                    .into_iter()
                    .zip(rs)
                    .map(|(a, b)| self.binop(op, a, b, loc))
                    .collect::<Result<Vec<_>>>()?;
                Ok(if op == "=" {
                    Expr::And(parts)
                } else {
                    Expr::Or(parts)
                })
            }
            // (The text_pattern_ops family's operators order rows too.)
            "<" | "<=" | ">" | ">=" | "~<~" | "~<=~" | "~>=~" | "~>~" if ls.len() > 1 => {
                // make_row_comparison_op: a RowCompareExpr, each pair compared with `op`
                // (equal pairs pass to the next one).
                let op: &'static str = match op {
                    "<" => "<",
                    "<=" => "<=",
                    ">" => ">",
                    ">=" => ">=",
                    "~<~" => "~<~",
                    "~<=~" => "~<=~",
                    "~>=~" => "~>=~",
                    _ => "~>~",
                };
                let mut cmps = Vec::with_capacity(ls.len());
                let mut eqs = Vec::with_capacity(ls.len());
                // (A pair of constants stays a pair: the planner doesn't fold a row
                // comparison's columns.)
                let pair = |b: &mut Self, op: &str, x: Expr, y: Expr| -> Result<Expr> {
                    let c = b.binop(op, x.clone(), y.clone(), loc)?;
                    if !matches!(c, Expr::Const(..)) {
                        return Ok(c);
                    }
                    let res = resolve_binop(op, &x.ty(), &y.ty()).map_err(|e| with_pos(e, loc))?;
                    let x = b.coerce_implicit(x, &res.arg_types[0])?;
                    let y = b.coerce_implicit(y, &res.arg_types[1])?;
                    let def = b.collated(res.def, &[&x, &y]);
                    Ok(Expr::Call(def, vec![x, y], DataType::Bool))
                };
                for (a, b) in ls.into_iter().zip(rs) {
                    let c = pair(self, op, a.clone(), b.clone())?;
                    // (The `=` of the pair's family, over the operands as compared.)
                    let e = match &c {
                        Expr::Call(_, args, _) if args.len() == 2 => {
                            pair(self, "=", args[0].clone(), args[1].clone())
                        }
                        _ => pair(self, "=", a, b),
                    }?;
                    cmps.push(c);
                    eqs.push(e);
                }
                Ok(Expr::RowCompare(Box::new(crate::sql::expr::RowCompare { op, cmps, eqs })))
            }
            "<" | "<=" | ">" | ">=" | "~<~" | "~<=~" | "~>=~" | "~>~" => {
                // (a1, a2) < (b1, b2)  ≡  a1 < b1 OR (a1 = b1 AND a2 < b2) …, with <= on the last.
                let n = ls.len();
                let strict = match op {
                    "<" | "<=" => "<",
                    ">" | ">=" => ">",
                    "~<~" | "~<=~" => "~<~",
                    _ => "~>~",
                };
                let mut alts = Vec::new();
                for i in 0..n {
                    let mut conj = Vec::new();
                    for j in 0..i {
                        conj.push(self.binop("=", ls[j].clone(), rs[j].clone(), loc)?);
                    }
                    let last_op = if i == n - 1 { op } else { strict };
                    conj.push(self.binop(last_op, ls[i].clone(), rs[i].clone(), loc)?);
                    alts.push(if conj.len() == 1 {
                        conj.pop().unwrap()
                    } else {
                        Expr::And(conj)
                    });
                }
                Ok(if alts.len() == 1 {
                    alts.pop().unwrap()
                } else {
                    Expr::Or(alts)
                })
            }
            // make_row_comparison_op: an operator of no btree family has no row meaning.
            _ => Err(with_pos(
                Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!("could not determine interpretation of row comparison operator {op}"),
                )
                .hint("Row comparison operators must be associated with btree operator families."),
                loc,
            )),
        }
    }

    fn distinct(&mut self, l: Expr, r: Expr, not: bool, loc: i32) -> Result<Expr> {
        // An undecorated NULL on one side: a null test of the other
        // (make_nulltest_from_distinct), except on a composite, whose IS NOT NULL would
        // test its fields.
        let null_lit = |e: &Expr| matches!(e, Expr::Const(Value::Null, DataType::Unknown));
        let composite = |e: &Expr| {
            matches!(e, Expr::Row(..)) || matches!(e.ty(), DataType::Row(_) | DataType::Record)
        };
        if null_lit(&r) && !composite(&l) {
            return Ok(Expr::IsNull(Box::new(l), !not));
        }
        if null_lit(&l) && !composite(&r) {
            return Ok(Expr::IsNull(Box::new(r), !not));
        }
        // ROW constructors compare field by field (make_row_distinct_op); two whole-row
        // references compare as records.
        if let (Expr::Row(ls, _), Expr::Row(rs, _)) = (&l, &r)
            && !(is_whole_row(&l) && is_whole_row(&r))
        {
            let parts = ls
                .iter()
                .zip(rs)
                .map(|(a, b)| self.distinct(a.clone(), b.clone(), not, loc))
                .collect::<Result<Vec<_>>>()?;
            return Ok(if not {
                Expr::And(parts)
            } else {
                Expr::Or(parts)
            });
        }
        let (def, types) = self.equality_op(&l, &r, loc)?;
        let l = self.coerce_implicit(l, &types[0])?;
        let r = self.coerce_implicit(r, &types[1])?;
        let def = self.collated(def, &[&l, &r]);
        let e = Expr::IsDistinct(Box::new(l), Box::new(r), not, def);
        // eval_const_expressions: a DistinctExpr over constants is evaluated (so `FULL JOIN
        // ... ON 1 IS DISTINCT FROM 1` is a constant-false clause).
        if let Expr::IsDistinct(l, r, _, def) = &e
            && !self.deparse
            && !self.unplanned
            && def.volatility == 'i'
            && matches!(**l, Expr::Const(..))
            && matches!(**r, Expr::Const(..))
            && let Ok(v) = eval(&e, &[], self.ctx)
        {
            return Ok(Expr::Const(v, DataType::Bool));
        }
        Ok(e)
    }

    /// The `=` IS DISTINCT FROM and NULLIF compare with (make_distinct_op's
    /// make_op): an extension's or a user's operator as `oper` picks it, else the built-in;
    /// with the argument types it takes.
    fn equality_op(&mut self, l: &Expr, r: &Expr, loc: i32) -> Result<(Arc<FuncDef>, Vec<DataType>)> {
        if let Some((def, types)) = self.user_op_def("=", &l.ty(), &r.ty(), loc)? {
            return Ok((def, types));
        }
        let res = resolve_binop("=", &l.ty(), &r.ty()).map_err(|e| with_pos(e, loc))?;
        Ok((res.def, res.arg_types.to_vec()))
    }

    fn in_list(&mut self, l: Expr, items: Vec<Expr>, not: bool, loc: i32) -> Result<Expr> {
        if let Expr::Row(..) = l {
            // (transformAExprIn: an unknown literal takes a named row type's.)
            let lty = l.ty();
            let items = items
                .into_iter()
                .map(|it| {
                    if matches!(it.ty(), DataType::Unknown)
                        && matches!(&lty, DataType::Row(rt) if rt.oid != 0)
                    {
                        self.coerce_implicit(it, &lty)
                    } else {
                        Ok(it)
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            // A whole-row Var (not a ROW() constructor) against constants of its type: one
            // `= ANY (array)` (transformAExprIn's ScalarArrayOpExpr).
            if !self.deparse
                && items.len() > 1
                && crate::sql::binder::is_whole_row(&l)
                && matches!(&lty, DataType::Row(rt) if rt.oid != 0)
                && items.iter().all(|it| matches!(it, Expr::Const(_, t) if *t == lty))
            {
                let vals: Vec<Value> = items
                    .iter()
                    .map(|it| match it {
                        Expr::Const(v, _) => v.clone(),
                        _ => Value::Null,
                    })
                    .collect();
                let arr = Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(lty.clone(), vals)));
                let right = Expr::Const(arr, DataType::array_of(lty.clone()));
                if let Ok(e) = self.array_op(if not { "<>" } else { "=" }, l.clone(), right, not, loc) {
                    return Ok(e);
                }
            }
            let parts = items
                .into_iter()
                .map(|it| self.binop(if not { "<>" } else { "=" }, l.clone(), it, loc))
                .collect::<Result<Vec<_>>>()?;
            return Ok(if not {
                Expr::And(parts)
            } else {
                Expr::Or(parts)
            });
        }
        // (transformAExprIn: values of an array type have no array type of their own to
        // probe, get_array_type: the comparisons ORed, NOT IN's `<>` ANDed.)
        if items.len() > 1 && items.iter().filter(|e| e.columns().is_empty()).count() > 1 {
            let mut types = vec![l.ty()];
            types.extend(items.iter().map(|e| e.ty()));
            if super::unify_types(&types, "IN").is_ok_and(|t| t.is_array()) {
                let op = if not { "<>" } else { "=" };
                let parts = items
                    .into_iter()
                    .map(|it| self.binop(op, l.clone(), it, loc))
                    .collect::<Result<Vec<_>>>()?;
                return Ok(if not { Expr::And(parts) } else { Expr::Or(parts) });
            }
        }
        if self.deparse {
            return self.in_list_parsed(l, items, not, loc);
        }
        // Values of a type whose `=` is a user-defined operator (an extension's type):
        // constants of the common type probed as one array, others compared one by one.
        if !self.cat.operators.is_empty() {
            let mut types = vec![l.ty()];
            types.extend(items.iter().map(|e| e.ty()));
            if let Ok(ty) = super::unify_types(&types, "IN")
                && self.user_op_def("=", &ty, &ty, loc)?.is_some()
            {
                let consts = items.len() > 1 && items.iter().all(|i| matches!(i, Expr::Const(..)));
                if consts {
                    let mut vals = Vec::with_capacity(items.len());
                    for it in items {
                        match self.coerce_implicit(it, &ty)? {
                            Expr::Const(v, _) => vals.push(v),
                            _ => return Err(crate::error::internal("IN constant")),
                        }
                    }
                    let arr = Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(ty.clone(), vals)));
                    let right = Expr::Const(arr, DataType::array_of(ty));
                    return self.array_op(if not { "<>" } else { "=" }, l, right, not, loc);
                }
                let op = if not { "<>" } else { "=" };
                let parts = items
                    .into_iter()
                    .map(|it| self.binop(op, l.clone(), it, loc))
                    .collect::<Result<Vec<_>>>()?;
                return Ok(if not { Expr::And(parts) } else { Expr::Or(parts) });
            }
        }
        // Prefer one common type for all values.
        let mut all = vec![l.clone()];
        all.extend(items.iter().cloned());
        let types: Vec<DataType> = all.iter().map(|e| e.ty()).collect();
        if let Ok(ty) = super::unify_types(&types, "IN")
            && let Ok(res) = resolve_binop("=", &ty, &ty)
        {
            // (Values not all constants, of a common type the operator takes only coerced
            // (varchar's `=` is text's): the array of them coerced as a whole, as the
            // parser leaves it (an ArrayCoerceExpr, which partition pruning can't use).)
            if res.arg_types[1] != ty.without_typmod()
                && matches!(ty, DataType::Varchar(_))
                && items.len() > 1
                && items.iter().all(|i| i.columns().is_empty())
                && !items.iter().all(|i| matches!(i, Expr::Const(..)))
            {
                return self.in_list_parsed(l, items, not, loc);
            }
            // transformAExprIn: the values take the common type first (an unknown literal
            // read as regclass, say), then the operator's.
            let left = self.coerce_implicit(l.clone(), &res.arg_types[0]);
            let rights: Result<Vec<Expr>> = items
                .iter()
                .map(|i| {
                    let i = if matches!(i.ty(), DataType::Unknown) && ty != res.arg_types[1] {
                        self.coerce_implicit(i.clone(), &ty)?
                    } else {
                        i.clone()
                    };
                    self.coerce_implicit(i, &res.arg_types[1])
                })
                .collect();
            if let (Ok(left), Ok(rights)) = (left, rights) {
                // All constants: one array probe.
                if !self.deparse
                    && rights.iter().all(|r| matches!(r, Expr::Const(..)))
                    && rights.len() > 1
                {
                    let elem = res.arg_types[1].clone();
                    let vals: Vec<Value> = rights
                        .iter()
                        .map(|r| {
                            if let Expr::Const(v, _) = r {
                                v.clone()
                            } else {
                                Value::Null
                            }
                        })
                        .collect();
                    let arr = Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(
                        elem.clone(),
                        vals,
                    )));
                    let right = Expr::Const(arr, DataType::array_of(elem));
                    // NOT IN: `x <> ALL (array)`, as Postgres's parser makes it.
                    if not
                        && let Ok(ne) = resolve_binop("<>", &res.arg_types[0], &res.arg_types[1])
                        && ne.arg_types == res.arg_types
                    {
                        let op = self.collated(ne.def.clone(), &[&left, &right]);
                        return Ok(Expr::ArrayOp {
                            left: Box::new(left),
                            right: Box::new(right),
                            op,
                            all: true,
                        });
                    }
                    let op = self.collated(res.def.clone(), &[&left, &right]);
                    let e = Expr::ArrayOp {
                        left: Box::new(left),
                        right: Box::new(right),
                        op,
                        all: false,
                    };
                    return Ok(if not { Expr::Not(Box::new(e)) } else { e });
                }
                let list = rights
                    .into_iter()
                    .map(|r| {
                        let def = self.collated(res.def.clone(), &[&left, &r]);
                        (r, def)
                    })
                    .collect();
                return Ok(Expr::InList(Box::new(left), list, not));
            }
        }
        // A type with only a user-defined `=` (and `<>`): `x = ANY (...)` with it, the
        // comparisons ORed (NOT IN: `<> ALL`, ANDed).
        if items
            .iter()
            .any(|it| resolve_binop("=", &l.ty(), &it.ty()).is_err())
        {
            let op = if not { "<>" } else { "=" };
            let parts = items
                .into_iter()
                .map(|it| self.binop(op, l.clone(), it, loc))
                .collect::<Result<Vec<_>>>()?;
            return Ok(if not {
                Expr::And(parts)
            } else {
                Expr::Or(parts)
            });
        }
        // (Values of no common type: each compared with its own coercion of the left side,
        // ORed, as transformAExprIn's fallback; NOT IN's `<>` ANDed.)
        let mut types = vec![l.ty()];
        types.extend(items.iter().map(|e| e.ty()));
        if items.len() > 1 && super::unify_types(&types, "IN").is_err() {
            let op = if not { "<>" } else { "=" };
            let parts = items
                .into_iter()
                .map(|it| self.binop(op, l.clone(), it, loc))
                .collect::<Result<Vec<_>>>()?;
            return Ok(if not { Expr::And(parts) } else { Expr::Or(parts) });
        }
        let mut list = Vec::new();
        let mut left = l;
        for it in items {
            let res = resolve_binop("=", &left.ty(), &it.ty()).map_err(|e| with_pos(e, loc))?;
            left = self.coerce_implicit(left, &res.arg_types[0])?;
            let it = self.coerce_implicit(it, &res.arg_types[1])?;
            let def = self.collated(res.def, &[&left, &it]);
            list.push((it, def));
        }
        Ok(Expr::InList(Box::new(left), list, not))
    }

    /// IN as Postgres's parser leaves it (what deparsing shows): the items that read no column
    /// of this query compared at once when there are several (`x = ANY (ARRAY[…])`, the array
    /// of their common type), the others one by one, ORed (NOT IN: `<>`, ANDed).
    fn in_list_parsed(&mut self, l: Expr, items: Vec<Expr>, not: bool, loc: i32) -> Result<Expr> {
        let op = if not { "<>" } else { "=" };
        let consts: Vec<Expr> = items
            .iter()
            .filter(|e| e.columns().is_empty())
            .cloned()
            .collect();
        let mut parts: Vec<Expr> = Vec::new();
        let mut rest = items;
        if consts.len() > 1 {
            let mut types = vec![l.ty()];
            types.extend(consts.iter().map(|e| e.ty()));
            if let Ok(common) = super::unify_types(&types, "IN") {
                let common = common.without_typmod();
                let elems = consts
                    .into_iter()
                    .map(|e| self.coerce_implicit(e, &common))
                    .collect::<Result<Vec<_>>>()?;
                parts.push(self.array_op(
                    op,
                    l.clone(),
                    Expr::Array(elems, DataType::array_of(common)),
                    not,
                    loc,
                )?);
                rest.retain(|e| !e.columns().is_empty());
            }
        }
        for it in rest {
            parts.push(self.binop(op, l.clone(), it, loc)?);
        }
        // Each comparison joins what came before (`((a OR b) OR c)`).
        let mut it = parts.into_iter();
        let first = it
            .next()
            .ok_or_else(|| crate::error::internal("empty IN list"))?;
        Ok(it.fold(first, |acc, p| {
            if not {
                Expr::And(vec![acc, p])
            } else {
                Expr::Or(vec![acc, p])
            }
        }))
    }

    fn array_op(&mut self, op: &str, l: Expr, r: Expr, all: bool, loc: i32) -> Result<Expr> {
        let rt = r.ty();
        let r = match rt {
            DataType::Array(_) => r,
            // int2vector / oidvector are arrays of int2 / oid.
            DataType::Int2Vector => self.coerce_implicit(r, &DataType::array_of(DataType::Int2))?,
            DataType::OidVector => self.coerce_implicit(r, &DataType::array_of(DataType::Oid))?,
            // An untyped literal takes the array type of the operator's right input.
            DataType::Unknown => {
                let user = self.user_op_def(op, &l.ty(), &DataType::Unknown, loc)?;
                let res =
                    resolve_binop(op, &l.ty(), &DataType::Unknown).map_err(|e| with_pos(e, loc));
                let elem = match (user, res) {
                    (Some((_, types)), _) => types[1].clone(),
                    (None, res) => res?.arg_types[1].clone(),
                };
                let elem = if elem.is_polymorphic() || matches!(elem, DataType::Unknown) {
                    if matches!(l.ty(), DataType::Unknown) {
                        DataType::Text
                    } else {
                        l.ty()
                    }
                } else {
                    elem
                };
                self.coerce_implicit(r, &DataType::array_of(elem))?
            }
            _ => {
                return Err(with_pos(
                    Error::new(
                        code::WRONG_OBJECT_TYPE,
                        "op ANY/ALL (array) requires array on right side",
                    ),
                    loc,
                ));
            }
        };
        let elem = r.ty().elem().cloned().unwrap_or(DataType::Text);
        // A user-defined operator (an extension's type's `=`).
        if let Some((def, types)) = self.user_op_def(op, &l.ty(), &elem, loc)? {
            if !matches!(def.ret, func::Ret::Fixed(DataType::Bool)) {
                return Err(with_pos(
                    Error::new(code::WRONG_OBJECT_TYPE, "op ANY/ALL (array) requires operator to yield boolean"),
                    loc,
                ));
            }
            let l = self.coerce_implicit(l, &types[0])?;
            let r = self.coerce_implicit(r, &DataType::array_of(types[1].clone()))?;
            let op = self.collated(def, &[&l, &r]);
            return Ok(Expr::ArrayOp {
                left: Box::new(l),
                right: Box::new(r),
                op,
                all,
            });
        }
        // An operator whose function is a user routine (bound as its body: no function to
        // call per element) over an array whose elements are known: each element compared,
        // ORed (ALL: ANDed), which is what ScalarArrayOpExpr gives too, NULLs included.
        let known: Option<Vec<Expr>> = match &r {
            Expr::Array(items, _) => Some(items.clone()),
            Expr::Const(Value::Array(a), _) if a.dims.len() <= 1 => {
                Some(a.items.iter().map(|v| Expr::Const(v.clone(), elem.clone())).collect())
            }
            _ => None,
        };
        if let Some(items) = known
            && !self.cat.operators.is_empty()
        {
            let probe = |t: &DataType| Expr::Const(Value::Null, t.clone());
            if self.user_operator(None, op, Some(probe(&l.ty())), probe(&elem), loc)?.is_some() {
                let mut parts = Vec::with_capacity(items.len());
                for it in items {
                    parts.push(self.binop(op, l.clone(), it, loc)?);
                }
                return Ok(match parts.len() {
                    0 => Expr::Const(Value::Bool(all), DataType::Bool),
                    _ if all => Expr::And(parts),
                    _ => Expr::Or(parts),
                });
            }
        }
        let res = resolve_binop(op, &l.ty(), &elem).map_err(|e| with_pos(e, loc))?;
        if !matches!(res.ret, DataType::Bool) {
            return Err(with_pos(
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    "op ANY/ALL (array) requires operator to yield boolean",
                ),
                loc,
            ));
        }
        // A record operator takes the composite as it is.
        let l = if matches!(res.arg_types[0], DataType::Record) && matches!(l.ty(), DataType::Row(_)) {
            l
        } else {
            self.coerce_implicit(l, &res.arg_types[0])?
        };
        let r = if matches!(res.arg_types[1], DataType::Record) && matches!(elem, DataType::Row(_)) {
            r
        } else {
            self.coerce_implicit(r, &DataType::array_of(res.arg_types[1].clone()))?
        };
        let op = self.collated(res.def, &[&l, &r]);
        Ok(Expr::ArrayOp {
            left: Box::new(l),
            right: Box::new(r),
            op,
            all,
        })
    }

    fn bind_bool_expr(&mut self, b: &pb::BoolExpr) -> Result<Expr> {
        let op = pb::BoolExprType::try_from(b.boolop).unwrap_or(pb::BoolExprType::AndExpr);
        let what = match op {
            pb::BoolExprType::AndExpr => "AND",
            pb::BoolExprType::OrExpr => "OR",
            _ => "NOT",
        };
        let mut args = Vec::with_capacity(b.args.len());
        for a in &b.args {
            let e = self.bind_expr(a)?;
            args.push(self.coerce_bool_at(e, what, a)?);
        }
        // (A constant deciding the AND / OR after an argument that failed to fold:
        // simplify_and_arguments simplified that argument first, so planning the level
        // fails; the constant goes, so nothing makes a dummy relation of the clause first.)
        // (simplify_and_arguments: the arguments after the constant deciding the clause
        // are never simplified, so nothing folding them fails.)
        if matches!(op, pb::BoolExprType::AndExpr | pb::BoolExprType::OrExpr)
            && !self.deparse
            && let Some(k) = args
                .iter()
                .position(|a| matches!(a, Expr::Const(Value::Bool(b), _) if *b == (op == pb::BoolExprType::OrExpr)))
            && args[k + 1..].iter().any(|a| super::fold::candidates_in(std::slice::from_ref(a)).len() > 0)
        {
            args.truncate(k + 1);
        }
        if matches!(op, pb::BoolExprType::AndExpr | pb::BoolExprType::OrExpr) && self.folds_failed() {
            let decides = Value::Bool(op == pb::BoolExprType::OrExpr);
            if let Some(first) = args.iter().position(|a| super::fold::fails(a, self.ctx, self.fold_err.as_deref().unwrap_or(&[]))) {
                let mut i = 0;
                args.retain(|a| {
                    i += 1;
                    i - 1 <= first || !matches!(a, Expr::Const(v, _) if *v == decides)
                });
            }
        }
        Ok(match op {
            pb::BoolExprType::AndExpr => Expr::And(args),
            pb::BoolExprType::OrExpr => Expr::Or(args),
            _ => {
                let a = args.pop().unwrap();
                match a {
                    Expr::Const(Value::Bool(v), t) => Expr::Const(Value::Bool(!v), t),
                    a => Expr::Not(Box::new(a)),
                }
            }
        })
    }

    /// A WHERE / JOIN ON condition as eval_const_expressions leaves it: NOT pushed in
    /// (`negate_clause`) and `x = true` / `x = false` made x / NOT x
    /// (simplify_boolean_equality), through AND / OR / NOT. (Only quals: GROUP BY matching
    /// of a select list or HAVING needs the expressions as written.)
    /// A WHERE / JOIN ON qual as the planner gets it: `simplify_qual`, then each
    /// top-level `X = X` (strict, X not volatile) made `X IS NOT NULL`, as an equivalence
    /// class with one member gives it back (process_equivalence).
    pub(crate) fn simplify_where(&mut self, e: Expr) -> Expr {
        let e = self.simplify_qual(e);
        if self.deparse {
            return e;
        }
        // The rest of find_duplicate_ors: ANDs and ORs flattened, and the clauses every arm of
        // an OR has taken out in front of it (process_duplicate_ors: `(A AND B) OR (A AND C)`
        // is `A AND (B OR C)`, so A can be an index or join clause).
        fn factor_ors(e: Expr) -> Expr {
            match e {
                Expr::Or(xs) => {
                    let mut list = Vec::new();
                    for x in xs.into_iter().map(factor_ors) {
                        match x {
                            Expr::Or(ys) => list.extend(ys),
                            x => list.push(x),
                        }
                    }
                    duplicate_ors(list)
                }
                Expr::And(xs) => {
                    let mut list = Vec::new();
                    for x in xs.into_iter().map(factor_ors) {
                        match x {
                            Expr::And(ys) => list.extend(ys),
                            x => list.push(x),
                        }
                    }
                    Expr::And(list)
                }
                e => e,
            }
        }
        fn duplicate_ors(orlist: Vec<Expr>) -> Expr {
            if orlist.len() < 2 {
                return orlist.into_iter().next().unwrap_or(Expr::Const(Value::Bool(false), DataType::Bool));
            }
            let args = |x: &Expr| -> Vec<Expr> {
                match x {
                    Expr::And(ys) => ys.clone(),
                    x => vec![x.clone()],
                }
            };
            // The shortest AND as the reference (a non-AND arm is one clause: it wins).
            let mut reference: Vec<Expr> = Vec::new();
            for x in &orlist {
                match x {
                    Expr::And(ys) => {
                        if reference.is_empty() || ys.len() < reference.len() {
                            reference = ys.clone();
                        }
                    }
                    x => {
                        reference = vec![x.clone()];
                        break;
                    }
                }
            }
            let mut uniq: Vec<Expr> = Vec::new();
            for r in reference {
                if !uniq.iter().any(|u| u.same_as(&r)) {
                    uniq.push(r);
                }
            }
            let winners: Vec<Expr> = uniq
                .into_iter()
                .filter(|r| orlist.iter().all(|x| args(x).iter().any(|a| a.same_as(r))))
                .collect();
            if winners.is_empty() {
                return Expr::Or(orlist);
            }
            // The arms less the winners; an arm left empty makes the OR redundant.
            let mut neworlist = Vec::new();
            let mut degenerate = false;
            for x in &orlist {
                let rest: Vec<Expr> = args(x).into_iter().filter(|a| !winners.iter().any(|w| w.same_as(a))).collect();
                match rest.len() {
                    0 => {
                        degenerate = true;
                        break;
                    }
                    1 => neworlist.extend(rest),
                    _ => neworlist.push(Expr::And(rest)),
                }
            }
            let mut out = winners;
            if !degenerate {
                if neworlist.len() == 1 {
                    out.extend(neworlist);
                } else {
                    let mut flat = Vec::new();
                    for x in neworlist {
                        match x {
                            Expr::Or(ys) => flat.extend(ys),
                            x => flat.push(x),
                        }
                    }
                    out.push(Expr::Or(flat));
                }
            }
            let mut flat = Vec::new();
            for x in out {
                match x {
                    Expr::And(ys) => flat.extend(ys),
                    x => flat.push(x),
                }
            }
            if flat.len() == 1 { flat.pop().unwrap() } else { Expr::And(flat) }
        }
        // canonicalize_qual's find_duplicate_ors (not a CHECK): NULL is as good as FALSE.
        fn drop_nulls(e: Expr, failing: &dyn Fn(&[Expr]) -> bool) -> Expr {
            let falsy = |x: &Expr| matches!(x, Expr::Const(Value::Null, _) | Expr::Const(Value::Bool(false), _));
            match e {
                Expr::Or(xs) => {
                    let mut out = Vec::new();
                    for x in xs.into_iter().map(|x| drop_nulls(x, failing)) {
                        if matches!(x, Expr::Const(Value::Bool(true), _)) {
                            if failing(&out) {
                                break;
                            }
                            return x;
                        }
                        if !falsy(&x) {
                            out.push(x);
                        }
                    }
                    match out.len() {
                        0 => Expr::Const(Value::Bool(false), DataType::Bool),
                        1 => out.pop().unwrap(),
                        _ => Expr::Or(out),
                    }
                }
                Expr::And(xs) => {
                    let mut out = Vec::new();
                    for x in xs.into_iter().map(|x| drop_nulls(x, failing)) {
                        if falsy(&x) {
                            if failing(&out) {
                                break;
                            }
                            return Expr::Const(Value::Bool(false), DataType::Bool);
                        }
                        if !matches!(x, Expr::Const(Value::Bool(true), _)) {
                            out.push(x);
                        }
                    }
                    match out.len() {
                        0 => Expr::Const(Value::Bool(true), DataType::Bool),
                        1 => out.pop().unwrap(),
                        _ => Expr::And(out),
                    }
                }
                Expr::Const(Value::Null, _) => Expr::Const(Value::Bool(false), DataType::Bool),
                e => e,
            }
        }
        // (Simplified before: an argument that failed to fold stays, and fails the
        // statement.)
        let check = self.folds_failed();
        let ctx = self.ctx;
        let pending = self.fold_err.clone().unwrap_or_default();
        let failing = |xs: &[Expr]| check && xs.iter().any(|x| super::fold::fails(x, ctx, &pending));
        let e = drop_nulls(e, &failing);
        let e = factor_ors(e);
        let fix = |q: Expr| match q {
            Expr::Call(d, args, ty)
                if d.name == "="
                    && d.strict
                    && args.len() == 2
                    && args[0].same_as(&args[1])
                    && !args[0].is_volatile()
                    // (A pseudoconstant clause is no equivalence class's: it gates the plan.)
                    && !args[0].columns().is_empty()
                    // (An outer query's value is a Param here, not an equivalence class
                    // member: the clause stays.)
                    && !args[0].has_outer_refs()
                    && args[0].ty() == args[1].ty()
                    && (super::optimize::hashable_type(&args[0].ty())
                        || matches!(args[0].ty(), DataType::Numeric(..))) =>
            {
                let mut args = args;
                let _ = ty;
                Expr::IsNull(Box::new(args.swap_remove(0)), true)
            }
            q => q,
        };
        match e {
            Expr::And(xs) => Expr::And(xs.into_iter().map(fix).collect()),
            e => fix(e),
        }
    }

    pub(crate) fn simplify_qual(&mut self, e: Expr) -> Expr {
        if self.deparse {
            return e;
        }
        match e {
            // simplify_and_arguments / simplify_or_arguments: nested lists flattened, the
            // neutral constant dropped, the absorbing one taking the whole clause.
            e @ (Expr::And(_) | Expr::Or(_)) => {
                let (and, args) = match e {
                    Expr::And(a) => (true, a),
                    Expr::Or(a) => (false, a),
                    _ => unreachable!(),
                };
                let mut out = Vec::new();
                let mut null = None;
                let mut stack: Vec<Expr> = args.into_iter().rev().collect();
                while let Some(a) = stack.pop() {
                    match self.simplify_qual(a) {
                        Expr::And(xs) if and => stack.extend(xs.into_iter().rev()),
                        Expr::Or(xs) if !and => stack.extend(xs.into_iter().rev()),
                        Expr::Const(Value::Bool(b), t) => {
                            if b != and {
                                // (The arguments before it were simplified: one that
                                // failed to fold fails the statement if the level is
                                // planned, so they stay, without the constant a dummy
                                // relation would be made of.)
                                if self.folds_failed() && out.iter().any(|x| super::fold::fails(x, self.ctx, self.fold_err.as_deref().unwrap_or(&[]))) {
                                    break;
                                }
                                return Expr::Const(Value::Bool(b), t);
                            }
                        }
                        c @ Expr::Const(Value::Null, _) => null = Some(c),
                        x => out.push(x),
                    }
                }
                out.extend(null);
                match out.len() {
                    0 => Expr::Const(Value::Bool(and), DataType::Bool),
                    1 => out.pop().unwrap(),
                    _ if and => Expr::And(out),
                    _ => Expr::Or(out),
                }
            }
            Expr::Not(x) => {
                let x = self.simplify_qual(*x);
                self.negate_clause(x)
            }
            // evaluate_function: a strict call with a NULL constant argument is NULL (its
            // arguments simplified first: one failing to fold keeps the call).
            Expr::Call(def, args, ty)
                if def.strict
                    && !def.is_srf()
                    && args.iter().any(|a| matches!(a, Expr::Const(Value::Null, _)))
                    && !(self.folds_failed() && args.iter().any(|a| super::fold::fails(a, self.ctx, self.fold_err.as_deref().unwrap_or(&[])))) =>
            {
                let _ = (def, args);
                Expr::Const(Value::Null, ty)
            }
            Expr::Call(def, args, ty)
                if args.len() == 2
                    && matches!(def.name, "=" | "<>")
                    && args[0].ty() == DataType::Bool
                    && args[1].ty() == DataType::Bool =>
            {
                let (x, b) = match (&args[0], &args[1]) {
                    (Expr::Const(Value::Bool(b), _), x) | (x, Expr::Const(Value::Bool(b), _))
                        if !matches!(x, Expr::Const(..)) =>
                    {
                        (x.clone(), *b)
                    }
                    _ => return Expr::Call(def, args, ty),
                };
                let x = self.simplify_qual(x);
                if b == (def.name == "=") {
                    x
                } else {
                    self.negate_clause(x)
                }
            }
            e => e,
        }
    }

    /// negate_clause, as eval_const_expressions applies it to NOT: a comparison becomes its
    /// negator, NOT NOT x is x, AND / OR follow De Morgan, IS [NOT] NULL and boolean tests
    /// flip; anything else stays under NOT.
    fn negate_clause(&mut self, e: Expr) -> Expr {
        match e {
            Expr::Const(Value::Bool(v), t) => Expr::Const(Value::Bool(!v), t),
            Expr::Not(x) => *x,
            Expr::And(args) => Expr::Or(args.into_iter().map(|a| self.negate_clause(a)).collect()),
            Expr::Or(args) => Expr::And(args.into_iter().map(|a| self.negate_clause(a)).collect()),
            Expr::IsNull(x, not) if !matches!(x.ty(), DataType::Row(..) | DataType::Record) => {
                Expr::IsNull(x, !not)
            }
            Expr::BoolTest(x, t) => {
                use crate::sql::expr::BoolTest as B;
                let t = match t {
                    B::IsTrue => B::IsNotTrue,
                    B::IsNotTrue => B::IsTrue,
                    B::IsFalse => B::IsNotFalse,
                    B::IsNotFalse => B::IsFalse,
                    B::IsUnknown => B::IsNotUnknown,
                    B::IsNotUnknown => B::IsUnknown,
                };
                Expr::BoolTest(x, t)
            }
            Expr::Call(def, args, ty)
                if args.len() == 2
                    && matches!(def.name, "=" | "<>" | "<" | "<=" | ">" | ">=")
                    && args[0].ty().without_typmod() == args[1].ty().without_typmod() =>
            {
                let neg = match def.name {
                    "=" => "<>",
                    "<>" => "=",
                    "<" => ">=",
                    "<=" => ">",
                    ">" => "<=",
                    _ => "<",
                };
                match resolve_binop(neg, &args[0].ty(), &args[1].ty()) {
                    Ok(res) if res.arg_types[0] == args[0].ty() && res.arg_types[1] == args[1].ty() => {
                        let d = self.collated(res.def.clone(), &[&args[0], &args[1]]);
                        Expr::Call(d, args, res.ret)
                    }
                    _ => Expr::Not(Box::new(Expr::Call(def, args, ty))),
                }
            }
            Expr::ArrayOp {
                left,
                right,
                op,
                all,
            } if matches!(op.name, "=" | "<>") => {
                let neg = if op.name == "=" { "<>" } else { "=" };
                let elem = match right.ty() {
                    DataType::Array(t) => *t,
                    t => t,
                };
                match resolve_binop(neg, &left.ty(), &elem) {
                    Ok(res) if res.arg_types[0] == left.ty() && res.arg_types[1] == elem => {
                        let d = self.collated(res.def.clone(), &[&left, &right]);
                        Expr::ArrayOp {
                            left,
                            right,
                            op: d,
                            all: !all,
                        }
                    }
                    _ => Expr::Not(Box::new(Expr::ArrayOp {
                        left,
                        right,
                        op,
                        all,
                    })),
                }
            }
            e => Expr::Not(Box::new(e)),
        }
    }

    fn bind_case(&mut self, c: &pb::CaseExpr) -> Result<Expr> {
        let operand = match c.arg.as_deref() {
            Some(a) => Some(self.bind_expr(a)?),
            None => None,
        };
        // The arms bind with folding errors deferred: only the arms eval_const_expressions
        // reaches are simplified (and can fail) below.
        let outer_defer = self.defer_fold;
        self.defer_fold = true;
        let bound = self.bind_case_arms(c, &operand);
        self.defer_fold = outer_defer;
        let (conds, results, else_, ty) = bound?;
        if self.deparse {
            return Ok(Expr::Case {
                operand: operand.map(Box::new),
                whens: conds.into_iter().zip(results).collect(),
                else_,
                ty,
            });
        }
        // The CASE's type stays (a typmod the branches disagree on is dropped).
        let typed = |r: Expr| {
            if r.ty() == ty {
                r
            } else {
                Expr::Relabel(Box::new(r), ty.clone())
            }
        };
        // eval_const_expressions: a constant operand goes into the WHEN comparisons; an
        // arm whose condition folds to false or NULL is dropped, one that folds to true
        // ends the CASE as its default.
        let const_operand = match &operand {
            Some(o @ Expr::Const(..)) => Some(o.clone()),
            _ => None,
        };
        let mut whens = Vec::new();
        let mut default = None;
        for (cond, res) in conds.into_iter().zip(results) {
            let cond = match &const_operand {
                Some(v) => self.refold(substitute_case_test(cond, v))?,
                None => cond,
            };
            self.check_deferred(&cond)?;
            match &cond {
                Expr::Const(Value::Bool(false) | Value::Null, _) => continue,
                Expr::Const(Value::Bool(true), _) => {
                    self.check_deferred(&res)?;
                    default = Some(res);
                    break;
                }
                _ => {}
            }
            self.check_deferred(&res)?;
            whens.push((cond, res));
        }
        let default = match default {
            Some(d) => d,
            None => match else_ {
                Some(e) => {
                    self.check_deferred(&e)?;
                    *e
                }
                None => Expr::Const(Value::Null, ty.clone()),
            },
        };
        if whens.is_empty() {
            return Ok(typed(default));
        }
        let else_ = match default {
            Expr::Const(Value::Null, _) if c.defresult.is_none() => None,
            d => Some(Box::new(d)),
        };
        Ok(Expr::Case {
            operand: if const_operand.is_some() {
                None
            } else {
                operand.map(Box::new)
            },
            whens,
            else_,
            ty,
        })
    }

    /// A CASE's WHEN conditions, results, ELSE and type (transformCaseExpr).
    fn bind_case_arms(
        &mut self,
        c: &pb::CaseExpr,
        operand: &Option<Expr>,
    ) -> Result<(Vec<Expr>, Vec<Expr>, Option<Box<Expr>>, DataType)> {
        let mut conds = Vec::new();
        let mut results = Vec::new();
        for w in &c.args {
            let Some(NodeEnum::CaseWhen(w)) = inner(w) else {
                continue;
            };
            let cond = match operand {
                Some(op) => {
                    let v = self.bind_expr(w.expr.as_deref().unwrap())?;
                    let test = Expr::CaseTest(op.ty());
                    // (The CaseTestExpr has the operand's type, a domain included.)
                    let dom = self.declared_domain(op);
                    let _g = crate::scoped::set(&CASE_TEST_DOMAIN, dom, "expr::CASE_TEST_DOMAIN");
                    self.binop("=", test, v, w.location)?
                }
                None => {
                    let e = self.bind_expr(w.expr.as_deref().unwrap())?;
                    self.coerce_bool_at(e, "CASE/WHEN", w.expr.as_deref().unwrap())?
                }
            };
            conds.push(cond);
            results.push(self.bind_expr(w.result.as_deref().unwrap())?);
        }
        let has_else = c.defresult.is_some();
        if let Some(d) = c.defresult.as_deref() {
            results.push(self.bind_expr(d)?);
        }
        // The ELSE result counts first in choosing the type (transformCaseExpr).
        if has_else {
            results.rotate_right(1);
        }
        let (mut results, ty) = self.unify(results, "CASE")?;
        if has_else {
            results.rotate_left(1);
        }
        let else_ = if has_else {
            Some(Box::new(results.pop().unwrap()))
        } else {
            None
        };
        Ok((conds, results, else_, ty))
    }

    /// Fold what became constant (a CASE operand put into its WHEN comparisons): calls,
    /// casts and relabels over constants.
    fn refold(&mut self, e: Expr) -> Result<Expr> {
        e.transform(&mut |e| match &e {
            Expr::Call(..) => self.fold_call(e),
            Expr::Cast(x, ..) | Expr::Relabel(x, _) if x.is_const() && e.is_immutable() => {
                match eval(&e, &[], self.ctx) {
                    Ok(v) => {
                        let ty = e.ty();
                        Ok(Expr::Const(v, ty))
                    }
                    Err(err) => {
                        self.fold_failed(&e, err)?;
                        Ok(e)
                    }
                }
            }
            _ => Ok(e),
        })
    }

    /// Raise the error a constant call kept unfolded while its CASE arm's reachability was
    /// unknown (`defer_fold`) gives, now that the arm is known to be simplified.
    fn check_deferred(&mut self, e: &Expr) -> Result<()> {
        // (In a statement the failures wait for its plan, `fold_failed`.)
        if self.defer_fold || self.unplanned || self.deparse || self.fold_err.is_some() {
            return Ok(());
        }
        check_folds(e, self.ctx)
    }

    fn bind_sql_value_function(&mut self, f: &pb::SqlValueFunction) -> Result<Expr> {
        use pb::SqlValueFunctionOp as O;
        let op = O::try_from(f.op).unwrap_or(O::SvfopCurrentDate);
        let (name, with_prec) = match op {
            O::SvfopCurrentDate => ("current_date", false),
            O::SvfopCurrentTime => ("current_time", false),
            O::SvfopCurrentTimeN => ("current_time", true),
            O::SvfopCurrentTimestamp => ("current_timestamp", false),
            O::SvfopCurrentTimestampN => ("current_timestamp", true),
            O::SvfopLocaltime => ("localtime", false),
            O::SvfopLocaltimeN => ("localtime", true),
            O::SvfopLocaltimestamp => ("localtimestamp", false),
            O::SvfopLocaltimestampN => ("localtimestamp", true),
            O::SvfopCurrentRole => ("current_role", false),
            O::SvfopCurrentUser => ("current_user", false),
            O::SvfopUser => ("user", false),
            O::SvfopSessionUser => ("session_user", false),
            O::SvfopCurrentCatalog => ("current_catalog", false),
            O::SvfopCurrentSchema => ("current_schema", false),
            _ => return Err(crate::error::unsupported("SQL value function")),
        };
        if let Some(owner) = self.definer
            && matches!(op, O::SvfopCurrentRole | O::SvfopCurrentUser | O::SvfopUser)
        {
            let res = resolve_function("pg_get_userbyid", &[DataType::Oid])?;
            return self.call_resolved(res, vec![Expr::Const(Value::Oid(owner), DataType::Oid)]);
        }
        // anytime_typmod_check / anytimestamp_typmod_check.
        let mut typmod = f.typmod;
        if with_prec && typmod > 6 {
            let what = if name.ends_with("timestamp") { "TIMESTAMP" } else { "TIME" };
            let tz = if name.starts_with("current") { " WITH TIME ZONE" } else { "" };
            self.ctx.notice(crate::session::warning(
                code::INVALID_PARAMETER_VALUE,
                format!("{what}({typmod}){tz} precision reduced to maximum allowed, 6"),
            ));
            typmod = 6;
        }
        let args = if with_prec {
            vec![Expr::Const(Value::Int4(typmod), DataType::Int4)]
        } else {
            vec![]
        };
        let types: Vec<DataType> = args.iter().map(|a| a.ty()).collect();
        let res = resolve_function(name, &types)?;
        let mut e = self.call_resolved(res, args)?;
        // current_time(3) etc. round to the precision.
        if with_prec && let Expr::Call(_, ref mut a, ref mut ty) = e {
            let t = match &*ty {
                DataType::Time(_) => DataType::Time(typmod),
                DataType::Timetz(_) => DataType::Timetz(typmod),
                DataType::Timestamp(_) => DataType::Timestamp(typmod),
                DataType::Timestamptz(_) => DataType::Timestamptz(typmod),
                t => t.clone(),
            };
            let _ = a;
            *ty = t;
        }
        Ok(match e {
            Expr::Call(..) => Expr::SqlSyntax(Box::new(e)),
            e => e,
        })
    }

    fn bind_array_expr(
        &mut self,
        a: &pb::AArrayExpr,
        elem_hint: Option<&DataType>,
    ) -> Result<Expr> {
        let mut items = Vec::with_capacity(a.elements.len());
        for el in &a.elements {
            let e = match (inner(el), elem_hint) {
                // Nested ARRAY[...] inherit the hint.
                (Some(NodeEnum::AArrayExpr(sub)), Some(h)) => self.bind_array_expr(sub, Some(h))?,
                _ => self.bind_expr(el)?,
            };
            items.push(e);
        }
        if items.is_empty() {
            return match elem_hint {
                // Shown as written: ARRAY[]::t[].
                Some(h) if self.deparse => Ok(Expr::Array(vec![], DataType::array_of(h.clone()))),
                Some(h) => Ok(Expr::Const(
                    Value::Array(Arc::new(crate::types::array::ArrayValue::empty(h.clone()))),
                    DataType::array_of(h.clone()),
                )),
                None => Err(with_pos(
                    Error::new(
                        code::INDETERMINATE_DATATYPE,
                        "cannot determine type of empty array",
                    )
                    .hint("Explicitly cast to the desired type, for example ARRAY[]::integer[]."),
                    a.location,
                )),
            };
        }
        let sub_arrays = a
            .elements
            .iter()
            .all(|e| matches!(inner(e), Some(NodeEnum::AArrayExpr(_))));
        // Elements of one domain over an array type are scalars (type_is_array is false for
        // a domain): a one-dimensional array of array values.
        let scalar_arrays = elem_hint.is_none() && self.common_domain(&items).is_some_and(|d| {
            self.cat
                .domain_type(d)
                .is_some_and(|(x, array)| !array && x.base.is_array())
        });
        // transformArrayExpr: an element that is a sub-ARRAY or of an array type makes it
        // multidimensional; then the other elements are coerced to the array type.
        let multidims = a.elements.iter().zip(&items).any(|(n, i)| {
            matches!(inner(n), Some(NodeEnum::AArrayExpr(_))) || i.ty().is_array()
        });
        let (items, ty) = match elem_hint {
            Some(h) if !sub_arrays => {
                let h = if multidims {
                    DataType::array_of(h.clone())
                } else {
                    h.clone()
                };
                let items = items
                    .into_iter()
                    .map(|i| self.cast_explicit(i, &h))
                    .collect::<Result<Vec<_>>>()?;
                (items, h)
            }
            _ => self.unify(items, "ARRAY")?,
        };
        if ty.array_oid() == 0 && ty.oid() < crate::catalog::FIRST_USER_OID && !ty.is_array() {
            return Err(with_pos(
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("could not find array type for data type {}", ty.display()),
                ),
                a.location,
            ));
        }
        let arr_ty = if ty.is_array() && !scalar_arrays {
            ty.clone()
        } else {
            DataType::Array(Box::new(ty.clone()))
        };
        let elem = if ty.is_array() && !scalar_arrays {
            ty.elem().cloned().unwrap()
        } else {
            ty
        };
        if !self.deparse && items.iter().all(|i| matches!(i, Expr::Const(..))) {
            let vals = items
                .iter()
                .map(|i| {
                    if let Expr::Const(v, _) = i {
                        v.clone()
                    } else {
                        Value::Null
                    }
                })
                .collect();
            let v = crate::sql::expr::build_array(elem, vals)?;
            return Ok(Expr::Const(v, arr_ty));
        }
        Ok(Expr::Array(items, arr_ty))
    }

    fn bind_indirection(&mut self, ind: &pb::AIndirection) -> Result<Expr> {
        let mut base = self.bind_expr(ind.arg.as_deref().unwrap())?;
        let arg_loc = ind.arg.as_deref().map_or(-1, super::ast::location);
        // Where the first slice's bound is (for "does not support slices").
        let slice_loc = ind
            .indirection
            .iter()
            .find_map(|n| match n.node.as_ref() {
                Some(NodeEnum::AIndices(ai)) if ai.is_slice => ai
                    .uidx
                    .as_deref()
                    .or(ai.lidx.as_deref())
                    .map(super::ast::location),
                _ => None,
            })
            .unwrap_or(-1);
        // Each subscript's position, in order (for jsonb's subscript type error).
        let sub_locs: Vec<i32> = ind
            .indirection
            .iter()
            .filter_map(|n| match n.node.as_ref() {
                Some(NodeEnum::AIndices(ai)) => Some(
                    ai.uidx
                        .as_deref()
                        .or(ai.lidx.as_deref())
                        .map_or(-1, super::ast::location),
                ),
                _ => None,
            })
            .collect();
        let flushed = std::cell::Cell::new(0usize);
        let mut subs: Vec<(Option<Expr>, Option<Expr>, bool)> = Vec::new();
        let flush = |b: &mut Self,
                     base: Expr,
                     subs: &mut Vec<(Option<Expr>, Option<Expr>, bool)>|
         -> Result<Expr> {
            if subs.is_empty() {
                return Ok(base);
            }
            let bt = base.ty();
            let slice = subs.iter().any(|s| s.2);
            if matches!(bt, DataType::Array(_)) && subs.len() > 6 {
                return Err(Error::new(
                    code::PROGRAM_LIMIT_EXCEEDED,
                    format!(
                        "number of array dimensions ({}) exceeds the maximum allowed (6)",
                        subs.len()
                    ),
                ));
            }
            let ty = match &bt {
                DataType::Array(e) => {
                    if slice {
                        bt.clone()
                    } else {
                        (**e).clone()
                    }
                }
                DataType::Jsonb => {
                    // jsonb['key'] subscripting: a path lookup.
                    if slice {
                        return Err(with_pos(
                            Error::new(
                                code::DATATYPE_MISMATCH,
                                "jsonb subscript does not support slices",
                            ),
                            slice_loc,
                        ));
                    }
                    // jsonb_subscript_transform: each subscript as integer or text (an
                    // unknown literal as text); a SubscriptingRef of jsonb, which fetches
                    // as a #> path does (jsonb_subscript_fetch) and prints as subscripts.
                    let mut keys = Vec::new();
                    for (l, _, _) in subs.drain(..) {
                        let n = flushed.get();
                        flushed.set(n + 1);
                        let Some(l) = l else { continue };
                        let lt = l.ty();
                        let key = if matches!(lt, DataType::Unknown) {
                            b.coerce_implicit(l, &DataType::Text)?
                        } else if func::can_coerce(&lt, &DataType::Int4) {
                            b.coerce_implicit(l, &DataType::Int4)?
                        } else if func::can_coerce(&lt, &DataType::Text) {
                            b.coerce_implicit(l, &DataType::Text)?
                        } else {
                            return Err(with_pos(
                                Error::new(
                                    code::DATATYPE_MISMATCH,
                                    format!("subscript type {} is not supported", lt.display()),
                                )
                                .hint("jsonb subscript must be coercible to either integer or text."),
                                sub_locs.get(n).copied().unwrap_or(-1),
                            ));
                        };
                        keys.push((Some(key), None, false));
                    }
                    return Ok(Expr::Subscript(Box::new(base), keys, DataType::Jsonb));
                }
                // A type whose SUBSCRIPT handler is native code (hstore): its transform
                // (one subscript, of text) and fetch.
                t if let Some(ns) = crate::ext::native_subscript(&b.cat, t) => {
                    if slice || subs.len() != 1 {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("{} allows only one subscript", ns.what),
                        ));
                    }
                    let Some((Some(key), _, _)) = subs.pop() else {
                        return Err(crate::error::internal("subscript"));
                    };
                    flushed.set(flushed.get() + 1);
                    let at = sub_locs.get(flushed.get() - 1).copied().unwrap_or(-1);
                    let key = b.coerce_assign(key, &DataType::Text, "").map_err(|_| {
                        with_pos(
                            Error::new(
                                code::DATATYPE_MISMATCH,
                                format!("{} subscript must have type text", ns.what),
                            ),
                            at,
                        )
                    })?;
                    let def = native_subscript_def(ns.fetch, vec![bt.clone(), DataType::Text], true);
                    return Ok(Expr::Call(def, vec![base, key], DataType::Text));
                }
                DataType::Int2Vector => DataType::Int2,
                DataType::OidVector => DataType::Oid,
                t if t.fixed_elem().is_some() => {
                    if slice {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            "slices of fixed-length arrays not implemented",
                        ));
                    }
                    t.fixed_elem().unwrap()
                }
                t => {
                    return Err(with_pos(
                        Error::new(
                            code::DATATYPE_MISMATCH,
                            format!(
                                "cannot subscript type {} because it does not support subscripting",
                                t.display()
                            ),
                        ),
                        arg_loc,
                    ));
                }
            };
            let s = std::mem::take(subs);
            Ok(Expr::Subscript(Box::new(base), s, ty))
        };
        for i in &ind.indirection {
            match inner(i) {
                Some(NodeEnum::AIndices(ix)) => {
                    let idx = |n: &Option<Box<Node>>, b: &mut Self| -> Result<Option<Expr>> {
                        match n.as_deref() {
                            Some(n) => {
                                let e = b.bind_expr(n)?;
                                // (A type's own subscripting takes the subscript as is.)
                                if matches!(base.ty(), DataType::Jsonb)
                                    || crate::ext::native_subscript(&b.cat, &base.ty()).is_some()
                                {
                                    return Ok(Some(e));
                                }
                                // (transformContainerSubscripts: an assignment
                                // coercion, so bigint subscripts are fine.)
                                Ok(Some(b.coerce_assign(e, &DataType::Int4, "").map_err(
                                    |_| {
                                        Error::new(
                                            code::DATATYPE_MISMATCH,
                                            "array subscript must have type integer",
                                        )
                                    },
                                )?))
                            }
                            None => Ok(None),
                        }
                    };
                    let upper = idx(&ix.uidx, self)?;
                    let lower = idx(&ix.lidx, self)?;
                    if ix.is_slice {
                        subs.push((lower, upper, true));
                    } else {
                        subs.push((upper, None, false));
                    }
                }
                Some(NodeEnum::String(s)) => {
                    base = flush(self, base, &mut subs)?;
                    // An element of an array of anonymous records is just a record.
                    if matches!(&base, Expr::Subscript(..))
                        && matches!(base.ty(), DataType::Row(r) if r.oid == 0)
                    {
                        let at = ind.arg.as_deref().map_or(-1, super::ast::location);
                        return Err(with_pos(
                            Error::new(
                                code::UNDEFINED_COLUMN,
                                format!(
                                    "could not identify column \"{}\" in record data type",
                                    s.sval
                                ),
                            ),
                            at,
                        ));
                    }
                    let at = ind.arg.as_deref().map_or(-1, super::ast::location);
                    let first = std::ptr::eq(i, &ind.indirection[0]);
                    base = match self.field_select(base.clone(), &s.sval, at) {
                        Ok(b) => b,
                        // `(x).f` with no field f is the function f(x) (functional notation).
                        Err(e)
                            if e.code == code::UNDEFINED_COLUMN
                                && first
                                && matches!(base.ty(), DataType::Row(r) if r.oid != 0)
                                && self
                                    .cat
                                    .functions
                                    .values()
                                    .any(|f| f.name == s.sval && f.args.len() == 1) =>
                        {
                            let call = pb::FuncCall {
                                funcname: vec![pb::Node {
                                    node: Some(NodeEnum::String(s.clone())),
                                }],
                                args: ind.arg.as_deref().cloned().into_iter().collect(),
                                funcformat: pb::CoercionForm::CoerceExplicitCall as i32,
                                location: at,
                                ..Default::default()
                            };
                            match self.bind_func_call(&call) {
                                Ok(f) => f,
                                Err(_) => return Err(e),
                            }
                        }
                        Err(e) => return Err(e),
                    };
                }
                Some(NodeEnum::AStar(_)) => {
                    base = flush(self, base, &mut subs)?;
                }
                _ => return Err(crate::error::unsupported("this indirection")),
            }
        }
        flush(self, base, &mut subs)
    }

    // ── subqueries ──

    /// Bind a subquery in a new level: its plan, columns and the params it reads.
    pub(crate) fn bind_subquery(
        &mut self,
        n: &Node,
    ) -> Result<(Arc<Plan>, Vec<OutCol>, Vec<Expr>)> {
        let ((plan, cols), params) =
            crate::stack::guard(|| self.in_level(|b| b.bind_select_node(n)))?;
        Ok((Arc::new(plan), cols, params))
    }

    /// An ANY / ALL / ROWCOMPARE sublink's comparisons, one per column: the left operands
    /// coerced to the operators' inputs, the operators (under the collation the operands
    /// derive together), and the subquery's plan with its columns cast to their right
    /// inputs. `uncorrelated`: an `=` ANY the planner may hash.
    #[allow(clippy::too_many_arguments)]
    fn sublink_comparisons(
        &mut self,
        op: &str,
        left: Vec<Expr>,
        cols: &[OutCol],
        plan: Arc<Plan>,
        uncorrelated: bool,
        all: bool,
        loc: i32,
    ) -> Result<(Vec<Expr>, Vec<Arc<FuncDef>>, Arc<Plan>)> {
        let mut lefts = Vec::new();
        let mut ops = Vec::new();
        let mut casts = Vec::new();
        for (l, c) in left.into_iter().zip(cols) {
            let res = resolve_binop(op, &l.ty(), &c.ty).map_err(|e| with_pos(e, loc))?;
            lefts.push(self.coerce_implicit(l, &res.arg_types[0])?);
            casts.push(res.arg_types[1].clone());
            // The comparison runs under what the left operand and the subquery's
            // column derive together.
            let rc = self.out_col_collation(c, &c.ty);
            let def = match lefts.last().map(|l| self.collation_of(l)) {
                Some(Ok(lc)) => match lc.merge(rc) {
                    Ok(super::Coll::Implicit(n) | super::Coll::Explicit(n, _))
                        if !self.deparse =>
                    {
                        crate::types::collation::apply(&res.def, &n, &self.cat, None)
                            .unwrap_or(res.def)
                    }
                    // Conflicting implicit collations leave the comparison none: it
                    // fails when it runs, hashing the subquery's rows when the
                    // planner hashes it (uncorrelated `=`), else comparing.
                    Ok(super::Coll::Conflict(..)) if !self.deparse => {
                        let probe = [lefts.last().cloned().unwrap(), Expr::Col(0, c.ty.clone())];
                        match needs_collation(&res.def, &probe) {
                            Some(imp) => {
                                let hashed = !all && op == "=" && uncorrelated;
                                Arc::new(func::FuncDef {
                                    imp: if hashed { indeterminate::<7> } else { imp },
                                    srf: None,
                                    ..(*res.def).clone()
                                })
                            }
                            None => res.def,
                        }
                    }
                    _ => res.def,
                },
                _ => res.def,
            };
            // (Composites: their fields under theirs.)
            let def = self.record_compare_def(def, &[lefts.last().cloned().unwrap(), Expr::Col(0, c.ty.clone())]);
            ops.push(def);
        }
        // Cast subquery columns to the operator's right-hand types.
        let needs = cols.iter().zip(&casts).any(|(c, t)| {
            !c.ty.same_base(t) && !t.is_polymorphic() && !matches!(t, DataType::Any)
        });
        let plan = if needs {
            let exprs = cols
                .iter()
                .zip(&casts)
                .enumerate()
                .map(|(i, (c, t))| {
                    let col = Expr::Col(i, c.ty.clone());
                    if c.ty.same_base(t) || t.is_polymorphic() {
                        Ok(col)
                    } else {
                        self.coerce_implicit(col, t)
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            Arc::new(Plan::Project { input: plan, exprs })
        } else {
            plan
        };
        Ok((lefts, ops, plan))
    }

    /// `ROW(a, b, ...) op (SELECT x, y, ...)` (ROWCOMPARE_SUBLINK): the subquery's one row
    /// (NULL without one) compared with the row column by column. Uncorrelated, it is an
    /// InitPlan whose columns the comparison reads as values (`(InitPlan n).colk`, the
    /// comparisons separate quals); else a SubPlan testing the comparison per row.
    #[allow(clippy::too_many_arguments)]
    fn rowcompare_sublink(
        &mut self,
        id: usize,
        op: &str,
        l: &Node,
        loc: i32,
        plan: Arc<Plan>,
        cols: Vec<OutCol>,
        params: Vec<Expr>,
    ) -> Result<Expr> {
        let left = match self.bind_expr(l)? {
            Expr::Row(items, _) => items,
            e => vec![e],
        };
        if left.len() != cols.len() {
            let msg = if left.len() < cols.len() {
                "subquery has too many columns"
            } else {
                "subquery has too few columns"
            };
            return Err(with_pos(Error::new(code::SYNTAX_ERROR, msg), loc));
        }
        if params.is_empty() && !self.deparse {
            let one = cols.len() == 1;
            let ty = |c: &OutCol| {
                if matches!(c.ty, DataType::Unknown) {
                    DataType::Text
                } else {
                    c.ty.clone()
                }
            };
            let sq = Expr::Subquery(Box::new(Subquery {
                id,
                kind: SubqueryKind::Scalar,
                plan,
                params,
                left: vec![],
                ops: vec![],
                ty: if one { ty(&cols[0]) } else { DataType::Record },
                func: None,
                call: None,
                test_op: None,
                names: Default::default(),
                qual: self.binding_qual(),
                tlist: self.binding_tlist(),
            }));
            let rs = if one {
                vec![sq]
            } else {
                cols.iter()
                    .enumerate()
                    .map(|(i, c)| Expr::Field(Box::new(sq.clone()), i, ty(c)))
                    .collect()
            };
            return self.row_compare(op, left, rs, loc);
        }
        if !matches!(op, "=" | "<>" | "<" | "<=" | ">" | ">=") {
            return Err(crate::error::unsupported("this row comparison with a subquery"));
        }
        let (left, ops, plan) = self.sublink_comparisons(op, left, &cols, plan, false, false, loc)?;
        Ok(Expr::Subquery(Box::new(Subquery {
            id,
            kind: SubqueryKind::RowCompare,
            plan,
            params,
            left,
            ops,
            ty: DataType::Bool,
            func: None,
            call: None,
            test_op: None,
            names: Default::default(),
            qual: self.binding_qual(),
            tlist: self.binding_tlist(),
        })))
    }

    fn bind_sublink(&mut self, s: &pb::SubLink) -> Result<Expr> {
        self.bind_sublink_in(s, None)
    }

    /// `row`: `ROW(...) op (SELECT ...)`, a ROWCOMPARE_SUBLINK: the operator, the ROW() and
    /// the operator's location.
    fn bind_sublink_in(&mut self, s: &pb::SubLink, row: Option<(&str, &Node, i32)>) -> Result<Expr> {
        use pb::SubLinkType as T;
        let kind = T::try_from(s.sub_link_type).unwrap_or(T::ExprSublink);
        let loc = s.location;
        // transformSubLink: expression kinds where a subquery can't appear.
        let banned = match self.clause() {
            Clause::Check => Some("check constraint"),
            Clause::Default => Some("DEFAULT expression"),
            Clause::IndexExpr => Some("index expression"),
            Clause::IndexPredicate => Some("index predicate"),
            Clause::StatsExpr => Some("statistics expression"),
            Clause::PartitionKey => Some("partition key expression"),
            Clause::Generated => Some("column generation expression"),
            Clause::PartitionBound => Some("partition bound"),
            Clause::CopyWhere => Some("COPY FROM WHERE condition"),
            _ => None,
        };
        if let Some(what) = banned
            && !self.deparse
        {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("cannot use subquery in {what}"),
                ),
                loc,
            ));
        }
        // make_subplan: an EXISTS sub-select is planned for its first row (as a LIMIT 1).
        let saved_limit = self.arm_limit;
        if kind == T::ExistsSublink {
            self.arm_limit = Some((self.levels.len() + 1, 1.0, 0.0));
        }
        // An IN / ANY sub-select that is one plain SELECT: its level planned here, after
        // binding, with the plan as bound kept for pull_up_sublinks (`unplanned_subs`).
        let deferrable = kind == T::AnySublink
            && row.is_none()
            && !self.deparse
            && !self.unplanned
            && self.deps.is_none()
            && self.refs.is_none()
            && matches!(inner(s.subselect.as_deref().unwrap()), Some(NodeEnum::SelectStmt(q))
                if matches!(
                    pb::SetOperation::try_from(q.op).unwrap_or(pb::SetOperation::SetopNone),
                    pb::SetOperation::SetopNone | pb::SetOperation::Undefined
                ) && q.values_lists.is_empty()
                    && q.locking_clause.is_empty()
                    && q.with_clause.is_none());
        let saved_defer = (self.defer_level.take(), std::mem::take(&mut self.deferred));
        if deferrable {
            self.defer_level = Some(self.levels.len() + 1);
        }
        let rt_lo = self.fresh_id();
        let r = self.bind_subquery(s.subselect.as_deref().unwrap());
        let rt_up = (rt_lo, self.fresh_id(), self.last_rt_flat, kind == T::ExistsSublink);
        self.level().sublinks.push(rt_up);
        let deferred = std::mem::take(&mut self.deferred);
        (self.defer_level, self.deferred) = saved_defer;
        self.arm_limit = saved_limit;
        let (plan, cols, params) = r?;
        let (plan, unplanned) = if deferred {
            let unplanned = plan.clone();
            let mut p = Arc::unwrap_or_clone(plan);
            let _pushed = crate::scoped::set(&crate::sql::paths::PUSHED_LEVEL, false, "rowmarks::PUSHED_LEVEL");
            let _own = crate::scoped::set(&crate::sql::paths::OWN_LOCK, false, "rowmarks::OWN_LOCK");
            self.hidden_rtes += crate::sql::paths::plan_query_limited(self.ctx, &mut p, None);
            drop(_own);
            drop(_pushed);
            if let Err(e) = super::check_merge_keys(self.ctx, &mut p) {
                self.plan_error(e)?;
            }
            (Arc::new(p), Some(unplanned))
        } else {
            (plan, None)
        };
        let id = self.fresh_id();
        if self.deparse {
            self.sublink_nodes
                .insert(id, (**s.subselect.as_ref().unwrap()).clone());
        }
        // simplify_EXISTS_query: its select list doesn't matter (constants only: dropped;
        // so are constants that failed to fold, never simplified).
        let folds_failed = self.folds_failed();
        let plan = match &*plan {
            Plan::Project { input, exprs }
                if kind == T::ExistsSublink
                    && !self.deparse
                    && exprs.iter().all(|e| {
                        matches!(e, Expr::Const(..))
                            || (folds_failed
                                && super::fold::exists_simplified(input)
                                && e.columns().is_empty()
                                && super::fold::fails(e, self.ctx, self.fold_err.as_deref().unwrap_or(&[])))
                    }) =>
            {
                Arc::new(Plan::Project { input: input.clone(), exprs: Vec::new() })
            }
            _ => plan,
        };
        if let Some((op, l, oploc)) = row {
            return self.rowcompare_sublink(id, op, l, oploc, plan, cols, params);
        }
        match kind {
            T::ExistsSublink => Ok(Expr::Subquery(Box::new(Subquery {
                id,
                kind: SubqueryKind::Exists,
                plan,
                params,
                left: vec![],
                ops: vec![],
                ty: DataType::Bool,
                func: None,

                call: None,
                test_op: None,
                names: Default::default(),
                qual: self.binding_qual(),
                tlist: self.binding_tlist(),
            }))),
            T::ExprSublink => {
                if cols.len() != 1 {
                    return Err(with_pos(
                        Error::new(code::SYNTAX_ERROR, "subquery must return only one column"),
                        loc,
                    ));
                }
                let ty = if matches!(cols[0].ty, DataType::Unknown) {
                    DataType::Text
                } else {
                    cols[0].ty.clone()
                };
                // A table column's collation comes through.
                match self.out_col_collation(&cols[0], &ty) {
                    super::Coll::Implicit(c) if ty.default_collation() == Some(c.as_str()) => {}
                    c => {
                        self.sub_colls.insert(id, c.implicit());
                    }
                }
                Ok(Expr::Subquery(Box::new(Subquery {
                    id,
                    kind: SubqueryKind::Scalar,
                    plan,
                    params,
                    left: vec![],
                    ops: vec![],
                    ty,
                    func: None,

                    call: None,
                    test_op: None,
                    names: Default::default(),
                    qual: self.binding_qual(),
                    tlist: self.binding_tlist(),
                })))
            }
            T::ArraySublink => {
                if cols.len() != 1 {
                    return Err(with_pos(
                        Error::new(code::SYNTAX_ERROR, "subquery must return only one column"),
                        loc,
                    ));
                }
                let ct = if matches!(cols[0].ty, DataType::Unknown) {
                    DataType::Text
                } else {
                    cols[0].ty.clone()
                };
                let ty = if ct.is_array() {
                    ct
                } else {
                    DataType::array_of(ct)
                };
                Ok(Expr::Subquery(Box::new(Subquery {
                    id,
                    kind: SubqueryKind::Array,
                    plan,
                    params,
                    left: vec![],
                    ops: vec![],
                    ty,
                    func: None,

                    call: None,
                    test_op: None,
                    names: Default::default(),
                    qual: self.binding_qual(),
                    tlist: self.binding_tlist(),
                })))
            }
            T::AnySublink | T::AllSublink => {
                let all = kind == T::AllSublink;
                let op = names(&s.oper_name)
                    .last()
                    .cloned()
                    .unwrap_or_else(|| "=".to_string());
                let test = s.testexpr.as_deref().unwrap();
                let left: Vec<Expr> = match inner(test) {
                    Some(NodeEnum::RowExpr(r)) => r
                        .args
                        .iter()
                        .map(|a| self.bind_expr(a))
                        .collect::<Result<_>>()?,
                    _ => vec![self.bind_expr(test)?],
                };
                if left.len() != cols.len() {
                    let msg = if left.len() < cols.len() {
                        "subquery has too many columns"
                    } else {
                        "subquery has too few columns"
                    };
                    return Err(with_pos(Error::new(code::SYNTAX_ERROR, msg), loc));
                }
                // A user-defined operator: each row's comparison runs inside the subquery
                // (`x op c` over its column, `x` passed in), and the sublink tests those
                // results against true.
                if left.len() == 1 && !self.deparse && !self.cat.operators.is_empty() {
                    let (l, cty) = (left[0].clone(), cols[0].ty.clone());
                    let seeded = params.clone();
                    let own_params = params.len();
                    let (test, params2) = self.in_level(|b| {
                        b.level().params = seeded;
                        let depth = b.levels.len() - 2;
                        let lo = b.lift(depth, l);
                        b.user_operator(None, &op, Some(lo), Expr::Col(0, cty), loc)
                    })?;
                    if let Some(test) = test {
                        let eq = resolve_binop("=", &DataType::Bool, &DataType::Bool)
                            .map_err(|e| with_pos(e, loc))?;
                        return Ok(Expr::Subquery(Box::new(Subquery {
                            id,
                            kind: if all {
                                SubqueryKind::All
                            } else {
                                SubqueryKind::Any
                            },
                            plan: Arc::new(Plan::Project {
                                input: plan,
                                exprs: vec![test],
                            }),
                            params: params2,
                            left: vec![Expr::Const(Value::Bool(true), DataType::Bool)],
                            ops: vec![eq.def],
                            ty: DataType::Bool,
                            func: None,

                            call: None,
                            test_op: Some(own_params),
                            names: Default::default(),
                            qual: self.binding_qual(),
                            tlist: self.binding_tlist(),
                        })));
                    }
                }
                let planned = plan.clone();
                let (lefts, ops, plan) =
                    self.sublink_comparisons(&op, left, &cols, plan, params.is_empty(), all, loc)?;
                // (The same casts over the plan as bound.)
                if let Some(u) = unplanned
                    && !all
                {
                    let u = match &*plan {
                        _ if Arc::ptr_eq(&plan, &planned) => Some(u),
                        Plan::Project { input, exprs } if Arc::ptr_eq(input, &planned) => {
                            Some(Arc::new(Plan::Project { input: u, exprs: exprs.clone() }))
                        }
                        _ => None,
                    };
                    if let Some(u) = u {
                        self.unplanned_subs.insert(id, u);
                    }
                }
                Ok(Expr::Subquery(Box::new(Subquery {
                    id,
                    kind: if all {
                        SubqueryKind::All
                    } else {
                        SubqueryKind::Any
                    },
                    plan,
                    params,
                    left: lefts,
                    ops,
                    ty: DataType::Bool,
                    func: None,

                    call: None,
                    test_op: None,
                    names: Arc::new(cols.iter().map(|c| c.name.clone()).collect()),
                    qual: self.binding_qual(),
                    tlist: self.binding_tlist(),
                })))
            }
            _ => Err(crate::error::unsupported("this kind of subquery")),
        }
    }

    // ── functions ──

    /// pg_column_size / pg_column_compression of a table's column: the value as stored
    /// depends on the whole row (TOAST compresses and moves the biggest values first), so
    /// the call also gets the column, the table and the row.
    fn stored_column_call(&mut self, e: Expr) -> Expr {
        let Expr::Call(def, args, ty) = &e else { return e };
        if self.deparse
            || args.len() != 1
            || !matches!(def.name, "pg_column_size" | "pg_column_compression")
            || !self.level().st.group_exprs.is_empty()
        {
            return e;
        }
        let Expr::Col(i, _) = &args[0] else { return e };
        let found = self.level().scope.rels.iter().find_map(|r| {
            let t = r.table.as_ref()?;
            let c = r.cols.iter().find(|c| matches!(&c.expr, Expr::Col(j, _) if j == i))?;
            if c.table != t.oid || c.attnum < 1 {
                return None;
            }
            let row: Vec<Expr> = t
                .columns
                .iter()
                .enumerate()
                .map(|(k, col)| {
                    r.cols
                        .iter()
                        .find(|x| x.table == t.oid && x.attnum == k as i16 + 1)
                        .map(|x| x.expr.clone())
                        .unwrap_or_else(|| Expr::null(col.ty.clone()))
                })
                .collect();
            Some((t.oid, c.attnum - 1, row))
        });
        let Some((oid, col, row)) = found else { return e };
        Expr::Call(
            def.clone(),
            vec![
                args[0].clone(),
                Expr::Const(Value::Int4(col as i32), DataType::Int4),
                Expr::Const(Value::Oid(oid), DataType::Oid),
                Expr::Row(row, DataType::Record),
            ],
            ty.clone(),
        )
    }

    fn bind_func_call(&mut self, f: &pb::FuncCall) -> Result<Expr> {
        // (A function whose body calls it again recurses here, inlined.)
        let r = crate::stack::guard(|| self.bind_func_call_inner(f));
        // pg_typeof reports the type its argument is declared as, a domain too.
        let r = match r {
            Ok(Expr::Call(def, mut args, ty)) if def.oid == 1619 && args.len() == 1 => {
                let a = args.pop().unwrap();
                args.push(self.tag_declared_domain(a)?);
                Ok(Expr::Call(def, args, ty))
            }
            r => r,
        };
        // ParseFuncOrColumn: `col(x)` on a composite x with no such function is x.col.
        if let Err(e) = &r
            && e.code == code::UNDEFINED_FUNCTION
            && f.funcname.len() == 1
            && f.args.len() == 1
            && !f.agg_star
            && !f.agg_distinct
            && !f.func_variadic
            && f.agg_order.is_empty()
            && f.agg_filter.is_none()
            && f.over.is_none()
            && !f.agg_within_group
            && !matches!(inner(&f.args[0]), Some(NodeEnum::NamedArgExpr(_)))
        {
            let col = names(&f.funcname).pop().unwrap_or_default();
            if let Ok(arg) = self.bind_expr(&f.args[0])
                && let DataType::Row(rt) = arg.ty()
                && rt.fields.iter().any(|(n, _)| *n == col)
            {
                return self.field_select(arg, &col, f.location);
            }
        }
        r
    }

    fn bind_func_call_inner(&mut self, f: &pb::FuncCall) -> Result<Expr> {
        let fname = names(&f.funcname);
        let (schema, name) = match fname.as_slice() {
            [n] => (None, n.as_str()),
            [s, n] => (Some(s.as_str()), n.as_str()),
            _ => {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "improper qualified name (too many dotted names): {}",
                            fname.join(".")
                        ),
                    ),
                    f.location,
                ));
            }
        };
        if let Some(s) = schema
            && s != "pg_catalog"
            && s != "information_schema"
            && self.cat.schema_by_name(s).is_none()
        {
            return Err(with_pos(
                Error::new(
                    code::UNDEFINED_SCHEMA,
                    format!("schema \"{s}\" does not exist"),
                ),
                f.location,
            ));
        }
        if f.over.is_some() {
            return self.bind_window_call(name, f);
        }
        // rank() and friends without arguments are the window functions, not the hypothetical-set
        // aggregates.
        let window_only = f.args.is_empty()
            && !f.agg_within_group
            && agg_registry().by_name.get(name).is_some_and(|v| {
                v.iter()
                    .all(|d| d.kind == crate::exec::agg::AggKind::Hypothetical)
            });
        if !window_only
            && (agg_registry().by_name.contains_key(name)
                && schema.is_none_or(|s| s == "pg_catalog")
                || !self.user_aggs(schema, name).is_empty())
        {
            return self.bind_agg_call(schema, name, f);
        }
        if schema.is_none_or(|s| s == "pg_catalog")
            && !f.agg_star
            && let Some(cands) = window_sigs().funcs.get(name)
        {
            let types = f
                .args
                .iter()
                .map(|a| Ok(self.bind_expr(a)?.ty()))
                .collect::<Result<Vec<_>>>()?;
            if let Ok(Some(_)) = select_candidate(cands, &types, false) {
                return Err(with_pos(
                    Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("window function {name} requires an OVER clause"),
                    ),
                    f.location,
                ));
            }
        }
        // func_get_detail runs first: a call that matches no function at all reports that.
        if f.agg_star
            || f.agg_distinct
            || !f.agg_order.is_empty()
            || f.agg_filter.is_some()
            || f.agg_within_group
        {
            let mut plain = f.clone();
            plain.agg_star = false;
            plain.agg_distinct = false;
            plain.agg_filter = None;
            if plain.agg_within_group {
                for s in std::mem::take(&mut plain.agg_order) {
                    if let Some(NodeEnum::SortBy(sb)) = inner(&s)
                        && let Some(n) = sb.node.as_deref()
                    {
                        plain.args.push(n.clone());
                    }
                }
            }
            plain.agg_order.clear();
            plain.agg_within_group = false;
            if let Err(mut e) = self.bind_func_call_inner(&plain) {
                if e.code == code::UNDEFINED_FUNCTION
                    && f.agg_order.len() > 1
                    && !f.agg_within_group
                {
                    e.hint = Some(
                        "No aggregate function matches the given name and argument types. \
                         Perhaps you misplaced ORDER BY; ORDER BY must appear after all regular \
                         arguments of the aggregate."
                            .into(),
                    );
                }
                return Err(e);
            }
        }
        if f.agg_star {
            return Err(with_pos(
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("{name}(*) specified, but {name} is not an aggregate function"),
                ),
                f.location,
            ));
        }
        if f.agg_distinct || !f.agg_order.is_empty() || f.agg_filter.is_some() || f.agg_within_group
        {
            let what = if f.agg_distinct {
                "DISTINCT"
            } else if f.agg_filter.is_some() {
                "FILTER"
            } else if f.agg_within_group {
                "WITHIN GROUP"
            } else {
                "ORDER BY"
            };
            return Err(with_pos(
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("{what} specified, but {name} is not an aggregate function"),
                ),
                f.location,
            ));
        }
        if let Some(e) = self.func_style_coercion(name, f)? {
            return Ok(e);
        }
        // User-defined SQL functions.
        if let Some(e) = self.try_user_function(schema, name, f)? {
            return Ok(e);
        }
        let (def, args, ret) = match self.range_constructors(schema, name) {
            Some(cands) => {
                let (def, mut args, ret) = self.resolve_call_among(
                    Some(&cands),
                    name,
                    &f.args,
                    f.func_variadic,
                    f.location,
                )?;
                if schema.is_none() {
                    self.note_func_ref(f.location, def.oid);
                }
                // A range over a domain: its constructor's bounds are of the domain (shown as
                // casts to it, `(a)::d`, where they aren't of it already).
                if let DataType::Range(rt) = &ret
                    && let Some(d) = rt.subtype_domain
                    && let Some(dc) = self.domain_coerce(d)?
                {
                    for a in args.iter_mut().take(2) {
                        // (One of the domain already, maybe relabeled to its base type by
                        // the call's coercion: that value.)
                        let mut x = &*a;
                        while self.domain_of(x) != Some(d)
                            && let Expr::Relabel(y, _) | Expr::Cast(y, _, _, false) = x
                        {
                            x = y;
                        }
                        if self.domain_of(x) == Some(d) {
                            *a = x.clone();
                            continue;
                        }
                        let e = std::mem::replace(a, Expr::null(DataType::Unknown));
                        *a = Expr::CoerceToDomain(Box::new(e), dc.clone());
                    }
                }
                (def, args, ret)
            }
            None => {
                // Built-in functions live in pg_catalog, information_schema's in it.
                let is_func = func::monitor::INFORMATION_SCHEMA_FUNCS.contains(&name);
                let visible = match schema {
                    None => {
                        !is_func
                            || self
                                .ctx
                                .search_path
                                .iter()
                                .any(|s| s == "information_schema")
                    }
                    Some("pg_catalog") => !is_func,
                    Some("information_schema") => is_func,
                    Some(_) => false,
                };
                if !visible {
                    let mut types = Vec::new();
                    for a in &f.args {
                        let a = match inner(a) {
                            Some(NodeEnum::NamedArgExpr(na)) => na.arg.as_deref().unwrap(),
                            _ => a,
                        };
                        types.push(self.bind_expr(a)?.ty());
                    }
                    let shown = schema.map_or_else(|| name.to_string(), |s| format!("{s}.{name}"));
                    return Err(with_pos(func::no_function(&shown, &types), f.location));
                }
                // The error names the function as written.
                self.resolve_call(name, &f.args, f.func_variadic, f.location)
                    .map_err(|e| match schema {
                        Some(s) if e.message.starts_with(&format!("function {name}(")) => Error {
                            message: e.message.replacen(name, &format!("{s}.{name}"), 1),
                            ..e
                        },
                        _ => e,
                    })?
            }
        };
        self.note_builtin_call(def.oid);
        if def.is_srf() {
            self.check_srf_placement(f.location)?;
            let ncols = if def.out_cols.len() > 1 {
                def.out_cols.len()
            } else if matches!(
                def.name,
                "json_populate_record"
                    | "jsonb_populate_record"
                    | "json_populate_recordset"
                    | "jsonb_populate_recordset"
            ) && let DataType::Row(r) = &ret
            {
                // A composite result: one record value per row (the row's fields).
                r.fields.len().max(2)
            } else {
                1
            };
            // A record of the OUT columns, so `(f(x)).*` and `(f(x)).col` work.
            let ret = if ncols > 1 && ret == DataType::Record {
                // Polymorphic OUT columns take the type of the arguments'.
                let elem = args.first().and_then(|a| a.ty().elem().cloned());
                let col = |t: &DataType| match (t, &elem) {
                    (DataType::AnyElement, Some(e)) => e.clone(),
                    _ => t.clone(),
                };
                DataType::row(
                    def.out_cols
                        .iter()
                        .map(|(n, t)| (n.to_string(), col(t)))
                        .collect(),
                )
            } else {
                ret
            };
            let def = self.input_collation_def(def, &args);
            let i = self.push_srf(SrfCall {
                func: def,
                args,
                ty: ret.clone(),
                ncols,
                body: None,
            });
            return Ok(Expr::Srf(i, ret));
        }
        let mut args = args;
        // pg_collation_for reads the collation its argument derives, which the call carries.
        if def.name == "pg_collation_for"
            && !self.deparse
            && let [a] = args.as_mut_slice()
            && !matches!(a, Expr::Collate(..))
            && let super::Coll::Implicit(c) | super::Coll::Explicit(c, _) = self.collation_of(a)?
        {
            *a = Expr::Collate(Box::new(a.clone()), c, -1);
        }
        // xpath(text, xml) and xpath_exists(text, xml) are SQL functions calling the
        // three-argument forms with no namespaces: inlined, or run (in their frame) when the
        // arguments are constants.
        if !self.deparse
            && let Some(full) = match def.oid {
                2932 => func::def_by_oid(2931),
                3050 => func::def_by_oid(3049),
                _ => None,
            }
        {
            let consts = args.iter().all(|a| matches!(a, Expr::Const(..)));
            let no_ns = DataType::array_of(DataType::Text);
            args.push(Expr::Const(
                Value::Array(Arc::new(crate::types::array::ArrayValue::empty(
                    DataType::Text,
                ))),
                no_ns,
            ));
            let call = Expr::Call(full, args, ret.clone());
            if !consts {
                return Ok(call);
            }
            let frame = crate::error::Frame::SqlFn {
                name: Arc::from(def.name),
                src: Arc::from(
                    format!(
                        "select pg_catalog.{}($1, $2, '{{}}'::pg_catalog.text[])",
                        def.name
                    )
                    .as_str(),
                ),
                at: SqlFnAt::Statement(1),
            };
            let ctx = self.ctx;
            let v = ctx.sess.framed(frame, |_| eval(&call, &[], ctx))?;
            return Ok(Expr::Const(v, ret));
        }
        let e = self.fold(Expr::Call(def, args, ret))?;
        Ok(match e {
            Expr::Call(..) if f.funcformat == pb::CoercionForm::CoerceSqlSyntax as i32 => {
                Expr::SqlSyntax(Box::new(e))
            }
            e => e,
        })
    }

    /// `typename(arg)` read as a cast (func_get_detail's FUNCDETAIL_COERCION): one plain
    /// argument, no function taking exactly its type, and a literal, or a binary-coercible or
    /// I/O conversion to the type.
    fn func_style_coercion(&mut self, name: &str, f: &pb::FuncCall) -> Result<Option<Expr>> {
        let [arg] = f.args.as_slice() else {
            return Ok(None);
        };
        if f.func_variadic || matches!(inner(arg), Some(NodeEnum::NamedArgExpr(_))) {
            return Ok(None);
        }
        let tn = pb::TypeName {
            names: f.funcname.clone(),
            typemod: -1,
            location: f.location,
            ..Default::default()
        };
        // (FuncNameAsType looks the name up as functions are: not in the temporary schema.)
        let path: Vec<String> = self
            .ctx
            .search_path
            .iter()
            .filter(|s| f.funcname.len() > 1 || !s.starts_with("pg_temp"))
            .cloned()
            .collect();
        let Ok((target, domain)) = super::resolve_type_name(&self.cat, &path, &tn) else {
            return Ok(None);
        };
        // Not a table's row type.
        if matches!(target, DataType::Row(_) | DataType::Record)
            || target.is_polymorphic()
            || matches!(target, DataType::Any | DataType::Unknown)
        {
            return Ok(None);
        }
        let e = self.bind_expr(arg)?;
        let from = e.ty();
        let exact = |t: &DataType| t.oid() == from.oid() && (t.oid() != 0 || *t == from);
        if func::registry()
            .funcs
            .get(name)
            .is_some_and(|c| c.iter().any(|d| d.args.len() == 1 && exact(&d.args[0])))
            || self
                .cat
                .functions
                .values()
                .any(|u| u.name == name && u.args.len() == 1 && exact(&u.args[0].1))
        {
            return Ok(None);
        }
        let literal = matches!(e, Expr::Const(_, DataType::Unknown));
        let coercion = literal
            || from.same_base(&target)
            || match crate::catalog::system::builtin_cast_method(from.oid(), target.oid()) {
                Some((m, _)) if m == 'b' || m == 'i' => true,
                // A cast function by this name the engine runs as the cast.
                Some((_, func)) => crate::catalog::system::builtin_proc_sigs()
                    .iter()
                    .any(|p| p.oid == func && p.name == name),
                None => {
                    from.elem().is_none()
                        && target.elem().is_none()
                        && (target.category() == crate::types::Category::String
                            && !matches!(from, DataType::Row(_) | DataType::Record)
                            || from.category() == crate::types::Category::String)
                }
            };
        if !coercion {
            return Ok(None);
        }
        let shown = target.display();
        let out = self.coerce_with(e, &target, CastContext::Explicit, &|from| {
            Error::new(
                code::CANNOT_COERCE,
                format!("cannot cast type {} to {shown}", from.display()),
            )
        })?;
        Ok(Some(match domain {
            Some(d) => self.domain_check(out, d)?,
            None => out,
        }))
    }

    /// Resolve a function call by name and bound arguments (named arguments allowed).
    pub(crate) fn resolve_call(
        &mut self,
        name: &str,
        arg_nodes: &[Node],
        variadic: bool,
        loc: i32,
    ) -> Result<(Arc<FuncDef>, Vec<Expr>, DataType)> {
        self.resolve_call_among(None, name, arg_nodes, variadic, loc)
    }

    /// The constructor functions named `name` of a user-defined range or multirange type.
    fn range_constructors(&self, schema: Option<&str>, name: &str) -> Option<Vec<Arc<FuncDef>>> {
        if self.cat.ranges.is_empty() {
            return None;
        }
        let schemas = match schema {
            Some(s) => vec![self.cat.schema_by_name(s)?.oid],
            None => self.cat.routine_path(&self.ctx.search_path),
        };
        let ctors = |r: &crate::catalog::RangeDef| {
            if r.ctors.0.is_empty() {
                (r.name.clone(), r.multi_name.clone())
            } else {
                r.ctors.clone()
            }
        };
        let (r, names) = schemas.iter().find_map(|s| {
            self.cat
                .ranges
                .values()
                .filter(|r| r.schema == *s)
                .map(|r| (r, ctors(r)))
                .find(|(_, (c, m))| c == name || m == name)
        })?;
        Some(func::range::user_constructors(
            &r.range_type(),
            (&names.0, &names.1),
            &r.fn_oids,
            name,
        ))
    }

    /// `resolve_call` among `cands` instead of the built-ins.
    fn resolve_call_among(
        &mut self,
        cands: Option<&[Arc<FuncDef>]>,
        name: &str,
        arg_nodes: &[Node],
        variadic: bool,
        loc: i32,
    ) -> Result<(Arc<FuncDef>, Vec<Expr>, DataType)> {
        let mut args = Vec::with_capacity(arg_nodes.len());
        let mut arg_names: Vec<Option<String>> = Vec::new();
        for a in arg_nodes {
            match inner(a) {
                Some(NodeEnum::NamedArgExpr(na)) => {
                    args.push(self.bind_expr(na.arg.as_deref().unwrap())?);
                    arg_names.push(Some(na.name.clone()));
                }
                _ => {
                    args.push(self.bind_expr(a)?);
                    arg_names.push(None);
                }
            }
        }
        check_arg_names(arg_nodes)?;
        drop_void_params(&mut args, &mut arg_names);
        if arg_names.iter().any(Option::is_some) && !variadic {
            return self.resolve_named(name, args, arg_names, loc);
        }
        let types: Vec<DataType> = args.iter().map(|a| a.ty()).collect();
        let res = if variadic {
            if let Some(last) = types.last()
                && last.elem().is_none()
                && !matches!(last, DataType::Unknown)
                && func::takes_variadic_any(name)
            {
                let at = arg_nodes.last().map_or(loc, super::ast::location);
                return Err(with_pos(
                    Error::new(
                        code::DATATYPE_MISMATCH,
                        "VARIADIC argument must be an array",
                    ),
                    at,
                ));
            }
            match cands {
                Some(c) => func::resolve_among(c, name, &types, true),
                None => func::resolve_spread_call(name, &types),
            }
        } else {
            match cands {
                Some(c) => func::resolve_among(c, name, &types, false),
                None => resolve_function(name, &types),
            }
        }
        .map_err(|e| with_pos(e, loc))?;
        // enforce_generic_type_consistency: anyelement-family parameters whose inputs are
        // all untyped literals have nothing to resolve from.
        let family1 = |t: &DataType| {
            matches!(
                t,
                DataType::AnyElement
                    | DataType::AnyNonArray
                    | DataType::AnyEnum
                    | DataType::AnyArray
                    | DataType::AnyRange
                    | DataType::AnyMultirange
            )
        };
        let poly: Vec<&DataType> = res
            .def
            .args
            .iter()
            .zip(&types)
            .filter(|(p, _)| family1(p))
            .map(|(_, i)| i)
            .collect();
        if !poly.is_empty() && poly.iter().all(|i| matches!(i, DataType::Unknown)) {
            return Err(Error::new(
                code::DATATYPE_MISMATCH,
                "could not determine polymorphic type because input has type unknown",
            ));
        }
        let def = res.def.clone();
        let ret = res.ret.clone();
        let mut out = Vec::with_capacity(args.len());
        for (a, t) in args.into_iter().zip(res.arg_types.iter()) {
            out.push(self.coerce_implicit(a, t)?);
        }
        Ok((def, out, ret))
    }

    fn resolve_named(
        &mut self,
        name: &str,
        args: Vec<Expr>,
        arg_names: Vec<Option<String>>,
        loc: i32,
    ) -> Result<(Arc<FuncDef>, Vec<Expr>, DataType)> {
        let reg = func::registry();
        let cands = reg.funcs.get(name).cloned().unwrap_or_default();
        for def in cands {
            if def.arg_names.is_empty() {
                continue;
            }
            // Place each argument.
            let mut slots: Vec<Option<Expr>> = vec![None; def.args.len()];
            let mut ok = true;
            for (i, (a, n)) in args.iter().zip(&arg_names).enumerate() {
                let pos = match n {
                    Some(n) => def.arg_names.iter().position(|x| x == n),
                    None => Some(i),
                };
                match pos {
                    Some(p) if p < slots.len() && slots[p].is_none() => slots[p] = Some(a.clone()),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            // Only parameters past min_args have defaults.
            if !ok
                || slots[..def.min_args.min(slots.len())]
                    .iter()
                    .any(Option::is_none)
            {
                continue;
            }
            // Fill omitted arguments with the declared defaults, or zero (make_interval & co.).
            let first_default = def.args.len().saturating_sub(def.defaults.len());
            let filled: Option<Vec<Expr>> = slots
                .into_iter()
                .zip(&def.args)
                .enumerate()
                .map(|(i, (s, t))| {
                    s.or_else(|| match t {
                        _ if i >= first_default => Some(Expr::Const(
                            def.defaults[i - first_default].clone(),
                            t.clone(),
                        )),
                        DataType::Int4 => Some(Expr::Const(Value::Int4(0), DataType::Int4)),
                        DataType::Int8 => Some(Expr::Const(Value::Int8(0), DataType::Int8)),
                        DataType::Float8 => Some(Expr::Const(Value::Float8(0.0), DataType::Float8)),
                        DataType::Numeric(_) => Some(Expr::Const(
                            Value::numeric(Numeric::zero()),
                            DataType::NUMERIC,
                        )),
                        DataType::Text => Some(Expr::Const(Value::Null, DataType::Text)),
                        _ => None,
                    })
                })
                .collect();
            let Some(filled) = filled else { continue };
            let types: Vec<DataType> = filled.iter().map(|a| a.ty()).collect();
            if let Ok(Some(res)) = select_candidate(std::slice::from_ref(&def), &types, false) {
                let ret = res.ret.clone();
                let mut out = Vec::new();
                for (a, t) in filled.into_iter().zip(res.arg_types.iter()) {
                    out.push(self.coerce_implicit(a, t)?);
                }
                return Ok((res.def, out, ret));
            }
        }
        let shown: Vec<String> = args
            .iter()
            .zip(&arg_names)
            .map(|(a, n)| match n {
                Some(n) => format!("{n} => {}", func::type_list(&[a.ty()])),
                None => func::type_list(&[a.ty()]),
            })
            .collect();
        let e = func::no_function(name, &[]);
        Err(with_pos(
            Error {
                message: format!("function {name}({}) does not exist", shown.join(", ")),
                ..e
            },
            loc,
        ))
    }

    // ── aggregates ──

    /// User-defined aggregates (CREATE AGGREGATE) named `name` in `schema` or on the path.
    pub(crate) fn user_aggs(
        &self,
        schema: Option<&str>,
        name: &str,
    ) -> Vec<Arc<crate::catalog::Function>> {
        let schemas: Vec<u32> = match schema {
            Some(s) => self
                .cat
                .schema_by_name(s)
                .map(|s| vec![s.oid])
                .unwrap_or_default(),
            None => self.cat.routine_path(&self.ctx.search_path),
        };
        self.cat
            .functions
            .values()
            .filter(|f| f.agg.is_some() && f.name == name && schemas.contains(&f.schema))
            .cloned()
            .collect()
    }

    fn bind_agg_call(
        &mut self,
        schema: Option<&str>,
        name: &str,
        f: &pb::FuncCall,
    ) -> Result<Expr> {
        let clause = self.clause();
        let loc = f.location;
        // In an aggregate's FILTER, one of the same level is "not allowed in FILTER".
        let in_filter = clause == Clause::Filter;
        if self.levels.last().unwrap().st.in_agg && !in_filter {
            return Err(with_pos(
                Error::new(
                    code::GROUPING_ERROR,
                    "aggregate function calls cannot be nested",
                ),
                loc,
            ));
        }
        // The outer references the arguments make (dropped if the aggregate moves out).
        let params_before: Vec<usize> = self.levels.iter().map(|l| l.params.len()).collect();
        // Which visible CTEs the arguments read (their `used` marks, set aside meanwhile).
        let used_before: Vec<Vec<bool>> = self
            .ctes
            .iter()
            .map(|s| s.iter().map(|c| c.used.replace(false)).collect())
            .collect();
        let was_in_agg = std::mem::replace(&mut self.level().st.in_agg, true);
        let refs_before = self.agg_refs.replace(Vec::new());
        let r = self.agg_call(schema, name, f);
        let written = std::mem::replace(&mut self.agg_refs, refs_before).unwrap_or_default();
        if let Some(r) = &mut self.agg_refs {
            r.extend(written.iter().filter(|l| !r.contains(l)).copied().collect::<Vec<_>>());
        }
        self.level().st.in_agg = was_in_agg;
        let mut args_ctes: Vec<(usize, String)> = Vec::new();
        // (`cte_levels` lines up with `ctes` from the top: the stack is set aside and put
        // back around function bodies and views.)
        let base = self.cte_levels.len().saturating_sub(self.ctes.len());
        for (i, (scope, before)) in self.ctes.iter().zip(&used_before).enumerate() {
            for (c, b) in scope.iter().zip(before) {
                if c.used.get() {
                    let level = self.cte_levels.get(base + i).copied().unwrap_or(0);
                    args_ctes.push((level, c.name.clone()));
                }
                c.used.set(c.used.get() || *b);
            }
        }
        let call = r?;
        // The aggregate belongs to the lowest query level its arguments read columns of
        // (an outer one's, inside a sublink): that level's clause decides if it's allowed.
        let cur = self.levels.len() - 1;
        // (Columns its arguments name that folding dropped count too: not an ordered-set
        // aggregate's, whose direct arguments were bound alike.)
        let named = written.iter().copied().filter(|&l| l <= cur && call.direct.is_empty()).max();
        let target = match (self.agg_level_read(&call), named) {
            (Some(a), Some(b)) => a.max(b),
            (a, b) => a.or(b).unwrap_or(cur),
        };
        let clause = if target == cur {
            clause
        } else {
            self.levels[target].clause
        };
        if !clause.aggs_ok() {
            return Err(with_pos(not_allowed("aggregate", clause), loc));
        }
        // (check_agg_arguments: a CTE defined below the aggregate's level can't be read.)
        if let Some((_, cte)) = args_ctes.iter().find(|(l, _)| *l > target) {
            return Err(with_pos(
                Error::new(
                    code::GROUPING_ERROR,
                    "outer-level aggregate cannot use a nested CTE",
                )
                .detail(format!(
                    "CTE \"{cte}\" is below the aggregate's semantic level."
                )),
                loc,
            ));
        }
        if target != cur {
            for (i, d) in call.direct.iter().enumerate() {
                if self.levels_up(d, cur).is_some_and(|u| u < cur - target) {
                    return Err(with_pos(
                        Error::new(
                            code::GROUPING_ERROR,
                            "outer-level aggregate cannot contain a lower-level variable in its direct arguments",
                        ),
                        f.args.get(i).map_or(loc, ast::location),
                    ));
                }
            }
        }
        if in_filter && was_in_agg {
            return Err(with_pos(
                Error::new(
                    code::GROUPING_ERROR,
                    "aggregate function calls cannot be nested",
                ),
                loc,
            ));
        }
        let ty = call.ty.clone();
        if target == cur {
            let st = &mut self.level().st;
            st.agg_loc.get_or_insert(loc);
            // (An identical call without volatile functions is the same aggregate:
            // find_compatible_agg.)
            let volatile = call.args.iter().chain(&call.direct).chain(&call.filter).any(|e| e.is_volatile())
                || call.order_by.iter().any(|k| k.expr.is_volatile());
            if !volatile {
                let key = format!("{call:?}");
                if let Some(j) = st.aggs.iter().position(|a| format!("{a:?}") == key) {
                    return Ok(Expr::Agg(j, ty));
                }
            }
            st.aggs.push(call);
            return Ok(Expr::Agg(st.aggs.len() - 1, ty));
        }
        if self.levels[target].st.in_agg {
            return Err(with_pos(
                Error::new(
                    code::GROUPING_ERROR,
                    "aggregate function calls cannot be nested",
                ),
                loc,
            ));
        }
        let mut call = call;
        let up = |b: &Self, e: Expr| b.unlift(e, cur, target);
        call.args = call.args.into_iter().map(|e| up(self, e)).collect();
        call.direct = call.direct.into_iter().map(|e| up(self, e)).collect();
        call.filter = call.filter.map(|e| up(self, e));
        for k in &mut call.order_by {
            k.expr = up(self, k.expr.clone());
        }
        for (l, n) in params_before.into_iter().enumerate().skip(target + 1) {
            self.levels[l].params.truncate(n);
        }
        let st = &mut self.levels[target].st;
        st.agg_loc.get_or_insert(loc);
        st.aggs.push(call);
        let at = st.aggs.len() - 1;
        Ok(self.lift(target, Expr::Agg(at, ty)))
    }

    /// The query level an aggregate's arguments make it belong to
    /// (`check_agglevels_and_constraints`): the lowest one whose columns they read (None:
    /// they read none, it is the current one).
    fn agg_level_read(&self, call: &AggCall) -> Option<usize> {
        let cur = self.levels.len() - 1;
        let mut up: Option<usize> = None;
        // (check_agg_arguments: not the direct arguments of an ordered-set aggregate.)
        let mut exprs: Vec<&Expr> = call.args.iter().collect();
        exprs.extend(call.filter.as_ref());
        exprs.extend(call.order_by.iter().map(|k| &k.expr));
        for e in exprs {
            if let Some(u) = self.levels_up(e, cur) {
                up = Some(up.map_or(u, |x| x.min(u)));
            }
        }
        up.map(|u| cur - u.min(cur))
    }

    /// How many levels up from `level` the nearest column `e` reads is.
    fn levels_up(&self, e: &Expr, level: usize) -> Option<usize> {
        let mut best: Option<usize> = None;
        let mut note = |u: usize| best = Some(best.map_or(u, |b: usize| b.min(u)));
        e.walk(&mut |x| match x {
            Expr::Col(..) => note(0),
            Expr::Outer(1, i, _) => {
                match level
                    .checked_sub(1)
                    .and_then(|_| self.levels.get(level))
                    .and_then(|l| l.params.get(*i))
                    .and_then(|p| self.levels_up(p, level - 1))
                {
                    Some(u) => note(u + 1),
                    None => note(0),
                }
            }
            Expr::Outer(..) => note(0),
            _ => {}
        });
        best
    }

    /// `e`, an expression at level `from`, in the terms of level `to` (an outer one): its
    /// outer references become what they reference.
    fn unlift(&self, e: Expr, from: usize, to: usize) -> Expr {
        if from <= to {
            return e;
        }
        match e {
            Expr::Outer(1, i, ty) => match self.levels.get(from).and_then(|l| l.params.get(i)) {
                Some(p) => self.unlift(p.clone(), from - 1, to),
                None => Expr::Outer(1, i, ty),
            },
            e => {
                let keep = e.clone();
                e.map_children(&mut |c| Ok(self.unlift(c, from, to)))
                    .unwrap_or(keep)
            }
        }
    }

    /// Bind an aggregate's arguments and resolve it.
    pub(crate) fn agg_call(
        &mut self,
        schema: Option<&str>,
        name: &str,
        f: &pb::FuncCall,
    ) -> Result<AggCall> {
        let loc = f.location;
        let reg = agg_registry();
        let mut sigs = if schema.is_none_or(|s| s == "pg_catalog") {
            reg.sigs(name)
        } else {
            Vec::new()
        };
        // User-defined ones compete with the built-in ones.
        let users = self.user_aggs(schema, name);
        // An ordered-set aggregate made of a built-in one's support functions
        // (ordered_set_transition + percentile_disc_final, ...) is that built-in one.
        if f.agg_within_group
            && let [u] = users.as_slice()
            && let Some(a) = &u.agg
            && matches!(a.kind, 'o' | 'h')
            && (3973..=3993).contains(&a.finalfn)
            && a.finalfn % 2 == 1
            && let Some(&(_, builtin, _)) =
                func::pg_oids::PROCS.iter().find(|p| p.0 == a.finalfn - 1)
        {
            return self.agg_call(Some("pg_catalog"), builtin, f);
        }
        let path = self.cat.routine_path(&self.ctx.search_path);
        let user_sigs: Vec<Arc<FuncDef>> = users
            .iter()
            .map(|u| {
                // As a view shows it: qualified when its schema isn't on the path.
                let shown = if path.contains(&u.schema) {
                    crate::catalog::quote_ident(&u.name)
                } else {
                    format!(
                        "{}.{}",
                        crate::catalog::quote_ident(self.cat.schema_name(u.schema)),
                        crate::catalog::quote_ident(&u.name)
                    )
                };
                let d = user_agg_sig(u, &shown);
                if f.func_variadic && d.variadic {
                    // Called with VARIADIC: the array is the argument.
                    Arc::new(FuncDef {
                        variadic: false,
                        ..(*d).clone()
                    })
                } else {
                    d
                }
            })
            .collect();
        sigs.extend(user_sigs.iter().cloned());
        let ordered_set = f.agg_within_group;
        let (direct, args, order_by): (Vec<Expr>, Vec<Expr>, Vec<SortKey>) = if ordered_set {
            let direct = f
                .args
                .iter()
                .map(|a| self.bind_expr(a))
                .collect::<Result<Vec<_>>>()?;
            let mut args = Vec::new();
            let mut keys = Vec::new();
            for s in &f.agg_order {
                let Some(NodeEnum::SortBy(sb)) = inner(s) else {
                    continue;
                };
                let e = self.bind_expr(sb.node.as_deref().unwrap())?;
                keys.push(self.sort_key_for(e.clone(), sb)?);
                args.push(e);
            }
            (direct, args, keys)
        } else if f.agg_star {
            (vec![], vec![], vec![])
        } else if f.args.is_empty() && name == "count" {
            // The parameterless aggregate is only called as count(*).
            return Err(with_pos(
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("{name}(*) must be used to call a parameterless aggregate function"),
                ),
                f.location,
            ));
        } else {
            let args = f
                .args
                .iter()
                .map(|a| self.bind_expr(a))
                .collect::<Result<Vec<_>>>()?;
            let mut keys = Vec::new();
            for s in &f.agg_order {
                let Some(NodeEnum::SortBy(sb)) = inner(s) else {
                    continue;
                };
                let e = self.bind_expr(sb.node.as_deref().unwrap())?;
                keys.push(self.sort_key_for(e, sb)?);
            }
            (vec![], args, keys)
        };
        let mut types: Vec<DataType> = direct.iter().map(|a| a.ty()).collect();
        types.extend(args.iter().map(|a| a.ty()));
        if ordered_set
            && user_sigs.is_empty()
            && let Some(def) = reg
                .by_name
                .get(name)
                .and_then(|v| {
                    v.iter()
                        .find(|d| d.kind == crate::exec::agg::AggKind::Hypothetical)
                })
                .cloned()
        {
            return self.hypothetical_call(name, def, direct, args, order_by, f);
        }
        let res = match select_candidate(&sigs, &types, false) {
            Ok(Some(r)) => r,
            Ok(None) => return Err(with_pos(func::no_function(name, &types), loc)),
            Err(_) => {
                return Err(with_pos(
                    Error::new(
                        code::AMBIGUOUS_FUNCTION,
                        format!(
                            "function {name}({}) is not unique",
                            types
                                .iter()
                                .map(|t| t.display())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ),
                    loc,
                ));
            }
        };
        let user = user_sigs
            .iter()
            .position(|d| Arc::ptr_eq(d, &res.def))
            .map(|i| users[i].clone());
        let def = match &user {
            Some(_) => Arc::new(crate::exec::agg::AggDef {
                sig: res.def.clone(),
                make: crate::exec::agg::user_accum,
                kind: crate::exec::agg::AggKind::Normal,
                skip_nulls: false,
            }),
            None => reg
                .by_sig(&res.def)
                .ok_or_else(|| crate::error::internal("aggregate signature"))?,
        };
        if ordered_set != def.kind.is_ordered() {
            return Err(with_pos(
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    if ordered_set {
                        format!(
                            "{name} is not an ordered-set aggregate, so it cannot have WITHIN GROUP"
                        )
                    } else {
                        format!("WITHIN GROUP is required for ordered-set aggregate {name}")
                    },
                ),
                loc,
            ));
        }
        let nd = direct.len();
        let mut d2 = Vec::new();
        for (a, t) in direct.into_iter().zip(res.arg_types.iter()) {
            d2.push(self.coerce_implicit(a, t)?);
        }
        let mut a2 = Vec::new();
        for (a, t) in args.into_iter().zip(res.arg_types[nd..].iter()) {
            a2.push(self.coerce_implicit(a, t)?);
        }
        // A VARIADIC user-defined aggregate: the trailing arguments make up its array.
        if user.is_some() && res.def.variadic && !ordered_set && !res.def.args.is_empty() {
            let fixed = res.def.args.len() - 1;
            if a2.len() >= fixed {
                let tail = a2.split_off(fixed);
                let mut elem = res.arg_types.get(nd + fixed).cloned().unwrap_or(DataType::Text);
                if elem.is_polymorphic() || matches!(elem, DataType::Unknown) {
                    let tys: Vec<DataType> = tail.iter().map(|a| a.ty()).collect();
                    elem = super::unify_types(&tys, "VARIADIC").map_err(|e| with_pos(e, loc))?;
                    if matches!(elem, DataType::Unknown) {
                        elem = DataType::Text;
                    }
                }
                let items = tail
                    .into_iter()
                    .map(|a| self.coerce_implicit(a, &elem))
                    .collect::<Result<Vec<_>>>()?;
                let ty = DataType::array_of(elem);
                a2.push(Expr::Array(items, ty));
            }
        }
        let arg_types: Vec<DataType> = a2.iter().map(|a| a.ty()).collect();
        let filter = match f.agg_filter.as_deref() {
            Some(fl) => {
                let e = self.in_clause(Clause::Filter, |b| b.bind_expr(fl))?;
                Some(self.coerce_bool_at(e, "FILTER", fl)?)
            }
            None => None,
        };
        if f.agg_distinct && !ordered_set {
            for (a, n) in a2.iter().zip(&f.args) {
                super::select::require_equality(&a.ty(), super::ast::location(n))?;
            }
        }
        if f.agg_distinct && !order_by.is_empty() {
            for (i, k) in order_by.iter().enumerate() {
                let at = match f.agg_order.get(i).and_then(inner) {
                    Some(NodeEnum::SortBy(sb)) => sb.node.as_deref().map_or(loc, ast::location),
                    _ => loc,
                };
                // findTargetlistEntrySQL99 strips implicit coercions from the arguments.
                fn strip(mut e: &Expr) -> &Expr {
                    while let Expr::Cast(inner, _, _, false) = e {
                        e = inner;
                    }
                    e
                }
                let explicit_cast = matches!(&k.expr, Expr::Cast(_, _, _, true));
                if !a2.iter().any(|a| {
                    let s = strip(a);
                    s.same_as(&k.expr) && (!explicit_cast || matches!(s, Expr::Cast(_, _, _, true)))
                }) {
                    return Err(with_pos(
                        Error::new(
                            code::INVALID_COLUMN_REFERENCE,
                            "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list",
                        ),
                        at,
                    ));
                }
            }
        }
        // DISTINCT under a nondeterministic collation finds its equal values equal; under
        // one that doesn't sort by bytes, it sorts its input in its order.
        if f.agg_distinct && !self.deparse {
            a2 = a2
                .iter()
                .map(|a| match self.nondeterministic_key(a) {
                    k @ Expr::Collate(..) => k,
                    _ => self.collated_key(a),
                })
                .collect();
        }
        if let Some(u) = user {
            self.require_function(u.oid);
            let ua = self.user_agg(&u, &arg_types, &res.ret, loc)?;
            return Ok(AggCall {
                def,
                args: a2,
                distinct: f.agg_distinct,
                filter,
                order_by,
                direct: d2,
                ty: res.ret.clone(),
                arg_types,
                cmp: None,
                json_ctor: false,
                user: Some(Arc::new(ua)),
            });
        }
        // min/max over types whose order isn't the storage order use the type's `<`.
        let cmp = match arg_types.first() {
            Some(t @ DataType::Enum(..)) if matches!(name, "min" | "max") => {
                Some(resolve_binop("<", t, t)?.def)
            }
            // Under a collation that doesn't sort by bytes, its "<".
            Some(t) if matches!(name, "min" | "max") && t.collatable() && !self.deparse => {
                match a2.first().map(|a| self.collation_of(a)).transpose()? {
                    Some(super::Coll::Implicit(c) | super::Coll::Explicit(c, _)) => {
                        match crate::types::collation::by_name(&self.cat, &c)
                            .filter(|k| !k.bytewise())
                        {
                            Some(coll) => {
                                let lt = resolve_binop("<", t, t)?.def;
                                Some(Arc::new(FuncDef {
                                    coll: Some(coll),
                                    ..(*lt).clone()
                                }))
                            }
                            None => None,
                        }
                    }
                    // No collation (the input's implicit ones conflict): text_larger /
                    // text_smaller fail once they compare two values.
                    Some(super::Coll::None | super::Coll::Conflict(..)) => {
                        let lt = resolve_binop("<", t, t)?.def;
                        Some(Arc::new(FuncDef {
                            imp: indeterminate::<0>,
                            ..(*lt).clone()
                        }))
                    }
                    _ => None,
                }
            }
            // Composites (and arrays of them) under their fields' collations.
            Some(t) if matches!(name, "min" | "max") && t.holds_composite() => {
                match a2.first().and_then(|a| self.record_coll(a)) {
                    Some(coll) => {
                        let lt = resolve_binop("<", t, t)?.def;
                        Some(Arc::new(FuncDef {
                            coll: Some(coll),
                            ..(*lt).clone()
                        }))
                    }
                    None => None,
                }
            }
            _ => None,
        };
        let ret = match (&res.ret, arg_types.first()) {
            // (min(record) / max(record): a composite's are records.)
            (_, Some(DataType::Row(_))) if matches!(name, "min" | "max") => DataType::Record,
            // min/max keep the input's typmod-less type.
            (_, Some(t)) if matches!(name, "min" | "max") => t.without_typmod(),
            (r, _) => r.clone(),
        };
        Ok(AggCall {
            def,
            args: a2,
            distinct: f.agg_distinct,
            filter,
            order_by,
            direct: d2,
            ty: ret,
            arg_types,
            cmp,
            json_ctor: false,
            user: None,
        })
    }

    /// rank(...) WITHIN GROUP (ORDER BY ...): each hypothetical argument pairs with an ordering
    /// column, both coerced to a common type.
    fn hypothetical_call(
        &mut self,
        name: &str,
        def: Arc<crate::exec::agg::AggDef>,
        direct: Vec<Expr>,
        args: Vec<Expr>,
        mut order_by: Vec<SortKey>,
        f: &pb::FuncCall,
    ) -> Result<AggCall> {
        let loc = f.location;
        if direct.len() != args.len() {
            let types: Vec<DataType> = direct.iter().chain(&args).map(|a| a.ty()).collect();
            return Err(with_pos(
                Error::new(code::UNDEFINED_FUNCTION, format!("function {name}({}) does not exist", func::type_list(&types))).hint(format!(
                    "To use the hypothetical-set aggregate {name}, the number of hypothetical direct arguments (here {}) must match the number of ordering columns (here {}).",
                    direct.len(),
                    args.len()
                )),
                loc,
            ));
        }
        let mut d2 = Vec::new();
        let mut a2 = Vec::new();
        for (i, (d, a)) in direct.into_iter().zip(args).enumerate() {
            let at = f.args.get(i).map(ast::location).unwrap_or(loc);
            let ty = super::unify_types(&[a.ty(), d.ty()], "WITHIN GROUP")
                .map_err(|e| with_pos(e, at))?;
            // Each direct argument and its ordering value compare under one collation.
            if ty.collatable() || matches!(ty, DataType::Unknown) {
                self.collation_of(&d)?.merge(self.collation_of(&a)?)?;
            }
            let ty = if matches!(ty, DataType::Unknown) {
                DataType::Text
            } else {
                ty
            };
            d2.push(self.coerce_implicit(d, &ty).map_err(|e| with_pos(e, at))?);
            let a = self.coerce_implicit(a, &ty)?;
            order_by[i].expr = a.clone();
            a2.push(a);
        }
        let filter = match f.agg_filter.as_deref() {
            Some(fl) => {
                let e = self.in_clause(Clause::Filter, |b| b.bind_expr(fl))?;
                Some(self.coerce_bool_at(e, "FILTER", fl)?)
            }
            None => None,
        };
        let arg_types: Vec<DataType> = a2.iter().map(|a| a.ty()).collect();
        let ty = match &def.sig.ret {
            crate::sql::func::Ret::Fixed(t) => t.clone(),
            _ => DataType::Int8,
        };
        Ok(AggCall {
            def,
            args: a2,
            distinct: false,
            filter,
            order_by,
            direct: d2,
            ty,
            arg_types,
            cmp: None,
            json_ctor: false,
            user: None,
        })
    }

    /// A user-defined aggregate's transition and final functions for a call with arguments of
    /// `arg_types` and result `ret` (polymorphic types resolved).
    fn user_agg(
        &mut self,
        u: &crate::catalog::Function,
        arg_types: &[DataType],
        ret: &DataType,
        loc: i32,
    ) -> Result<crate::sql::plan::UserAgg> {
        let spec = u
            .agg
            .as_deref()
            .ok_or_else(|| crate::error::internal("not an aggregate"))?;
        // The type polymorphic parameters stand for.
        let mut elem = None;
        for ((_, p), a) in u.args.iter().zip(arg_types) {
            let e = match p {
                DataType::AnyElement
                | DataType::AnyNonArray
                | DataType::AnyEnum
                | DataType::AnyCompatible
                | DataType::AnyCompatibleNonArray => Some(a.clone()),
                DataType::AnyArray | DataType::AnyCompatibleArray => a.elem().cloned(),
                _ => None,
            };
            elem = elem.or(e);
        }
        let subst = |t: &DataType| match (t, &elem) {
            (
                DataType::AnyElement
                | DataType::AnyNonArray
                | DataType::AnyEnum
                | DataType::AnyCompatible
                | DataType::AnyCompatibleNonArray,
                Some(e),
            ) => e.clone(),
            (DataType::AnyArray | DataType::AnyCompatibleArray, Some(e)) => {
                DataType::array_of(e.clone())
            }
            (t, _) => t.clone(),
        };
        let stype = subst(&spec.transtype);
        let mut row = vec![Expr::Col(0, stype.clone())];
        row.extend(
            arg_types
                .iter()
                .enumerate()
                .map(|(i, t)| Expr::Col(i + 1, t.clone())),
        );
        let (trans, trans_strict) = self.agg_support_call(spec.transfn, row, &stype, loc)?;
        let (final_, final_strict) = if spec.finalfn != 0 {
            let mut row = vec![Expr::Col(0, stype.clone())];
            if spec.finalextra {
                row.extend(
                    arg_types
                        .iter()
                        .map(|t| Expr::Const(Value::Null, t.clone())),
                );
            }
            let (e, s) = self.agg_support_call(spec.finalfn, row, ret, loc)?;
            (Some(e), s)
        } else {
            (None, false)
        };
        let init = match &spec.initval {
            Some(v) => Some(self.coerce_implicit(
                Expr::Const(Value::Text(Arc::from(v.as_str())), DataType::Unknown),
                &stype,
            )?),
            None => None,
        };
        // The moving-aggregate implementation, when it has an inverse transition.
        let moving = if spec.minvtransfn != 0 && spec.mtransfn != 0 {
            let mstype = subst(spec.mtranstype.as_ref().unwrap_or(&spec.transtype));
            let mut row = vec![Expr::Col(0, mstype.clone())];
            row.extend(
                arg_types
                    .iter()
                    .enumerate()
                    .map(|(i, t)| Expr::Col(i + 1, t.clone())),
            );
            let (trans, trans_strict) =
                self.agg_support_call(spec.mtransfn, row.clone(), &mstype, loc)?;
            let inverse = self.agg_support_call(spec.minvtransfn, row, &mstype, loc)?;
            let (final_, final_strict) = if spec.mfinalfn != 0 {
                let mut row = vec![Expr::Col(0, mstype.clone())];
                if spec.mfinalextra {
                    row.extend(
                        arg_types
                            .iter()
                            .map(|t| Expr::Const(Value::Null, t.clone())),
                    );
                }
                let (e, s) = self.agg_support_call(spec.mfinalfn, row, ret, loc)?;
                (Some(e), s)
            } else {
                (None, false)
            };
            let init = match &spec.minitval {
                Some(v) => Some(self.coerce_implicit(
                    Expr::Const(Value::Text(Arc::from(v.as_str())), DataType::Unknown),
                    &mstype,
                )?),
                None => None,
            };
            Some(Box::new(crate::sql::plan::UserAgg {
                trans,
                trans_strict,
                final_,
                final_strict,
                init,
                moving: None,
                inverse: Some(inverse),
                combine: None,
            }))
        } else {
            None
        };
        // The combine function, for a state other than `internal` (whose partial states
        // travel serialized).
        let combine = if spec.combinefn != 0 && spec.transtype.oid() != 2281 {
            let row = vec![Expr::Col(0, stype.clone()), Expr::Col(1, stype.clone())];
            self.agg_support_call(spec.combinefn, row, &stype, loc).ok()
        } else {
            None
        };
        Ok(crate::sql::plan::UserAgg {
            trans,
            trans_strict,
            final_,
            final_strict,
            init,
            moving,
            inverse: None,
            combine,
        })
    }

    /// A call of an aggregate's support function (a user function or a built-in) on `args`;
    /// whether it is strict.
    fn agg_support_call(
        &mut self,
        oid: u32,
        args: Vec<Expr>,
        ret: &DataType,
        loc: i32,
    ) -> Result<(Expr, bool)> {
        if let Some(f) = self.cat.functions.get(&oid).cloned() {
            let params: Vec<(Option<String>, DataType, Option<String>)> = f
                .args
                .iter()
                .map(|(n, t)| (n.clone(), t.clone(), None))
                .collect();
            let strict = f.strict;
            let written: Vec<DataType> = args.iter().map(Expr::ty).collect();
            let f = resolve_polymorphic(f, &params, &args, &written)?;
            let mut coerced = Vec::with_capacity(args.len());
            for (a, (_, t)) in args.into_iter().zip(&f.args) {
                coerced.push(self.coerce_implicit(a, t)?);
            }
            return Ok((self.user_call(f, coerced, loc)?, strict));
        }
        let def = func::def_by_oid(oid)
            .ok_or_else(|| crate::error::internal("aggregate support function"))?;
        let ty = match &def.ret {
            func::Ret::Fixed(t) => t.clone(),
            _ => ret.clone(),
        };
        Ok((Expr::Call(def.clone(), args, ty), def.strict))
    }

    /// `ORDER BY x USING op` with a made operator `op` over x's type: whether it sorts
    /// descending (a btree family's strategy 5, else 1) and the "less than" comparing by the
    /// family's comparison function (FUNCTION 1) when that is a made function (None: the
    /// type's own order). Ok(None) when no made operator matches.
    fn user_ordering_op(
        &self,
        parts: &[String],
        ty: &DataType,
        loc: i32,
    ) -> Result<Option<(bool, Option<Arc<FuncDef>>)>> {
        let Some(op) = parts.last() else {
            return Ok(None);
        };
        if self.cat.operators.is_empty() {
            return Ok(None);
        }
        let path: Vec<u32> = match parts {
            [s, _] => self
                .cat
                .schema_by_name(s)
                .map(|x| vec![x.oid])
                .unwrap_or_default(),
            _ => self.cat.routine_path(&self.ctx.search_path),
        };
        let Some(o) = path.iter().find_map(|sc| {
            self.cat
                .operators
                .values()
                .find(|o| {
                    o.schema == *sc
                        && &o.name == op
                        && o.left.as_ref().is_some_and(|l| l.same_base(ty))
                        && o.right.same_base(ty)
                })
                .cloned()
        }) else {
            return Ok(None);
        };
        // get_ordering_op_properties: a "<" or ">" member of a btree family.
        let Some(m) = self
            .cat
            .opmembers_all(false)
            .into_iter()
            .find(|m| m.object == o.oid && m.method == 403 && matches!(m.number, 1 | 5))
        else {
            return Err(with_pos(
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("operator {op} is not a valid ordering operator"),
                )
                .hint("Ordering operators must be \"<\" or \">\" members of btree operator families."),
                loc,
            ));
        };
        let cmp = self.cat.opmembers_all(true).into_iter().find(|f| {
            f.family == m.family && f.number == 1 && f.left == m.left && f.right == m.right
        });
        let using = cmp
            .filter(|f| self.cat.functions.contains_key(&f.object))
            .and_then(|f| crate::catalog::partition::comparator_for_op(f.object, op))
            .and_then(|coll| {
                let lt = resolve_binop("<", ty, ty)
                    .or_else(|_| resolve_binop("<", &DataType::Int4, &DataType::Int4))
                    .ok()?
                    .def;
                Some(Arc::new(FuncDef {
                    coll: Some(coll),
                    ..(*lt).clone()
                }))
            });
        Ok(Some((m.number == 5, using)))
    }

    /// A SortKey for an already-bound expression from a SortBy node.
    pub(crate) fn sort_key_for(&mut self, expr: Expr, sb: &pb::SortBy) -> Result<SortKey> {
        let dir = pb::SortByDir::try_from(sb.sortby_dir).unwrap_or(pb::SortByDir::SortbyDefault);
        let nulls = pb::SortByNulls::try_from(sb.sortby_nulls)
            .unwrap_or(pb::SortByNulls::SortbyNullsDefault);
        let ty = expr.ty();
        let (desc, using) = match dir {
            pb::SortByDir::SortbyDesc => (true, None),
            pb::SortByDir::SortbyUsing => {
                let op = names(&sb.use_op).last().cloned().unwrap_or_default();
                if let Some(k) = self.user_ordering_op(&names(&sb.use_op), &ty, sb.location)? {
                    return Ok(SortKey {
                        expr,
                        desc: k.0,
                        nulls_first: match nulls {
                            pb::SortByNulls::SortbyNullsFirst => true,
                            pb::SortByNulls::SortbyNullsLast => false,
                            _ => k.0,
                        },
                        using: k.1,
                    });
                }
                let res = resolve_binop(&op, &ty, &ty).map_err(|e| with_pos(e, sb.location))?;
                // The non-default btree families' ordering operators (text_pattern_ops,
                // record_image_ops): sort by the "less than" one.
                let lt_of = match op.as_str() {
                    "~<~" | "~>~" if matches!(ty, DataType::Text | DataType::Bpchar(_) | DataType::Varchar(_)) => Some("~<~"),
                    "*<" | "*>" if matches!(ty, DataType::Row(_) | DataType::Record) => Some("*<"),
                    _ => None,
                };
                if let Some(lt) = lt_of {
                    let def = resolve_binop(lt, &ty, &ty).map_err(|e| with_pos(e, sb.location))?.def;
                    let desc = lt != op;
                    return Ok(SortKey {
                        expr,
                        desc,
                        nulls_first: match nulls {
                            pb::SortByNulls::SortbyNullsFirst => true,
                            pb::SortByNulls::SortbyNullsLast => false,
                            _ => desc,
                        },
                        using: Some(def),
                    });
                }
                if op != "<" && op != ">" {
                    return Err(with_pos(
                        Error::new(code::WRONG_OBJECT_TYPE, format!("operator {op} is not a valid ordering operator"))
                            .hint("Ordering operators must be \"<\" or \">\" members of btree operator families."),
                        sb.location,
                    ));
                }
                (
                    op == ">",
                    if matches!(ty, DataType::Enum(..)) {
                        Some(resolve_binop("<", &ty, &ty)?.def)
                    } else {
                        let _ = res;
                        None
                    },
                )
            }
            _ => (false, None),
        };
        let nulls_first = match nulls {
            pb::SortByNulls::SortbyNullsFirst => true,
            pb::SortByNulls::SortbyNullsLast => false,
            _ => desc,
        };
        // A key whose implicit collations conflict is an error; one with no collation fails
        // when rows are compared.
        let mut using = using;
        if ty.collatable() {
            match self.collation_of(&expr)? {
                super::Coll::Conflict(a, b) => {
                    // (At the expression bringing in the second collation.)
                    if let Some(n) = sb.node.as_deref() {
                        self.check_key_collation(&expr, n, &[])?;
                    }
                    return Err(Error::new(
                        code::COLLATION_MISMATCH,
                        format!("collation mismatch between implicit collations \"{a}\" and \"{b}\""),
                    )
                    .hint("You can choose the collation by applying the COLLATE clause to one or both expressions."));
                }
                super::Coll::None => {
                    let lt = resolve_binop("<", &ty, &ty)?.def;
                    using = Some(Arc::new(FuncDef {
                        imp: indeterminate::<0>,
                        ..(*lt).clone()
                    }));
                }
                // A collation that doesn't sort by bytes (ICU) orders by its "<".
                super::Coll::Implicit(c) | super::Coll::Explicit(c, _)
                    if using.is_none() && !self.deparse =>
                {
                    if let Some(coll) = crate::types::collation::by_name(&self.cat, &c)
                        .filter(|k| !k.bytewise())
                    {
                        let lt = resolve_binop("<", &ty, &ty)?.def;
                        using = Some(Arc::new(FuncDef {
                            coll: Some(coll),
                            ..(*lt).clone()
                        }));
                    }
                }
                _ => {}
            }
        }
        // Ranges order by their bounds under the subtype collation, composites (and arrays
        // and ranges of them) by their fields under theirs.
        if using.is_none()
            && !self.deparse
            && let Some(coll) = match &ty {
                DataType::Range(rt) | DataType::Multirange(rt) if rt.coll().is_some() => rt.coll(),
                _ => self.record_coll(&expr),
            }
        {
            let lt = resolve_binop("<", &ty, &ty)?.def;
            using = Some(Arc::new(FuncDef {
                coll: Some(coll),
                ..(*lt).clone()
            }));
        }
        let using = using.or_else(|| {
            if matches!(ty, DataType::Enum(..)) {
                resolve_binop("<", &ty, &ty).ok().map(|r| r.def)
            } else {
                None
            }
        });
        fn unorderable(ty: &DataType) -> bool {
            matches!(
                ty,
                DataType::Json
                    | DataType::Xml
                    | DataType::Gtsvector
                    | DataType::PgBrinBloomSummary
                    | DataType::PgBrinMinmaxMultiSummary
            ) || ty.category() == crate::types::Category::Geometric
                // (A composite type orders by its columns: each must have an ordering.)
                || matches!(ty, DataType::Row(rt) if rt.fields.iter().any(|(_, t)| unorderable(t)))
        }
        if unorderable(&ty) {
            return Err(with_pos(
                Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!(
                        "could not identify an ordering operator for type {}",
                        ty.display()
                    ),
                )
                .hint("Use an explicit ordering operator or modify the query."),
                if sb.location >= 0 {
                    sb.location
                } else {
                    sb.node.as_deref().map_or(-1, super::ast::location)
                },
            ));
        }
        Ok(SortKey {
            expr,
            desc,
            nulls_first,
            using,
        })
    }

    // ── window functions ──

    fn bind_window_call(&mut self, name: &str, f: &pb::FuncCall) -> Result<Expr> {
        let clause = self.clause();
        let loc = f.location;
        if self.levels.last().unwrap().st.in_window {
            return Err(with_pos(
                Error::new(
                    code::WINDOWING_ERROR,
                    "window function calls cannot be nested",
                ),
                loc,
            ));
        }
        if !clause.windows_ok() || self.levels.last().unwrap().st.in_agg {
            let c = if self.levels.last().unwrap().st.in_agg {
                Clause::Other
            } else {
                clause
            };
            if self.levels.last().unwrap().st.in_agg {
                return Err(with_pos(
                    Error::new(
                        code::GROUPING_ERROR,
                        "aggregate function calls cannot contain window function calls",
                    ),
                    loc,
                ));
            }
            return Err(with_pos(not_allowed("window", c), loc));
        }
        if f.agg_distinct {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "DISTINCT is not implemented for window functions",
                ),
                loc,
            ));
        }
        if f.agg_within_group && agg_registry().by_name.contains_key(name) {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("OVER is not supported for ordered-set aggregate {name}"),
                ),
                loc,
            ));
        }
        self.level().st.in_window = true;
        let r = self.window_call(name, f);
        self.level().st.in_window = false;
        let call = r?;
        let ty = call.ty.clone();
        let deparse = self.deparse;
        let st = &mut self.level().st;
        if deparse {
            st.window_defs.push((
                name.to_string(),
                f.over.as_deref().cloned().unwrap_or_default(),
            ));
        }
        st.windows.push(call);
        Ok(Expr::Window(st.windows.len() - 1, ty))
    }

    fn window_call(&mut self, name: &str, f: &pb::FuncCall) -> Result<WindowCall> {
        let loc = f.location;
        let builtin = match name {
            "row_number" => Some(WinKind::RowNumber),
            "rank" => Some(WinKind::Rank),
            "dense_rank" => Some(WinKind::DenseRank),
            "percent_rank" => Some(WinKind::PercentRank),
            "cume_dist" => Some(WinKind::CumeDist),
            "ntile" => Some(WinKind::Ntile),
            "lag" => Some(WinKind::Lag),
            "lead" => Some(WinKind::Lead),
            "first_value" => Some(WinKind::FirstValue),
            "last_value" => Some(WinKind::LastValue),
            "nth_value" => Some(WinKind::NthValue),
            _ => None,
        };
        let (func, args, ty) = match builtin {
            Some(k) => {
                let mut args = f
                    .args
                    .iter()
                    .map(|a| self.bind_expr(a))
                    .collect::<Result<Vec<_>>>()?;
                let n = args.len();
                let bad = || {
                    let types: Vec<DataType> = Vec::new();
                    with_pos(func::no_function(name, &types), loc)
                };
                let ty = match k {
                    WinKind::RowNumber | WinKind::Rank | WinKind::DenseRank => {
                        if n != 0 {
                            return Err(bad());
                        }
                        DataType::Int8
                    }
                    WinKind::PercentRank | WinKind::CumeDist => {
                        if n != 0 {
                            return Err(bad());
                        }
                        DataType::Float8
                    }
                    WinKind::Ntile => {
                        if n != 1 {
                            return Err(bad());
                        }
                        args[0] = self.coerce_implicit(args[0].clone(), &DataType::Int4)?;
                        DataType::Int4
                    }
                    WinKind::Lag | WinKind::Lead => {
                        if !(1..=3).contains(&n) {
                            return Err(bad());
                        }
                        if n >= 2 {
                            args[1] = self.coerce_implicit(args[1].clone(), &DataType::Int4)?;
                        }
                        if n == 3 {
                            let (u, t) =
                                self.unify(vec![args[0].clone(), args[2].clone()], "lag")?;
                            args[0] = u[0].clone();
                            args[2] = u[1].clone();
                            t
                        } else {
                            let t = args[0].ty();
                            if matches!(t, DataType::Unknown) {
                                args[0] = self.coerce_implicit(args[0].clone(), &DataType::Text)?;
                            }
                            args[0].ty()
                        }
                    }
                    WinKind::FirstValue | WinKind::LastValue => {
                        if n != 1 {
                            return Err(bad());
                        }
                        if matches!(args[0].ty(), DataType::Unknown) {
                            args[0] = self.coerce_implicit(args[0].clone(), &DataType::Text)?;
                        }
                        args[0].ty()
                    }
                    WinKind::NthValue => {
                        if n != 2 {
                            return Err(bad());
                        }
                        args[1] = self.coerce_implicit(args[1].clone(), &DataType::Int4)?;
                        if matches!(args[0].ty(), DataType::Unknown) {
                            args[0] = self.coerce_implicit(args[0].clone(), &DataType::Text)?;
                        }
                        args[0].ty()
                    }
                };
                (WinFunc::Builtin(k), args, ty)
            }
            // A user function (LANGUAGE internal WINDOW) over a built-in window function's
            // code: its parameters (names, defaults, polymorphism) apply.
            None if self.cat.functions.values().any(|u| {
                u.name == name && u.language == "internal" && u.body.starts_with("window_")
            }) =>
            {
                let Some((fdef, args)) = self.resolve_user_function(None, name, f)? else {
                    let types: Vec<DataType> = f.args.iter().map(|_| DataType::Unknown).collect();
                    return Err(with_pos(func::no_function(name, &types), loc));
                };
                let k = match fdef.body.as_str() {
                    "window_row_number" => WinKind::RowNumber,
                    "window_rank" => WinKind::Rank,
                    "window_dense_rank" => WinKind::DenseRank,
                    "window_percent_rank" => WinKind::PercentRank,
                    "window_cume_dist" => WinKind::CumeDist,
                    "window_ntile" => WinKind::Ntile,
                    "window_first_value" => WinKind::FirstValue,
                    "window_last_value" => WinKind::LastValue,
                    "window_nth_value" => WinKind::NthValue,
                    b if b.starts_with("window_lag") => WinKind::Lag,
                    b if b.starts_with("window_lead") => WinKind::Lead,
                    b => {
                        return Err(with_pos(
                            crate::error::unsupported(format!("window function {b}")),
                            loc,
                        ));
                    }
                };
                (WinFunc::Builtin(k), args, fdef.ret.clone())
            }
            None => {
                if !agg_registry().by_name.contains_key(name)
                    && self.user_aggs(None, name).is_empty()
                {
                    if func::function_exists(name) {
                        return Err(with_pos(
                            Error::new(
                                code::WRONG_OBJECT_TYPE,
                                format!(
                                    "OVER specified, but {name} is not a window function nor an aggregate function"
                                ),
                            ),
                            loc,
                        ));
                    }
                    let types: Vec<DataType> = f.args.iter().map(|_| DataType::Unknown).collect();
                    return Err(with_pos(func::no_function(name, &types), loc));
                }
                if f.agg_within_group {
                    return Err(with_pos(
                        Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("OVER is not supported for ordered-set aggregate {name}"),
                        ),
                        loc,
                    ));
                }
                let call = self.agg_call(None, name, f)?;
                let ty = call.ty.clone();
                let args = call.args.clone();
                (WinFunc::Agg(call), args, ty)
            }
        };
        let over = f.over.as_deref().unwrap();
        let (partition_by, order_by, frame) =
            self.in_clause(Clause::Window, |b| b.bind_window_def(over))?;
        let partition_by = partition_by.iter().map(|e| self.collated_key(e)).collect();
        Ok(WindowCall {
            func,
            args,
            partition_by,
            order_by,
            frame,
            ty,
            ignore_nulls: false,
            subquery: None,
            // (`OVER w` alone: the named window itself.)
            name: f.over.as_deref().and_then(|w| {
                (!w.name.is_empty() && w.partition_clause.is_empty() && w.order_clause.is_empty() && w.refname.is_empty())
                    .then(|| w.name.clone())
            }),
        })
    }

    /// PARTITION BY / ORDER BY / frame of a window, resolving named windows.
    pub(crate) fn bind_window_def(
        &mut self,
        w: &pb::WindowDef,
    ) -> Result<(Vec<Expr>, Vec<SortKey>, Frame)> {
        // PARTITION BY / ORDER BY may call set-returning functions (they run below the window).
        let saved = std::mem::replace(&mut self.level().st.in_window, false);
        let r = self.bind_window_def_inner(w);
        self.level().st.in_window = saved;
        r
    }

    fn bind_window_def_inner(
        &mut self,
        w: &pb::WindowDef,
    ) -> Result<(Vec<Expr>, Vec<SortKey>, Frame)> {
        // `OVER w` / `OVER (w ...)`: start from the named window.
        let base_name = if !w.name.is_empty()
            && w.partition_clause.is_empty()
            && w.order_clause.is_empty()
            && w.refname.is_empty()
        {
            Some(w.name.clone())
        } else if !w.refname.is_empty() {
            Some(w.refname.clone())
        } else {
            None
        };
        let base = match &base_name {
            Some(n) => match self
                .levels
                .last()
                .unwrap()
                .st
                .named_windows
                .iter()
                .find(|(x, _)| x == n)
            {
                Some((_, d)) => Some(d.clone()),
                None => {
                    return Err(with_pos(
                        Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("window \"{n}\" does not exist"),
                        ),
                        w.location,
                    ));
                }
            },
            None => None,
        };
        let (pclause, oclause, frame_opts, start, end): (
            Vec<Node>,
            Vec<Node>,
            i32,
            Option<Box<Node>>,
            Option<Box<Node>>,
        ) = match &base {
            Some(b) if w.refname.is_empty() => (
                b.partition_clause.clone(),
                b.order_clause.clone(),
                b.frame_options,
                b.start_offset.clone(),
                b.end_offset.clone(),
            ),
            Some(b) => {
                if !w.partition_clause.is_empty() {
                    return Err(with_pos(
                        Error::new(
                            code::WINDOWING_ERROR,
                            format!(
                                "cannot override PARTITION BY clause of window \"{}\"",
                                w.refname
                            ),
                        ),
                        w.location,
                    ));
                }
                if !w.order_clause.is_empty() && !b.order_clause.is_empty() {
                    return Err(with_pos(
                        Error::new(
                            code::WINDOWING_ERROR,
                            format!(
                                "cannot override ORDER BY clause of window \"{}\"",
                                w.refname
                            ),
                        ),
                        w.location,
                    ));
                }
                if b.frame_options & FRAMEOPTION_NONDEFAULT != 0 {
                    return Err(with_pos(
                        Error::new(
                            code::WINDOWING_ERROR,
                            format!(
                                "cannot copy window \"{}\" because it has a frame clause",
                                w.refname
                            ),
                        )
                        .hint("Omit the parentheses in this OVER clause."),
                        w.location,
                    ));
                }
                let order = if w.order_clause.is_empty() {
                    b.order_clause.clone()
                } else {
                    w.order_clause.clone()
                };
                (
                    b.partition_clause.clone(),
                    order,
                    w.frame_options,
                    w.start_offset.clone(),
                    w.end_offset.clone(),
                )
            }
            None => (
                w.partition_clause.clone(),
                w.order_clause.clone(),
                w.frame_options,
                w.start_offset.clone(),
                w.end_offset.clone(),
            ),
        };
        let partition = pclause
            .iter()
            .map(|p| {
                let e = self.bind_expr(p)?;
                super::select::require_equality(&e.ty(), super::ast::location(p))?;
                Ok(e)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut order = Vec::new();
        for o in &oclause {
            let Some(NodeEnum::SortBy(sb)) = inner(o) else {
                continue;
            };
            let e = self.bind_expr(sb.node.as_deref().unwrap())?;
            order.push(self.sort_key_for(e, sb)?);
        }
        let frame = self.bind_frame(
            frame_opts,
            start.as_deref(),
            end.as_deref(),
            &order,
            w.location,
        )?;
        Ok((partition, order, frame))
    }

    fn bind_frame(
        &mut self,
        opts: i32,
        start: Option<&Node>,
        end: Option<&Node>,
        order: &[SortKey],
        loc: i32,
    ) -> Result<Frame> {
        if opts & FRAMEOPTION_NONDEFAULT == 0 {
            return Ok(Frame::default());
        }
        let unit = if opts & FRAMEOPTION_ROWS != 0 {
            FrameUnit::Rows
        } else if opts & FRAMEOPTION_GROUPS != 0 {
            FrameUnit::Groups
        } else {
            FrameUnit::Range
        };
        if unit == FrameUnit::Groups && order.is_empty() {
            return Err(with_pos(
                Error::new(
                    code::WINDOWING_ERROR,
                    "GROUPS mode requires an ORDER BY clause",
                ),
                loc,
            ));
        }
        let offset = |b: &mut Self, n: Option<&Node>| -> Result<Expr> {
            let n = n.unwrap();
            let e = b.in_clause(Clause::WindowFrame, |b| b.bind_expr(n))?;
            let at = super::ast::location(n);
            let (e, construct) = match unit {
                FrameUnit::Rows => (b.coerce_implicit(e, &DataType::Int8)?, "ROWS"),
                FrameUnit::Groups => (b.coerce_implicit(e, &DataType::Int8)?, "GROUPS"),
                FrameUnit::Range => {
                    if order.len() != 1 {
                        return Err(with_pos(
                            Error::new(
                                code::WINDOWING_ERROR,
                                "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column",
                            ),
                            loc,
                        ));
                    }
                    let kt = order[0].expr.ty().without_typmod();
                    // The sort column's btree family's in_range offset types, in catalog
                    // (right type oid) order.
                    let offsets: &[DataType] = match kt {
                        DataType::Int2 | DataType::Int4 => {
                            &[DataType::Int8, DataType::Int2, DataType::Int4]
                        }
                        DataType::Int8 => &[DataType::Int8],
                        DataType::Float4 | DataType::Float8 => &[DataType::Float8],
                        DataType::Numeric(_) => &[DataType::NUMERIC],
                        DataType::Date
                        | DataType::Timestamp(_)
                        | DataType::Timestamptz(_)
                        | DataType::Time(_)
                        | DataType::Timetz(_)
                        | DataType::Interval(_) => &[DataType::INTERVAL],
                        _ => &[],
                    };
                    // The opclass's input type (text_ops for varchar).
                    let shown = match &kt {
                        DataType::Varchar(_) => DataType::Text,
                        t => t.clone(),
                    };
                    let node_ty = e.ty();
                    if offsets.is_empty() {
                        return Err(with_pos(
                            Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!(
                                    "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {}",
                                    shown.display()
                                ),
                            ),
                            at,
                        ));
                    }
                    // Prefer the offset's own type, or for an unknown literal the column's.
                    let preferred = if matches!(node_ty, DataType::Unknown) {
                        kt.clone()
                    } else {
                        node_ty.clone()
                    };
                    let matches: Vec<&DataType> = offsets
                        .iter()
                        .filter(|t| func::can_coerce(&node_ty, t))
                        .collect();
                    let selected = matches
                        .iter()
                        .find(|t| t.same_base(&preferred))
                        .or(matches.last())
                        .copied()
                        .cloned();
                    let Some(selected) = selected else {
                        return Err(with_pos(
                            Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!(
                                    "RANGE with offset PRECEDING/FOLLOWING is not supported for column type {} and offset type {}",
                                    shown.display(),
                                    node_ty.display()
                                ),
                            )
                            .hint("Cast the offset value to an appropriate type."),
                            at,
                        ));
                    };
                    if matches.len() > 1 && !selected.same_base(&preferred) {
                        return Err(with_pos(
                            Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                format!(
                                    "RANGE with offset PRECEDING/FOLLOWING has multiple interpretations for column type {} and offset type {}",
                                    shown.display(),
                                    node_ty.display()
                                ),
                            )
                            .hint("Cast the offset value to the exact intended type."),
                            at,
                        ));
                    }
                    (b.coerce_implicit(e, &selected)?, "RANGE")
                }
            };
            // checkExprIsVarFree: no columns of this query level.
            let mut has_var = false;
            e.walk(&mut |x| {
                if matches!(x, Expr::Col(..)) {
                    has_var = true;
                }
            });
            if has_var {
                let var_at = n
                    .node
                    .as_ref()
                    .and_then(|x| {
                        x.nodes().into_iter().find_map(|(x, ..)| match x {
                            pg_query::NodeRef::ColumnRef(c) => Some(c.location),
                            _ => None,
                        })
                    })
                    .unwrap_or(at);
                return Err(with_pos(
                    Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        format!("argument of {construct} must not contain variables"),
                    ),
                    var_at,
                ));
            }
            Ok(e)
        };
        let start_b = if opts & FRAMEOPTION_START_UNBOUNDED_PRECEDING != 0 {
            FrameBound::UnboundedPreceding
        } else if opts & FRAMEOPTION_START_CURRENT_ROW != 0 {
            FrameBound::CurrentRow
        } else if opts & FRAMEOPTION_START_OFFSET_PRECEDING != 0 {
            FrameBound::Preceding(offset(self, start)?)
        } else if opts & FRAMEOPTION_START_OFFSET_FOLLOWING != 0 {
            FrameBound::Following(offset(self, start)?)
        } else if opts & FRAMEOPTION_START_UNBOUNDED_FOLLOWING != 0 {
            return Err(with_pos(
                Error::new(
                    code::WINDOWING_ERROR,
                    "frame start cannot be UNBOUNDED FOLLOWING",
                ),
                loc,
            ));
        } else {
            FrameBound::UnboundedPreceding
        };
        let end_b = if opts & FRAMEOPTION_BETWEEN == 0 {
            FrameBound::CurrentRow
        } else if opts & FRAMEOPTION_END_UNBOUNDED_FOLLOWING != 0 {
            FrameBound::UnboundedFollowing
        } else if opts & FRAMEOPTION_END_CURRENT_ROW != 0 {
            FrameBound::CurrentRow
        } else if opts & FRAMEOPTION_END_OFFSET_PRECEDING != 0 {
            FrameBound::Preceding(offset(self, end)?)
        } else if opts & FRAMEOPTION_END_OFFSET_FOLLOWING != 0 {
            FrameBound::Following(offset(self, end)?)
        } else if opts & FRAMEOPTION_END_UNBOUNDED_PRECEDING != 0 {
            return Err(with_pos(
                Error::new(
                    code::WINDOWING_ERROR,
                    "frame end cannot be UNBOUNDED PRECEDING",
                ),
                loc,
            ));
        } else {
            FrameBound::CurrentRow
        };
        // Start must not come after end.
        let rank = |b: &FrameBound| match b {
            FrameBound::UnboundedPreceding => 0,
            FrameBound::Preceding(_) => 1,
            FrameBound::CurrentRow => 2,
            FrameBound::Following(_) => 3,
            FrameBound::UnboundedFollowing => 4,
        };
        if matches!(start_b, FrameBound::CurrentRow) && matches!(end_b, FrameBound::Preceding(_)) {
            return Err(with_pos(
                Error::new(
                    code::WINDOWING_ERROR,
                    "frame starting from current row cannot have preceding rows",
                ),
                loc,
            ));
        }
        if matches!(start_b, FrameBound::Following(_)) && rank(&end_b) < 3 {
            return Err(with_pos(
                Error::new(
                    code::WINDOWING_ERROR,
                    "frame starting from following row cannot have preceding rows",
                ),
                loc,
            ));
        }
        let exclude = if opts & FRAMEOPTION_EXCLUDE_CURRENT_ROW != 0 {
            FrameExclude::CurrentRow
        } else if opts & FRAMEOPTION_EXCLUDE_GROUP != 0 {
            FrameExclude::Group
        } else if opts & FRAMEOPTION_EXCLUDE_TIES != 0 {
            FrameExclude::Ties
        } else {
            FrameExclude::NoOthers
        };
        Ok(Frame {
            unit,
            start: start_b,
            end: end_b,
            exclude,
            between: opts & FRAMEOPTION_BETWEEN != 0,
        })
    }

    fn bind_grouping(&mut self, g: &pb::GroupingFunc) -> Result<Expr> {
        // Like an aggregate, GROUPING belongs to the query level its arguments' columns
        // are of (the nearest).
        let params_before: Vec<usize> = self.levels.iter().map(|l| l.params.len()).collect();
        let mut args = Vec::with_capacity(g.args.len());
        for a in &g.args {
            args.push(self.bind_expr(a)?);
        }
        let cur = self.levels.len() - 1;
        let up = args
            .iter()
            .filter_map(|e| self.levels_up(e, cur))
            .min()
            .unwrap_or(0)
            .min(cur);
        let target = cur - up;
        let clause = self.levels[target].clause;
        if !matches!(clause, Clause::Select | Clause::Having | Clause::OrderBy) {
            return Err(with_pos(
                Error::new(
                    code::GROUPING_ERROR,
                    format!("grouping operations are not allowed in {}", clause.name()),
                ),
                g.location,
            ));
        }
        if g.args.len() > 31 {
            return Err(with_pos(
                Error::new(
                    code::TOO_MANY_ARGUMENTS,
                    "GROUPING must have fewer than 32 arguments",
                ),
                g.location,
            ));
        }
        let mut pos = Vec::new();
        for (e, n) in args.into_iter().zip(&g.args) {
            let e = self.unlift(e, cur, target);
            let groups = &self.levels[target].st.group_exprs;
            match groups.iter().position(|ge| ge.same_query_as(&e)) {
                Some(p) => pos.push(p),
                None => {
                    return Err(with_pos(
                        Error::new(
                            code::GROUPING_ERROR,
                            "arguments to GROUPING must be grouping expressions of the associated query level",
                        ),
                        ast::location(n),
                    ));
                }
            }
        }
        if target == cur {
            return Ok(Expr::Grouping(pos));
        }
        for (l, n) in params_before.into_iter().enumerate().skip(target + 1) {
            self.levels[l].params.truncate(n);
        }
        Ok(self.lift(target, Expr::Grouping(pos)))
    }

    /// A CREATE FUNCTION function matching a call: the function and its coerced arguments
    /// (defaults filled in). `Ok(None)`: no user function by that name.
    pub(crate) fn resolve_user_function(
        &mut self,
        schema: Option<&str>,
        name: &str,
        f: &pb::FuncCall,
    ) -> Result<Option<(Arc<crate::catalog::Function>, Vec<Expr>)>> {
        let r = self.resolve_routine(schema, name, &f.args, f.func_variadic, f.location, false)?;
        if let Some((fdef, _)) = &r {
            self.note_func_ref(f.location, fdef.oid);
        }
        Ok(r)
    }

    /// Resolve a call of a user-defined function or procedure, and bind its arguments
    /// (positional, then named; omitted ones from their defaults). A CALL (`call`) matches
    /// procedures on all their parameters, OUT ones included, and returns an argument for
    /// each; otherwise the input parameters' arguments. `None`: no user routine has the name
    /// (a built-in may).
    /// `variadic`: the last argument has VARIADIC (it is the variadic array itself).
    pub(crate) fn resolve_routine(
        &mut self,
        schema: Option<&str>,
        name: &str,
        arg_nodes: &[Node],
        variadic: bool,
        loc: i32,
        call: bool,
    ) -> Result<Option<(Arc<crate::catalog::Function>, Vec<Expr>)>> {
        let schemas: Vec<u32> = match schema {
            Some(s) => self
                .cat
                .schema_by_name(s)
                .map(|s| vec![s.oid])
                .unwrap_or_default(),
            None => self.cat.routine_path(&self.ctx.search_path),
        };
        let mut cands: Vec<Arc<crate::catalog::Function>> = self
            .cat
            .functions
            .values()
            .filter(|fx| fx.agg.is_none() && fx.name == name && schemas.contains(&fx.schema))
            .cloned()
            .collect();
        if cands.is_empty() && !call {
            return Ok(None);
        }
        // (FuncnameGetCandidates walks the path: of the same argument types, the function of
        // the schema searched first; the built-ins are pg_catalog's, at its place.)
        let at = |f: &crate::catalog::Function| schemas.iter().position(|&s| s == f.schema);
        cands.sort_by_key(|f| at(f));
        let pc = schemas.iter().position(|&s| s == crate::catalog::PG_CATALOG_OID);
        let builtin: Vec<Arc<FuncDef>> = match pc {
            Some(pc) if !call && !variadic && schema.is_none_or(|s| s == "pg_catalog") => {
                let same = |f: &crate::catalog::Function, b: &FuncDef| {
                    f.args.len() == b.args.len() && f.args.iter().zip(&b.args).all(|(x, y)| x.1.same_base(y))
                };
                let b: Vec<Arc<FuncDef>> = func::registry()
                    .funcs
                    .get(name)
                    .into_iter()
                    .flatten()
                    .filter(|b| !cands.iter().any(|f| at(f).is_some_and(|i| i < pc) && same(f, b)))
                    .cloned()
                    .collect();
                cands.retain(|f| !(at(f).is_some_and(|i| i > pc) && b.iter().any(|b| same(f, b))));
                if cands.is_empty() {
                    return Ok(None);
                }
                b
            }
            _ => Vec::new(),
        };
        let mut args = Vec::with_capacity(arg_nodes.len());
        let mut arg_names: Vec<Option<String>> = Vec::with_capacity(arg_nodes.len());
        for a in arg_nodes {
            match inner(a) {
                Some(NodeEnum::NamedArgExpr(na)) => {
                    args.push(self.bind_expr(na.arg.as_deref().unwrap())?);
                    arg_names.push(Some(na.name.clone()));
                }
                _ => {
                    args.push(self.bind_expr(a)?);
                    arg_names.push(None);
                }
            }
        }
        check_arg_names(arg_nodes)?;
        if call {
            // (CALL keeps a void parameter, of unknown type: it may stand for an output
            // argument.)
            for a in args.iter_mut() {
                if let Expr::Param(i, DataType::Void) = a {
                    *a = Expr::Param(*i, DataType::Unknown);
                }
            }
        } else {
            drop_void_params(&mut args, &mut arg_names);
        }
        let types: Vec<DataType> = args.iter().map(|a| a.ty()).collect();
        // Overloads told apart only by domain parameters: keep the one whose domains the
        // arguments have (else the base-typed one).
        let cands = {
            let arg_doms: Vec<Option<u32>> =
                args.iter().map(|a| self.declared_domain(a)).collect();
            let score = |c: &crate::catalog::Function| {
                (0..c.args.len())
                    .filter(|&i| c.arg_domain(i) == arg_doms.get(i).copied().flatten())
                    .count()
            };
            let mut kept: Vec<Arc<crate::catalog::Function>> = Vec::new();
            for c in cands {
                match kept
                    .iter()
                    .position(|k| k.args.iter().map(|a| &a.1).eq(c.args.iter().map(|a| &a.1)))
                {
                    Some(i) if score(&c) > score(&kept[i]) => kept[i] = c,
                    Some(_) => {}
                    None => kept.push(c),
                }
            }
            kept
        };
        // `name(type, n => type)`, as error messages show the call.
        let signature = || {
            let parts: Vec<String> = types
                .iter()
                .zip(&arg_names)
                .map(|(t, n)| match n {
                    Some(n) => format!("{n} => {}", t.display()),
                    None => t.display(),
                })
                .collect();
            format!("{name}({})", parts.join(", "))
        };
        // The parameters a call matches against: (name, type, default).
        let params =
            |c: &crate::catalog::Function| -> Vec<(Option<String>, DataType, Option<String>)> {
                let mut k = 0;
                let all = if call {
                    c.all_params()
                } else {
                    c.args
                        .iter()
                        .map(|(n, t)| (n.clone(), t.clone(), 'i'))
                        .collect()
                };
                all.into_iter()
                    .map(|(n, t, m)| {
                        let d = if matches!(m, 'i' | 'b' | 'v') {
                            k += 1;
                            c.defaults.get(k - 1).cloned().flatten()
                        } else {
                            None
                        };
                        (n, t, d)
                    })
                    .collect()
            };
        let mut chosen: Option<(
            Arc<crate::catalog::Function>,
            Vec<Option<Expr>>,
            Vec<DataType>,
        )> = None;
        if arg_names.iter().all(Option::is_none) {
            let defs: Vec<Arc<FuncDef>> = cands
                .iter()
                .map(|c| {
                    let ps = params(c);
                    let ndefaults = ps.iter().rev().take_while(|p| p.2.is_some()).count();
                    // Without VARIADIC in the call, a variadic function takes its trailing
                    // arguments as the array's elements (at least one; else the default).
                    let expand = c.is_variadic() && !variadic && types.len() >= ps.len();
                    Arc::new(FuncDef {
                        oid: c.oid,
                        name: "",
                        args: ps.iter().map(|p| p.1.clone()).collect(),
                        variadic: expand,
                        spread: false,
                        ret: func::Ret::Fixed(c.ret.clone()),
                        strict: c.strict,
                        volatility: c.volatility,
                        imp: |_, _| Err(crate::error::internal("inlined function")),
                        min_args: if expand {
                            ps.len()
                        } else {
                            ps.len() - ndefaults
                        },
                        srf: None,
                        out_cols: Vec::new(),
                        arg_names: Vec::new(),
                        defaults: vec![],
                        coll: None,
                    })
                })
                .collect();
            let mut defs = defs;
            let nuser = defs.len();
            defs.extend(builtin.iter().cloned());
            match select_candidate(&defs, &types, false) {
                // (A built-in of pg_catalog's matches best: it's the caller's to bind.)
                Ok(Some(r)) if nuser < defs.len() && !r.def.name.is_empty() => return Ok(None),
                Ok(Some(r)) => {
                    let fdef = cands.iter().find(|c| c.oid == r.def.oid).cloned().unwrap();
                    let n = params(&fdef).len();
                    let mut arg_types = r.arg_types;
                    if r.def.variadic {
                        // The trailing arguments make up the variadic array.
                        let fixed = n - 1;
                        let elem = arg_types[fixed..]
                            .first()
                            .cloned()
                            .unwrap_or(DataType::Text);
                        let items = args
                            .split_off(fixed)
                            .into_iter()
                            .map(|a| self.coerce_implicit(a, &elem))
                            .collect::<Result<Vec<_>>>()?;
                        let ty = DataType::array_of(elem);
                        args.push(Expr::Array(items, ty.clone()));
                        arg_types.truncate(fixed);
                        arg_types.push(ty);
                    }
                    let mut slots: Vec<Option<Expr>> = args.into_iter().map(Some).collect();
                    slots.resize(n, None);
                    chosen = Some((fdef, slots, arg_types));
                }
                Ok(None) => {}
                Err(_) => {
                    let what = if call { "procedure" } else { "function" };
                    return Err(with_pos(
                        Error::new(code::AMBIGUOUS_FUNCTION, format!("{what} {} is not unique", signature()))
                            .hint(format!("Could not choose a best candidate {what}. You might need to add explicit type casts.")),
                        loc,
                    ));
                }
            }
        } else {
            // Named arguments: place them for each candidate; keep those every argument fits.
            let mut fits = Vec::new();
            'cand: for c in &cands {
                // Named arguments can't be spread into a variadic array.
                if c.is_variadic() && !variadic {
                    continue;
                }
                let ps = params(c);
                let mut slots: Vec<Option<Expr>> = vec![None; ps.len()];
                for (i, (a, n)) in args.iter().zip(&arg_names).enumerate() {
                    let at = match n {
                        Some(n) => match ps.iter().position(|p| p.0.as_deref() == Some(n.as_str()))
                        {
                            Some(at) => at,
                            None => continue 'cand,
                        },
                        None => i,
                    };
                    if at >= slots.len()
                        || slots[at].is_some()
                        || !func::can_coerce(&a.ty(), &ps[at].1)
                    {
                        continue 'cand;
                    }
                    slots[at] = Some(a.clone());
                }
                if slots
                    .iter()
                    .zip(&ps)
                    .any(|(s, p)| s.is_none() && p.2.is_none())
                {
                    continue;
                }
                let types = ps.iter().map(|p| p.1.clone()).collect();
                fits.push((c.clone(), slots, types));
            }
            if fits.len() > 1 {
                let what = if call { "procedure" } else { "function" };
                return Err(with_pos(
                    Error::new(code::AMBIGUOUS_FUNCTION, format!("{what} {} is not unique", signature()))
                        .hint(format!("Could not choose a best candidate {what}. You might need to add explicit type casts.")),
                    loc,
                ));
            }
            chosen = fits.pop();
        }
        let Some((fdef, slots, arg_types)) = chosen else {
            // A built-in function (or aggregate) of these arguments: not a procedure.
            if call
                && schema.is_none_or(|s| s == "pg_catalog")
                && arg_names.iter().all(Option::is_none)
                && (func::resolve_function(name, &types).is_ok()
                    || matches!(
                        select_candidate(&crate::exec::agg::agg_registry().sigs(name), &types, false),
                        Ok(Some(_))
                    ))
            {
                return Err(with_pos(
                    Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("{} is not a procedure", signature()),
                    )
                    .hint("To call a function, use SELECT."),
                    loc,
                ));
            }
            if call {
                return Err(with_pos(
                    Error::new(code::UNDEFINED_FUNCTION, format!("procedure {} does not exist", signature()))
                        .hint("No procedure matches the given name and argument types. You might need to add explicit type casts."),
                    loc,
                ));
            }
            if func::function_exists(name) {
                return Ok(None);
            }
            return Err(with_pos(
                Error::new(code::UNDEFINED_FUNCTION, format!("function {} does not exist", signature()))
                    .hint("No function matches the given name and argument types. You might need to add explicit type casts."),
                loc,
            ));
        };
        if call && !fdef.procedure {
            return Err(with_pos(
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("{} is not a procedure", signature()),
                )
                .hint("To call a function, use SELECT."),
                loc,
            ));
        }
        if !call && fdef.procedure {
            return Err(with_pos(
                Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("{} is a procedure", signature()),
                )
                .hint("To call a procedure, use CALL."),
                loc,
            ));
        }
        self.require_function(fdef.oid);
        if fdef.language != "sql"
            && fdef.language != "plpgsql"
            && fdef.language != "internal"
            && !(fdef.language == "c" && Self::native_def(&fdef).is_some())
        {
            return Err(with_pos(
                crate::error::unsupported(format!("functions in language {}", fdef.language)),
                loc,
            ));
        }
        let ps = params(&fdef);
        // Each parameter's domain (a CALL lists output parameters too, which have none here).
        let domains: Vec<Option<u32>> = if call {
            let mut k = 0;
            fdef.all_params()
                .iter()
                .map(|(_, _, m)| {
                    matches!(m, 'i' | 'b' | 'v').then(|| {
                        k += 1;
                        fdef.arg_domain(k - 1)
                    })?
                })
                .collect()
        } else {
            (0..ps.len()).map(|i| fdef.arg_domain(i)).collect()
        };
        let mut out = Vec::with_capacity(ps.len());
        // The arguments' own types (unknown literals don't resolve polymorphic types).
        let mut written = Vec::with_capacity(ps.len());
        for (i, (slot, p)) in slots.into_iter().zip(&ps).enumerate() {
            let e = match slot {
                Some(e) => e,
                None => {
                    // Omitted: the parameter's default.
                    let Some(d) = &p.2 else { break };
                    let n = super::ast::parse_expr(d)?;
                    let e = self.bind_expr(&n)?;
                    // CREATE FUNCTION stored it coerced to the parameter's type (assignment
                    // rules: `int DEFAULT 1.0`).
                    let to = p.1.clone();
                    let mismatch = |_: &DataType| Error::new(code::CANNOT_COERCE, "");
                    self.coerce_with(e.clone(), &to, CastContext::Assignment, &mismatch)
                        .unwrap_or(e)
                }
            };
            written.push(e.ty());
            let t = arg_types.get(i).unwrap_or(&p.1).clone();
            // Where a positional argument was written, for coercion errors.
            let at = if arg_names.iter().all(Option::is_none) {
                arg_nodes.get(i).map_or(-1, super::ast::location)
            } else {
                -1
            };
            // Only a ROW constructor makes a record into a composite (`coerce_record_to_complex`).
            if matches!(e.ty(), DataType::Row(r) if r.oid == 0)
                && t.row_oid().is_some()
                && !matches!(e, Expr::Row(..))
            {
                return Err(with_pos(
                    Error::new(
                        code::CANNOT_COERCE,
                        format!("cannot cast type record to {}", t.display()),
                    ),
                    at,
                ));
            }
            let e = self.coerce_implicit(e, &t).map_err(|err| {
                if err.position.is_none() {
                    with_pos(err, at)
                } else {
                    err
                }
            })?;
            // A domain parameter checks its argument (unless it already is of the domain).
            let e = match domains.get(i).copied().flatten() {
                Some(d) if !matches!(&e, Expr::CoerceToDomain(_, x) if x.oid == d) => {
                    self.domain_check(e, d)?
                }
                _ => e,
            };
            out.push(e);
        }
        Ok(Some((resolve_polymorphic(fdef, &ps, &out, &written)?, out)))
    }

    /// The input collation (`fn_input_collation`) of a call with `args`: what its collatable
    /// arguments derive; None for the default, or when implicit collations conflict.
    pub(crate) fn call_collation(&self, args: &[Expr]) -> Result<Option<String>> {
        let mut merged = super::Coll::None;
        for a in args.iter().filter(|a| a.ty().collatable()) {
            merged = merged.merge(self.collation_of(a)?)?;
        }
        Ok(match merged {
            super::Coll::Implicit(c) | super::Coll::Explicit(c, _) if c != "default" => Some(c),
            _ => None,
        })
    }

    /// Bind a SQL function's body with `args` as its parameters: it sees them as $1.. and by
    /// name, and nothing else from the calling query. The plan reads the arguments as
    /// Outer(1, i).
    pub(crate) fn bind_user_body(
        &mut self,
        fdef: &crate::catalog::Function,
        args: &[Expr],
    ) -> Result<(Plan, Vec<OutCol>)> {
        // PL/pgSQL, and SQL bodies of several statements, run when called.
        // So do functions with SET clauses, which hold while they run.
        let by_statement = std::mem::take(&mut self.sql_fn_statement);
        let inline = std::mem::take(&mut self.inline_body);
        if fdef.language == "plpgsql"
            || !fdef.config.is_empty()
            || (fdef.language == "sql" && crate::sql::parse::body_runs_by_statement(fdef))
            || (!by_statement && fdef.language == "sql" && self.body_writes_ruled_table(fdef))
            || (fdef.language == "sql" && !inline && self.bodies.contains(&fdef.oid))
        {
            // Interpreted at run time; the arguments arrive as the outer row.
            let cols: Vec<OutCol> = if !fdef.out_cols.is_empty() {
                fdef.out_cols
                    .iter()
                    .enumerate()
                    // (An unnamed OUT parameter's column: `columnN`.)
                    .map(|(i, (n, t))| OutCol::new(out_col_name(n, i), t.clone()))
                    .collect()
            } else if let DataType::Row(fields) = &fdef.ret {
                fields
                    .iter()
                    .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                    .collect()
            } else {
                vec![OutCol::new(fdef.name.clone(), fdef.ret.clone())]
            };
            let outer = args
                .iter()
                .enumerate()
                .map(|(i, a)| Expr::Outer(1, i, a.ty()))
                .collect();
            // (Explicit collations that disagree failed before the body: `call_collation`.)
            let coll = self.call_collation(args).unwrap_or(None);
            let plan = Plan::PlCall {
                func: Arc::new(fdef.clone()),
                args: outer,
                ncols: cols.len(),
                coll,
                scan: false,
            };
            return Ok((plan, cols));
        }
        let parsed = crate::sql::parse::parse_body(fdef);
        for w in crate::sql::scs::take_warnings() {
            self.ctx.notice(w);
        }
        let Some(body) = parsed?.into_iter().next().and_then(|s| s.stmt).map(|b| *b) else {
            // An empty body isn't inlined; starting it finds no final statement.
            let mut e = check_sql_retval(&self.cat, &self.ctx.search_path, fdef, None)
                .err()
                .unwrap_or_else(|| crate::error::internal("empty query"));
            e.context = Some(format!("SQL function \"{}\" during startup", fdef.name).into());
            e.stacked = true;
            return Err(e);
        };
        if let Some(NodeEnum::TransactionStmt(t)) = inner(&body) {
            let tag = crate::session::ddl::transaction_tag(t);
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("{tag} is not allowed in an SQL function"),
            ));
        }
        let mut lvl = super::Level::new();
        lvl.barrier = true;
        lvl.fn_name = fdef.name.clone();
        lvl.params = args.to_vec();
        lvl.inline_args = std::mem::take(&mut self.fold_args);
        lvl.fn_args = Some(
            fdef.args
                .iter()
                .zip(args)
                .map(|((n, _), a)| (n.clone(), a.ty()))
                .collect(),
        );
        self.levels.push(lvl);
        self.fn_levels.push(self.levels.len() - 1);
        let saved_ctes = std::mem::take(&mut self.ctes);
        // A SECURITY DEFINER body reads tables with its owner's privileges.
        let (saved_as, saved_definer) = (self.check_as, self.definer);
        if fdef.security_definer {
            self.check_as = Some(fdef.owner);
            self.definer = Some(fdef.owner);
        }
        // Positions are in the body: `in_sql_fn` makes them the internal query's. A statement
        // run on its own that writes a relation with rules is rewritten by them, as a
        // statement is.
        // inline_function's active_fns: the bodies being inlined into this query; a body
        // that isn't inlined is planned as a query of its own.
        self.bodies.push(fdef.oid);
        let outer_inlining = if inline {
            self.inlining.push(fdef.oid);
            None
        } else {
            Some(std::mem::take(&mut self.inlining))
        };
        let r = if by_statement && self.body_writes_ruled_table(fdef) {
            self.bind_statement(&body)
                .map(|bq| ((*bq.plan).clone(), bq.cols))
        } else {
            self.bind_select_node(&body)
        };
        match outer_inlining {
            Some(outer) => self.inlining = outer,
            None => {
                self.inlining.pop();
            }
        }
        self.bodies.pop();
        (self.check_as, self.definer) = (saved_as, saved_definer);
        self.ctes = saved_ctes;
        self.fn_levels.pop();
        self.levels.pop();
        r
    }

    /// A SQL body that is one INSERT / UPDATE / DELETE of a relation with rules: it runs as
    /// a statement of its own (the rules' queries with it), as Postgres's SQL functions do.
    fn body_writes_ruled_table(&self, fdef: &crate::catalog::Function) -> bool {
        if self.cat.rules.is_empty() || self.deparse {
            return false;
        }
        let Ok(stmts) = crate::sql::parse::parse_body_quiet(fdef) else {
            return false;
        };
        let [one] = &stmts[..] else {
            return false;
        };
        let rv = match one.stmt.as_deref().and_then(|n| n.node.as_ref()) {
            Some(NodeEnum::InsertStmt(s)) => s.relation.as_ref(),
            Some(NodeEnum::UpdateStmt(s)) => s.relation.as_ref(),
            Some(NodeEnum::DeleteStmt(s)) => s.relation.as_ref(),
            _ => None,
        };
        let Some(rv) = rv else {
            return false;
        };
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        let Ok(Some(rel)) = self.cat.lookup(schema, &rv.relname, &self.ctx.search_path) else {
            return false;
        };
        let oid = rel.oid();
        self.cat
            .rules
            .values()
            .any(|r| r.table == oid && r.name != "_RETURN")
    }

    /// Bind inside CASE / COALESCE, where set-returning functions aren't allowed.
    fn no_srfs_in<T>(
        &mut self,
        what: &'static str,
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let saved = self.level().st.srf_banned.replace(what);
        let r = f(self);
        self.level().st.srf_banned = saved;
        r
    }

    /// check_srf_call_placement: where a set-returning function may be called.
    fn check_srf_placement(&self, loc: i32) -> Result<()> {
        let st = &self.levels.last().unwrap().st;
        let hint = "You might be able to move the set-returning function into a LATERAL FROM item.";
        if st.in_window {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "window function calls cannot contain set-returning function calls",
                )
                .hint(hint),
                loc,
            ));
        }
        if let Some(what) = st.srf_banned {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("set-returning functions are not allowed in {what}"),
                )
                .hint(hint),
                loc,
            ));
        }
        if self.levels.last().unwrap().st.in_agg {
            return Err(with_pos(
                Error::new(code::FEATURE_NOT_SUPPORTED, "aggregate function calls cannot contain set-returning function calls")
                    .hint("You might be able to move the set-returning function into a LATERAL FROM item."),
                loc,
            ));
        }
        let clause = self.clause();
        // (A FROM function's own call doesn't come here: one here is in its arguments.)
        if clause == Clause::FromFunction {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "set-returning functions must appear at top level of FROM",
                ),
                loc,
            ));
        }
        if !clause.srfs_ok() {
            return Err(with_pos(not_allowed("srf", clause), loc));
        }
        Ok(())
    }

    /// A set-returning CREATE FUNCTION function in the select list: a ProjectSet call that
    /// runs its body once per input row.
    fn user_srf_call(
        &mut self,
        fdef: &crate::catalog::Function,
        args: Vec<Expr>,
        loc: i32,
    ) -> Result<Expr> {
        self.check_srf_placement(loc)?;
        if fdef.language == "plpgsql" {
            self.call_collation(&args)?;
        }
        let at = self.sql_fn_at(fdef, &args, FnPlace::Run);
        let (body, cols) = self.in_sql_fn(fdef, at, |b| {
            let (body, bcols) = b.bind_user_body(fdef, &args)?;
            if fdef.language == "sql" {
                check_sql_retval(&b.cat, &b.ctx.search_path, fdef, Some(&bcols))?;
            }
            let cols: Vec<OutCol> =
                if !fdef.out_cols.is_empty() && fdef.out_cols.len() == bcols.len() {
                    fdef.out_cols
                        .iter()
                        .enumerate()
                        .map(|(i, (n, t))| OutCol::new(out_col_name(n, i), t.clone()))
                        .collect()
                } else if let DataType::Row(r) = &fdef.ret
                    && r.oid != 0
                    && r.fields.len() == bcols.len()
                    && bcols.len() > 1
                {
                    // The declared composite's columns (and typmods) are the result's.
                    r.fields
                        .iter()
                        .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                        .collect()
                } else if matches!(fdef.ret, DataType::Record | DataType::Row(_)) {
                    bcols.clone()
                } else {
                    vec![OutCol::new(fdef.name.clone(), fdef.ret.clone())]
                };
            if cols.len() != bcols.len() {
                return Err(Error::new(
                    code::INVALID_FUNCTION_DEFINITION,
                    format!(
                        "return type mismatch in function declared to return {}",
                        fdef.ret.display()
                    ),
                )
                .detail("Final statement returns too many columns."));
            }
            let mut exprs = Vec::new();
            for (i, (c, bc)) in cols.iter().zip(&bcols).enumerate() {
                let e = b.coerce_assign(Expr::Col(i, bc.ty.clone()), &c.ty, &c.name)?;
                exprs.push(b.result_domain(fdef, i, cols.len(), e)?);
            }
            Ok((
                Plan::Project {
                    input: Arc::new(body),
                    exprs,
                },
                cols,
            ))
        })?;
        let body = self.sql_fn_plan(fdef, at, body);
        let ncols = cols.len();
        let ty = match &fdef.ret {
            // A named composite stays that type (a field of it, or an argument for it).
            DataType::Row(r) if ncols > 1 && r.oid != 0 && r.fields.len() == ncols => {
                fdef.ret.clone()
            }
            _ if ncols > 1 => DataType::row(
                cols.iter()
                    .map(|c| (c.name.clone(), c.ty.clone()))
                    .collect(),
            ),
            _ => cols[0].ty.clone(),
        };
        let def = func::display_def(fdef);
        let i = self.push_srf(SrfCall {
            func: def,
            args,
            ty: ty.clone(),
            ncols,
            body: Some(Arc::new(body)),
        });
        Ok(Expr::Srf(i, ty))
    }

    /// Collect a set-returning call of this level; one the GROUP BY has already is that one
    /// (grouping on it, the select list's call is the grouped value).
    fn push_srf(&mut self, call: SrfCall) -> usize {
        let in_group = self.clause() == Clause::GroupBy;
        let st = &mut self.level().st;
        let known = if in_group { st.srfs.len() } else { st.group_srfs.min(st.srfs.len()) };
        let same = |s: &SrfCall| {
            s.func.oid == call.func.oid
                && s.func.name == call.func.name
                && s.body.is_none() == call.body.is_none()
                && s.args.len() == call.args.len()
                && s.args.iter().zip(&call.args).all(|(a, b)| a.same_as(b))
        };
        if let Some(i) = st.srfs[..known].iter().position(same) {
            return i;
        }
        // (The select list's equal calls are one (split_pathtarget_at_srfs).)
        if !in_group
            && let Some(i) = st.srfs[known..].iter().position(same)
        {
            return known + i;
        }
        st.srfs.push(call);
        st.srfs.len() - 1
    }

    /// SQL-language functions defined with CREATE FUNCTION: inlined as a scalar subquery
    /// over their arguments.
    fn try_user_function(
        &mut self,
        schema: Option<&str>,
        name: &str,
        f: &pb::FuncCall,
    ) -> Result<Option<Expr>> {
        let Some((fdef, coerced)) = self.resolve_user_function(schema, name, f)? else {
            return Ok(None);
        };
        self.user_fn_loc.get_or_insert(f.location);
        // Postgres's tree keeps a call in named notation as written (defaults not filled
        // in): the arguments in their order, the named ones by name.
        if self.deparse
            && !f.func_variadic
            && f.args.iter().any(|a| matches!(inner(a), Some(NodeEnum::NamedArgExpr(_))))
        {
            let mut args = Vec::new();
            let mut arg_names = Vec::new();
            for (i, a) in f.args.iter().enumerate() {
                let (pos, n) = match inner(a) {
                    Some(NodeEnum::NamedArgExpr(na)) => (
                        fdef.args.iter().position(|(n, _)| n.as_deref() == Some(na.name.as_str())),
                        na.name.as_str(),
                    ),
                    _ => (Some(i), ""),
                };
                let Some(e) = pos.and_then(|p| coerced.get(p)) else {
                    return self.user_call(fdef, coerced, f.location).map(Some);
                };
                args.push(e.clone());
                arg_names.push(func::intern(n));
            }
            let mut def = (*func::display_def(&fdef)).clone();
            def.arg_names = arg_names;
            return Ok(Some(Expr::Call(Arc::new(def), args, fdef.ret.clone())));
        }
        self.user_call(fdef, coerced, f.location).map(Some)
    }

    /// A C function's call target: the native code for its library and symbol
    /// (`crate::ext::native`), over its declared argument and result types.
    pub(crate) fn native_def(fdef: &crate::catalog::Function) -> Option<FuncDef> {
        let bin = fdef.probin.as_deref()?;
        // (A SECURITY DEFINER C function runs as its owner: a library may have code for
        // that case, `symbol#secdef`.)
        let n = fdef
            .security_definer
            .then(|| crate::ext::native::lookup(bin, &format!("{}#secdef", fdef.body)))
            .flatten()
            .or_else(|| crate::ext::native::lookup(bin, &fdef.body))?;
        let args: Vec<DataType> = fdef.args.iter().map(|a| a.1.clone()).collect();
        Some(FuncDef {
            oid: fdef.oid,
            name: func::intern(&fdef.name),
            min_args: args.len(),
            args,
            variadic: false,
            spread: false,
            ret: func::Ret::Fixed(fdef.ret.clone()),
            strict: fdef.strict,
            volatility: fdef.volatility,
            imp: n.imp,
            srf: n.srf,
            out_cols: fdef
                .out_cols
                .iter()
                .map(|(name, t)| (func::intern(name), t.clone()))
                .collect(),
            arg_names: vec![],
            defaults: vec![],
            coll: None,
        })
    }

    /// A C function's library is loaded once called (fmgr's load_external_function): its
    /// `_PG_init` defines its parameters for the session.
    pub(crate) fn load_library(&self, fdef: &crate::catalog::Function) {
        if let Some(bin) = &fdef.probin {
            let w = self.ctx.sess.settings.borrow_mut().load_module(crate::ext::native::library(bin));
            for e in w {
                self.ctx.notice(e);
            }
        }
    }

    /// A call of a C function: its native code (a set-returning one as the select list's
    /// ProjectSet runs it).
    fn native_call(
        &mut self,
        fdef: &crate::catalog::Function,
        coerced: Vec<Expr>,
        loc: i32,
    ) -> Result<Expr> {
        self.load_library(fdef);
        let Some(def) = Self::native_def(fdef) else {
            return Err(with_pos(
                crate::error::unsupported(format!("functions in language {}", fdef.language)),
                loc,
            ));
        };
        let def = self.input_collation_def(Arc::new(def), &coerced);
        let ret = fdef.ret.clone();
        if fdef.returns_set {
            self.check_srf_placement(loc)?;
            let ty = if fdef.out_cols.len() > 1 {
                DataType::row(fdef.out_cols.clone())
            } else {
                ret
            };
            // (OUT columns: one record of them per row.)
            let ncols = fdef.out_cols.len().max(1);
            let i = self.push_srf(SrfCall {
                func: def,
                args: coerced,
                ty: ty.clone(),
                ncols,
                body: None,
            });
            return Ok(Expr::Srf(i, ty));
        }
        Ok(Expr::Call(def, coerced, ret))
    }

    /// A call of an internal-language function (`AS 'int4eq'`): the built-in function its
    /// body names, over the declared argument and result types.
    fn internal_call(
        &mut self,
        fdef: &crate::catalog::Function,
        coerced: Vec<Expr>,
        loc: i32,
    ) -> Result<Expr> {
        let builtin = crate::catalog::system::builtin_by_prosrc(&fdef.body);
        // A set-returning one (`AS 'array_unnest'`): the built-in set-returning function, its
        // rows typed as declared.
        if fdef.returns_set
            && let Some(def) = builtin.and_then(|b| {
                func::registry().funcs.values().flatten().find(|d| d.oid == b && d.srf.is_some()).cloned()
            })
        {
            self.check_srf_placement(loc)?;
            let ty = if fdef.out_cols.len() > 1 { DataType::row(fdef.out_cols.clone()) } else { fdef.ret.clone() };
            let i = self.push_srf(SrfCall {
                func: def,
                args: coerced,
                ty: ty.clone(),
                ncols: fdef.out_cols.len().max(1),
                body: None,
            });
            return Ok(Expr::Srf(i, ty));
        }
        let mut def = builtin.and_then(|b| {
            func::registry()
                .funcs
                .values()
                .flatten()
                .find(|d| d.oid == b && d.srf.is_none())
                .cloned()
        });
        // A built-in the engine runs under another oid (or as a cast): found by its pg_proc
        // name and argument types (`rtrim1` is rtrim(text), `booltext` text(boolean)).
        if def.is_none()
            && let Some(sig) = builtin.and_then(|b| crate::catalog::system::builtin_proc_sigs().iter().find(|s| s.oid == b))
        {
            let args: Option<Vec<DataType>> = sig.args.iter().map(|a| DataType::from_oid(*a)).collect();
            if let Some(args) = args {
                if let Ok(r) = func::resolve_function(sig.name, &args) {
                    def = Some(r.def);
                } else if sig.name == "text" && sig.ret == crate::types::oid::TEXT && args.len() == 1 {
                    // text(x): the cast to text.
                    fn to_text(a: &[Value], c: &func::FnCtx) -> Result<Value> {
                        crate::types::cast::cast_value(a[0].clone(), &c.arg_type(0), &DataType::Text, c.exec.fmt(), true)
                    }
                    let mut d = (*func::display_def(fdef)).clone();
                    d.name = "text";
                    d.imp = to_text;
                    def = Some(Arc::new(d));
                }
            }
        }
        let Some(def) = def else {
            return Err(with_pos(
                crate::error::unsupported(format!(
                    "calling internal function {} from SQL",
                    fdef.body
                )),
                loc,
            ));
        };
        let mut d = (*def).clone();
        let void = matches!(d.ret, func::Ret::Fixed(DataType::Void));
        // (A parameter of an extension's native type keeps the built-in's own type: the call
        // passes the built-in value, `crate::ext::types::wants_builtin`.)
        d.args = fdef
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| match (&a.1, def.args.get(i)) {
                (DataType::Base(b), Some(own)) if b.ext != 0 => own.clone(),
                _ => a.1.clone(),
            })
            .collect();
        d.ret = func::Ret::Fixed(fdef.ret.clone());
        d.variadic = false;
        d.spread = false;
        d.strict = fdef.strict;
        d.volatility = fdef.volatility;
        // Its parallel safety is the declared one (max_parallel_hazard reads the called
        // function's proparallel): a PARALLEL SAFE alias of a restricted built-in runs in a
        // worker, its locks a worker's (deadlock-parallel's lock_share / lock_excl).
        if crate::sql::paths::parallel::func_parallel(&self.cat, d.oid) != fdef.parallel {
            d.oid = fdef.oid;
        }
        d.min_args = d.args.len();
        // A void built-in declared to return an integer gives its (Datum) 0 as that.
        let zero = match &fdef.ret {
            DataType::Int2 if void => Some(Value::Int2(0)),
            DataType::Int4 if void => Some(Value::Int4(0)),
            DataType::Int8 if void => Some(Value::Int8(0)),
            _ => None,
        };
        let call = Expr::Call(Arc::new(d), coerced.clone(), fdef.ret.clone());
        Ok(match zero {
            Some(z) => {
                let value = Expr::Coalesce(vec![call, Expr::Const(z, fdef.ret.clone())], fdef.ret.clone());
                if fdef.strict && !coerced.is_empty() {
                    let any_null = Expr::Or(
                        coerced.into_iter().map(|a| Expr::IsNull(Box::new(a), false)).collect(),
                    );
                    Expr::Case {
                        operand: None,
                        whens: vec![(any_null, Expr::Const(Value::Null, fdef.ret.clone()))],
                        else_: Some(Box::new(value)),
                        ty: fdef.ret.clone(),
                    }
                } else {
                    value
                }
            }
            None => call,
        })
    }

    /// A call of a user function with bound arguments (already coerced).
    pub(crate) fn user_call(
        &mut self,
        fdef: Arc<crate::catalog::Function>,
        coerced: Vec<Expr>,
        loc: i32,
    ) -> Result<Expr> {
        if fdef.language == "c" && !self.deparse {
            return self.native_call(&fdef, coerced, loc);
        }
        if fdef.language == "internal" && !self.deparse {
            // It runs the built-in, but plans show the function as called.
            return Ok(match self.internal_call(&fdef, coerced, loc)? {
                Expr::Call(d, a, t) if d.name != fdef.name => Expr::Call(
                    Arc::new(FuncDef {
                        name: func::intern(&fdef.name),
                        ..(*d).clone()
                    }),
                    a,
                    t,
                ),
                e => e,
            });
        }
        if self.deparse {
            return Ok(Expr::Call(
                func::display_def(&fdef),
                coerced,
                fdef.ret.clone(),
            ));
        }
        if fdef.returns_set {
            return self.user_srf_call(&fdef, coerced, loc);
        }
        let ret = fdef.ret.clone();
        let id = self.fresh_id();
        if fdef.language == "plpgsql" {
            self.call_collation(&coerced)?;
        }
        let at = self.sql_fn_at(&fdef, &coerced, FnPlace::Scalar);
        let hidden_rtes = self.hidden_rtes;
        let bound = self.in_sql_fn(&fdef, at, |b| {
            let (plan, cols) = b.bind_user_body(&fdef, &coerced)?;
            if fdef.language == "sql" {
                check_sql_retval(&b.cat, &b.ctx.search_path, &fdef, Some(&cols))?;
            }
            let plan = if matches!(ret, DataType::Void) {
                // A SQL function's void result is NULL; PL/pgSQL's is an empty value.
                let v = if fdef.language == "sql" {
                    Value::Null
                } else {
                    Value::Text(Arc::from(""))
                };
                // (A body computing a void value itself: that value, as inlining keeps it.)
                let e = if fdef.language == "sql" && cols.len() == 1 && matches!(cols[0].ty, DataType::Void) {
                    Expr::Col(0, DataType::Void)
                } else {
                    Expr::Const(v, DataType::Void)
                };
                Plan::Project {
                    input: Arc::new(plan),
                    exprs: vec![e],
                }
            } else if cols.len() > 1 {
                // Composite result in a scalar position: one record value.
                let outs = fdef.out_cols.len() == cols.len();
                let fields: Vec<(String, DataType)> = if outs {
                    out_fields(&fdef)
                } else {
                    cols.iter()
                        .map(|c| (c.name.clone(), c.ty.clone()))
                        .collect()
                };
                // A declared composite's columns take its types (and typmods).
                let declared = match &ret {
                    DataType::Row(r) if !outs && r.oid != 0 && r.fields.len() == cols.len() => {
                        Some(r.fields.clone())
                    }
                    _ => None,
                };
                let mut items = Vec::with_capacity(cols.len());
                for (i, (c, (name, t))) in cols.iter().zip(&fields).enumerate() {
                    let e = Expr::Col(i, c.ty.clone());
                    items.push(match &declared {
                        Some(f) => b.coerce_assign(e, &f[i].1, &f[i].0)?,
                        None if outs => b.coerce_assign(e, t, name)?,
                        None => e,
                    });
                }
                // Of the declared composite type; anonymous for OUT parameters / RECORD
                // (named by the OUT parameters).
                let ty = match &ret {
                    DataType::Row(r) if r.oid != 0 && r.fields.len() == fields.len() => ret.clone(),
                    _ => DataType::row(fields),
                };
                let row = Expr::Row(items, ty);
                Plan::Project {
                    input: Arc::new(Plan::Limit {
                        input: Arc::new(plan),
                        limit: Some(Expr::Const(Value::Int8(1), DataType::Int8)),
                        offset: None,
                        with_ties: None,
                    }),
                    exprs: vec![row],
                }
            } else {
                let col = cols.first().map(|c| c.ty.clone()).unwrap_or(DataType::Void);
                let e = Expr::Col(0, col.clone());
                let mut e = match &ret {
                    // One column for a one-field composite result: that field
                    // (check_sql_fn_retval).
                    DataType::Row(r)
                        if r.fields.len() == 1
                            && !col.same_base(&ret)
                            && !matches!(col, DataType::Row(_) | DataType::Record) =>
                    {
                        let (name, fty) = &r.fields[0];
                        let f = b.coerce_assign(e, fty, name)?;
                        Expr::Row(vec![f], ret.clone())
                    }
                    _ => b.coerce_assign(e, &ret, "result")?,
                };
                if let Some(d) = fdef.ret_domain {
                    e = b.domain_check(e, d)?;
                }
                Plan::Project {
                    input: Arc::new(Plan::Limit {
                        input: Arc::new(plan),
                        limit: Some(Expr::Const(Value::Int8(1), DataType::Int8)),
                        offset: None,
                        with_ties: None,
                    }),
                    exprs: vec![e],
                }
            };
            Ok((plan, cols))
        });
        let (plan, cols) = match bound {
            Ok(x) => x,
            // Postgres reads a SQL function's body only when it plans the call (inlining)
            // or runs it, never for a query it only stores (a view, a rule, CREATE TABLE AS
            // ... WITH NO DATA): a body that doesn't bind here (it names a table not on this
            // search_path, as in a pg_dump restore) leaves the call as it is.
            Err(_) if self.unplanned && fdef.language == "sql" => {
                return Ok(Expr::Call(func::display_def(&fdef), coerced, ret));
            }
            Err(e) => return Err(e),
        };
        // inline_function: the body's expression in place of the call, the arguments in
        // place of its parameters (so a CASE evaluates only the branch it takes).
        // (Not with an argument that is a CASE's test value, which a CASE in the body
        // would take for its own: contain_context_dependent_node.)
        let context_dependent = coerced.iter().any(|a| {
            let mut f = false;
            a.walk(&mut |x| f |= matches!(x, Expr::CaseTest(_)));
            f
        });
        // (A void function's body must return void itself: inline_function's exprType check.)
        let void_mismatch = matches!(ret, DataType::Void)
            && !(cols.len() == 1 && matches!(cols[0].ty, DataType::Void));
        if fdef.language == "sql"
            && !self.deparse
            && !context_dependent
            && !void_mismatch
            && at.is_some_and(|c| matches!(c.at, SqlFnAt::Inlining) && !c.fold)
            && let Some(e) = inline_scalar(&plan, &coerced)
        {
            return Ok(e);
        }
        // A call that isn't inlined plans its body on its own: the body's relations aren't
        // the statement's (EXPLAIN qualifies columns by the statement's alone).
        self.hidden_rtes = hidden_rtes;
        let plan = Arc::new(self.sql_fn_plan(&fdef, at, plan));
        let nargs = coerced.len();
        let mut params = coerced;
        let args_null: Vec<Expr> = params
            .iter()
            .map(|p| Expr::IsNull(Box::new(p.clone()), false))
            .collect();
        if params.is_empty() && fdef.volatility == 'v' {
            // A volatile call runs every time, not once per statement like an uncorrelated subquery.
            params.push(Expr::Const(Value::Null, DataType::Int4));
        }
        let named = matches!(&ret, DataType::Row(r) if r.oid != 0 && r.fields.len() == cols.len());
        let ret = if cols.len() > 1 && !matches!(ret, DataType::Void) && !named {
            if fdef.out_cols.len() == cols.len() {
                DataType::row(out_fields(&fdef))
            } else {
                DataType::row(
                    cols.iter()
                        .map(|c| (c.name.clone(), c.ty.clone()))
                        .collect(),
                )
            }
        } else {
            ret
        };
        let sub = Expr::Subquery(Box::new(Subquery {
            id,
            kind: SubqueryKind::Scalar,
            plan,
            params,
            left: vec![],
            ops: vec![],
            ty: ret.clone(),
            func: Some(fdef.volatility),
            call: Some(Arc::new(crate::sql::expr::FnShow {
                def: func::display_def(&fdef),
                nargs,
                cost: fdef.procost() as f64,
                pgss: (fdef.language == "sql" && self.ctx.sess.cluster.statements().is_some()).then(|| {
                    let vars = fdef
                        .args
                        .iter()
                        .enumerate()
                        .filter_map(|(i, (n, _))| Some((n.clone()?, i as i32 + 1)))
                        .collect();
                    (Arc::from(fdef.body.trim().trim_end_matches(';').trim_end()), vars)
                }),
                inline: !void_mismatch
                    && at.is_some_and(|c| c.fold || matches!(c.at, SqlFnAt::Inlining)),
            })),
            test_op: None,
            names: Default::default(),
            qual: self.binding_qual(),
            tlist: self.binding_tlist(),
        }));
        // evaluate_function: an immutable call over constants is its value (a composite one
        // a constant of its type). (One that fails runs, and fails, when executed.)
        // (Only a call the planner runs, not one it inlines over the constants; and not of
        // a domain, whose constant would lose it.)
        if at.is_some_and(|c| c.fold && matches!(c.at, SqlFnAt::Statement(_)))
            && fdef.ret_domain.is_none()
            && let Expr::Subquery(s) = &sub
            && s.params.iter().all(|p| matches!(p, Expr::Const(..)))
        {
            if fdef.strict && s.params.iter().any(|p| matches!(p, Expr::Const(Value::Null, _))) {
                return Ok(Expr::Const(Value::Null, ret));
            }
            // (One that fails stays a call, and fails the statement if planning still has
            // it: `fold_failed`.)
            match crate::sql::expr::eval(&sub, &[], self.ctx) {
                Ok(v) => return Ok(Expr::Const(v, ret)),
                Err(err) if self.fold_err.is_some() => self.fold_failed(&sub, err)?,
                Err(_) => {}
            }
        }
        if fdef.strict {
            // STRICT: NULL in → NULL out without running the body.
            if !args_null.is_empty() {
                return Ok(Expr::Case {
                    operand: None,
                    whens: vec![(Expr::Or(args_null), Expr::Const(Value::Null, ret.clone()))],
                    else_: Some(Box::new(sub)),
                    ty: ret,
                });
            }
        }
        Ok(sub)
    }
}

/// A SQL function's body run for one value (`Project [result] <- Limit 1 <- Project [e]
/// <- One`) as the expression it computes, its parameters (`Outer(1, i)`) replaced by
/// `args`: None when it is anything else.
pub(crate) fn inline_scalar(plan: &Plan, args: &[Expr]) -> Option<Expr> {
    let Plan::Project { input, exprs } = plan else {
        return None;
    };
    let [top] = exprs.as_slice() else {
        return None;
    };
    let Plan::Limit {
        input: body,
        offset: None,
        with_ties: None,
        ..
    } = &**input
    else {
        return None;
    };
    let Plan::Project {
        input: one,
        exprs: bexprs,
    } = &**body
    else {
        return None;
    };
    let ([b], Plan::One) = (bexprs.as_slice(), &**one) else {
        return None;
    };
    // Only the parameters may come from outside; nothing that runs apart.
    let mut ok = true;
    b.walk(&mut |x| match x {
        Expr::Outer(1, i, _) if *i < args.len() => {}
        Expr::Outer(..)
        | Expr::Col(..)
        | Expr::Subquery(_)
        | Expr::Agg(..)
        | Expr::Window(..)
        | Expr::Srf(..)
        | Expr::Param(..) => ok = false,
        _ => {}
    });
    top.walk(&mut |x| match x {
        Expr::Col(0, _) => {}
        Expr::Col(..) | Expr::Outer(..) | Expr::Subquery(_) | Expr::Param(..) => ok = false,
        _ => {}
    });
    if !ok {
        return None;
    }
    // eval_const_expressions: a field of a row constructor is that field's expression (a
    // whole-row argument's column, its Var).
    fn field_of_row(x: Expr) -> Expr {
        match x {
            Expr::Field(r, i, t) => match *r {
                Expr::Row(items, _) if i < items.len() && items[i].ty() == t => {
                    items.into_iter().nth(i).expect("checked")
                }
                // (An outer join's nullable whole row: its column all the same.)
                r => match nullable_whole_row(&r) {
                    Some(Expr::Row(items, _)) if items.get(i).is_some_and(|c| c.ty() == t) => items[i].clone(),
                    _ => Expr::Field(Box::new(r), i, t),
                },
            },
            x => x,
        }
    }
    let b = b
        .clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Outer(1, i, _) => args[i].clone(),
                x => field_of_row(x),
            })
        })
        .ok()?;
    top.clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Col(0, _) => b.clone(),
                x => field_of_row(x),
            })
        })
        .ok()
}

/// Where a SQL function is called, for what PostgreSQL's planner does with the call.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FnPlace {
    /// In an expression (or alone in FROM without SETOF): `evaluate_function` runs it over
    /// constants, `inline_function` may put its body in place.
    Scalar,
    /// SETOF, alone in FROM: `inline_set_returning_function` may put its body in place.
    FromSet,
    /// Run by the executor (SETOF in a select list, ROWS FROM, a procedure).
    Run,
}

impl Binder<'_, '_> {
    /// What of SQL function `f` runs for this call, as its errors' CONTEXT shows: the planner
    /// inlining it, or (not inlined, or evaluated over constants) its statement 1. None when
    /// it runs statement by statement when called (several statements, SET clauses), which
    /// frames itself.
    pub(crate) fn sql_fn_at(
        &self,
        f: &crate::catalog::Function,
        args: &[Expr],
        place: FnPlace,
    ) -> Option<SqlFnCall> {
        if f.language != "sql" || !f.config.is_empty() {
            return None;
        }
        let place = if self.fmgr_call { FnPlace::Run } else { place };
        let Ok(stmts) = crate::sql::parse::parse_body_quiet(f) else {
            // An unparsable body (not checked when created) fails while inlining, or
            // starting to run.
            let inlining = place == FnPlace::Scalar
                && !f.procedure
                && !f.security_definer
                && !f.returns_set
                && f.ret != DataType::Record;
            let at = if inlining {
                SqlFnAt::Inlining
            } else {
                SqlFnAt::Startup
            };
            return Some(SqlFnCall {
                at,
                fold: false,
                analyze: at,
            });
        };
        if crate::sql::parse::body_runs_by_statement(f) {
            return None;
        }
        let body = stmts.first().and_then(|s| s.stmt.as_deref());
        let consts = args.iter().all(|a| matches!(a, Expr::Const(..)));
        // evaluate_function: an immutable call over constants runs while planning.
        let evaluated = place == FnPlace::Scalar
            && consts
            && f.volatility == 'i'
            && !f.returns_set
            && f.ret != DataType::Record;
        // inline_function never inlines a function into its own inlined body: that call
        // runs when executed, each level planning its body as fmgr_sql does.
        // inline_set_returning_function has no such check: Postgres inlines a recursive one
        // level by level until a constant-false qual leaves a level out (or the stack runs
        // out); here a recursive one in FROM runs as a call.
        let recursive = match place {
            FnPlace::Scalar => self.inlining.contains(&f.oid),
            _ => self.bodies.contains(&f.oid),
        };
        let inlined = !evaluated && !recursive && inlines(f, body, args, place);
        // A call that isn't inlined of a function whose body is being bound runs its body
        // when called (`bind_user_body`), framing itself.
        if !inlined && self.bodies.contains(&f.oid) {
            return None;
        }
        Some(if inlined {
            // Folded with the constants while inlining (in FROM, later: without the frame).
            SqlFnCall {
                at: SqlFnAt::Inlining,
                // (A set-returning one in FROM: substitute_actual_srf_parameters puts
                // in arguments that read no row, whatever they are.)
                fold: consts && place == FnPlace::Scalar
                    || place == FnPlace::FromSet && args.iter().all(level_free),
                analyze: SqlFnAt::Inlining,
            }
        } else {
            // `inline_function` analyzes the body before it checks it is a simple SELECT.
            let tried = place == FnPlace::Scalar
                && !evaluated
                && !recursive
                && !f.procedure
                && !f.security_definer
                && !f.returns_set
                && f.ret != DataType::Record;
            SqlFnCall {
                at: SqlFnAt::Statement(1),
                fold: evaluated,
                analyze: if tried {
                    SqlFnAt::Inlining
                } else {
                    SqlFnAt::Statement(1)
                },
            }
        })
    }

    /// Bind (part of) a call of SQL function `f` inside its frame (`sql_fn_at`). A position
    /// in its body becomes the error's internal query; without a frame it is dropped.
    pub(crate) fn in_sql_fn<T>(
        &mut self,
        f: &crate::catalog::Function,
        at: Option<SqlFnCall>,
        run: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        self.fold_args = at.is_some_and(|c| c.fold);
        // (A set-returning body inlined in FROM is a subquery, planned on its own.)
        self.inline_body = at.is_some_and(|c| matches!(c.at, SqlFnAt::Inlining)) && !f.returns_set;
        match at {
            // Run by the executor: its privileges are checked when it runs.
            Some(
                c @ SqlFnCall {
                    at: SqlFnAt::Statement(_),
                    ..
                },
            ) => {
                let outer = std::mem::take(&mut self.perms);
                let r = self.ctx.sess.framed(sql_fn_frame(f, c.analyze), |_| run(self));
                self.fn_perms = Some(std::mem::replace(&mut self.perms, outer));
                r
            }
            Some(c) => self.ctx.sess.framed(sql_fn_frame(f, c.at), |_| run(self)),
            None => run(self).map_err(|mut e| {
                if !e.stacked {
                    e.position = None;
                }
                e
            }),
        }
    }

    /// A SQL function's bound body (`in_sql_fn`): run inside its frame, checking its
    /// privileges then, unless the planner inlines it.
    pub(crate) fn sql_fn_plan(
        &mut self,
        f: &crate::catalog::Function,
        at: Option<SqlFnCall>,
        plan: Plan,
    ) -> Plan {
        let perms = self.fn_perms.take();
        match at {
            Some(SqlFnCall {
                at: at @ SqlFnAt::Statement(_),
                ..
            }) => Plan::Context {
                input: Arc::new(plan),
                frame: sql_fn_frame(f, at),
                perms: perms.filter(|p| !p.is_empty()).map(Arc::new),
                volatile: f.volatility == 'v',
                scan: false,
            },
            _ => plan,
        }
    }
}

impl SqlFnCall {
    /// The planner evaluates the call over its constants, or puts the body in its place.
    pub(crate) fn inlined(&self) -> bool {
        self.fold || matches!(self.at, SqlFnAt::Inlining)
    }
}

/// How a call of a SQL function runs (`Binder::sql_fn_at`): `at`, and whether its body is
/// bound over the constant arguments (inlined or evaluated while planning).
#[derive(Clone, Copy)]
pub(crate) struct SqlFnCall {
    at: SqlFnAt,
    fold: bool,
    /// What its errors while the body is analyzed say: the planner tries to inline a scalar
    /// call (analyzing the body) before it finds the body can't be inlined.
    analyze: SqlFnAt,
}

/// The frame of SQL function `f` doing `at`.
pub(crate) fn sql_fn_frame(f: &crate::catalog::Function, at: SqlFnAt) -> crate::error::Frame {
    crate::error::Frame::SqlFn {
        name: Arc::from(f.name.as_str()),
        src: Arc::from(f.body.as_str()),
        at,
    }
}

/// `check_sql_fn_retval`: the final statement's columns (None: it returns no rows)
/// against the function's result.
pub(crate) fn check_sql_retval(
    cat: &crate::catalog::Catalog,
    search_path: &[String],
    f: &crate::catalog::Function,
    last: Option<&[OutCol]>,
) -> Result<()> {
    use crate::types::cast::{CastContext, cast_context};
    let returns = !(f.ret == DataType::Void || f.procedure && f.out_cols.is_empty());
    if !returns {
        return Ok(());
    }
    let ret_name = cat.ret_type_name(f, search_path);
    // Still polymorphic (anyarray for an anyarray statistics column): not a result type.
    if f.ret.is_polymorphic() && f.out_cols.len() <= 1 {
        return Err(Error::new(
            code::INVALID_FUNCTION_DEFINITION,
            format!("return type {ret_name} is not supported for SQL functions"),
        ));
    }
    let shown = |c: &OutCol| cat.format_type(&c.ty, c.domain, search_path);
    let mismatch = |d: String| {
        Error::new(
            code::INVALID_FUNCTION_DEFINITION,
            format!("return type mismatch in function declared to return {ret_name}"),
        )
        .detail(d)
    };
    let Some(cols) = last else {
        return Err(mismatch(
            "Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING."
                .into(),
        ));
    };
    let fits = |from: &DataType, to: &DataType| {
        from == to
            || *from == DataType::Unknown
            || cast_context(from, to).is_some_and(|c| c <= CastContext::Assignment)
    };
    let fields: Vec<DataType> = match &f.ret {
        _ if f.out_cols.len() > 1 => f.out_cols.iter().map(|(_, t)| t.clone()).collect(),
        DataType::Row(r) => r.fields.iter().map(|(_, t)| t.clone()).collect(),
        DataType::Record => return Ok(()),
        t => {
            if cols.len() != 1 {
                return Err(mismatch(
                    "Final statement must return exactly one column.".into(),
                ));
            }
            if !fits(&cols[0].ty, t) {
                return Err(mismatch(format!(
                    "Actual return type is {}.",
                    shown(&cols[0])
                )));
            }
            return Ok(());
        }
    };
    // A row of the result type as one column.
    if cols.len() == 1
        && (cols[0].ty == f.ret
            || fields.len() != 1 && matches!(cols[0].ty, DataType::Row(_) | DataType::Record))
    {
        return Ok(());
    }
    for (i, c) in cols.iter().enumerate() {
        let Some(want) = fields.get(i) else {
            return Err(mismatch("Final statement returns too many columns.".into()));
        };
        if !fits(&c.ty, want) {
            return Err(mismatch(format!(
                "Final statement returns {} instead of {} at column {}.",
                shown(c),
                cat.format_type(want, None, search_path),
                i + 1
            )));
        }
    }
    if cols.len() < fields.len() {
        return Err(mismatch("Final statement returns too few columns.".into()));
    }
    Ok(())
}

/// Whether PostgreSQL's planner puts the body of SQL function `f` (one statement, `body`)
/// in place of a call with `args` (`inline_function`, `inline_set_returning_function`),
/// rather than running it. An immutable call over constants is run (`evaluate_function`).
fn inlines(
    f: &crate::catalog::Function,
    body: Option<&Node>,
    args: &[Expr],
    place: FnPlace,
) -> bool {
    let Some(NodeEnum::SelectStmt(s)) = body.and_then(inner) else {
        return false;
    };
    if f.procedure || f.security_definer || s.into_clause.is_some() {
        return false;
    }
    match place {
        FnPlace::Run => false,
        FnPlace::FromSet => {
            // (Arguments that are volatile or have sub-selects: evaluated once, as the
            // function scan's.)
            f.returns_set
                && !f.strict
                && f.volatility != 'v'
                && f.ret != DataType::Void
                && args.iter().all(|a| !a.is_volatile() && !has_subplan(a))
        }
        FnPlace::Scalar => {
            if f.returns_set || f.ret == DataType::Record {
                return false;
            }
            let simple = s.target_list.len() == 1
                && s.from_clause.is_empty()
                && s.where_clause.is_none()
                && s.group_clause.is_empty()
                && s.having_clause.is_none()
                && s.window_clause.is_empty()
                && s.distinct_clause.is_empty()
                && s.sort_clause.is_empty()
                && s.limit_count.is_none()
                && s.limit_offset.is_none()
                && s.with_clause.is_none()
                && s.values_lists.is_empty()
                && s.locking_clause.is_empty()
                && s.larg.is_none();
            if !simple {
                return false;
            }
            // Each parameter's uses; subqueries, aggregates and window calls stop it.
            let mut uses = vec![0usize; args.len()];
            let Some(target) = s.target_list[0].node.as_ref() else {
                return false;
            };
            for (n, ..) in target.nodes() {
                // (inline_function: a strict function's body must contain only strict
                // constructs, contain_nonstrict_functions.)
                if f.strict {
                    let nonstrict = match n {
                        pg_query::NodeRef::CaseExpr(_)
                        | pg_query::NodeRef::CoalesceExpr(_)
                        | pg_query::NodeRef::NullTest(_)
                        | pg_query::NodeRef::BooleanTest(_)
                        | pg_query::NodeRef::MinMaxExpr(_)
                        | pg_query::NodeRef::AArrayExpr(_)
                        | pg_query::NodeRef::RowExpr(_) => true,
                        pg_query::NodeRef::BoolExpr(b) => b.boolop != pb::BoolExprType::NotExpr as i32,
                        pg_query::NodeRef::AExpr(a) => matches!(
                            pb::AExprKind::try_from(a.kind),
                            Ok(pb::AExprKind::AexprDistinct | pb::AExprKind::AexprNotDistinct | pb::AExprKind::AexprNullif)
                        ),
                        _ => false,
                    };
                    if nonstrict {
                        return false;
                    }
                }
                match n {
                    pg_query::NodeRef::SubLink(_) => return false,
                    pg_query::NodeRef::FuncCall(c)
                        if c.over.is_some()
                            || c.agg_star
                            || c.agg_distinct
                            || !c.agg_order.is_empty()
                            || c.agg_filter.is_some() =>
                    {
                        return false;
                    }
                    pg_query::NodeRef::FuncCall(c)
                        if names(&c.funcname)
                            .last()
                            .is_some_and(|n| crate::exec::agg::is_aggregate(n)) =>
                    {
                        return false;
                    }
                    pg_query::NodeRef::ParamRef(p) => {
                        if let Some(u) = uses.get_mut((p.number as usize).wrapping_sub(1)) {
                            *u += 1;
                        }
                    }
                    pg_query::NodeRef::ColumnRef(c) => {
                        let parts: Vec<&str> = c.fields.iter().filter_map(sval).collect();
                        let name = match parts.as_slice() {
                            [n] => Some(*n),
                            [q, n] if *q == f.name => Some(*n),
                            _ => None,
                        };
                        if let Some(i) = name
                            .and_then(|n| f.args.iter().position(|(a, _)| a.as_deref() == Some(n)))
                            && let Some(u) = uses.get_mut(i)
                        {
                            *u += 1;
                        }
                    }
                    _ => {}
                }
            }
            uses.iter().zip(args).all(|(&u, a)| match u {
                0 => !f.strict,
                1 => true,
                // Used more than once: not when the argument is costly or volatile.
                _ => {
                    let mut ops = 0;
                    let mut sub = false;
                    a.walk(&mut |e| match e {
                        Expr::Call(..) => ops += 1,
                        Expr::Subquery(_) => sub = true,
                        _ => {}
                    });
                    !sub && ops <= 10 && !a.is_volatile()
                }
            })
        }
    }
}

/// Whether `e` has a sub-select (contain_subplans; an inlined function call is none).
fn has_subplan(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| found |= matches!(x, Expr::Subquery(s) if s.func.is_none()));
    found
}

/// Whether `e` reads nothing of the query level it is bound at (no column, no enclosing
/// value, no aggregate): the same expression at any level below it.
pub(crate) fn level_free(e: &Expr) -> bool {
    let mut ok = !e.is_volatile() && !has_subplan(e);
    e.walk(&mut |x| match x {
        Expr::Col(..)
        | Expr::Outer(..)
        | Expr::Agg(..)
        | Expr::Window(..)
        | Expr::Srf(..)
        | Expr::Grouping(_)
        | Expr::CaseTest(_)
        | Expr::Default(_) => ok = false,
        _ => {}
    });
    ok
}

/// A user-defined aggregate as an overload candidate.
/// `shown`: its name as a view shows it (quoted, qualified as needed).
fn user_agg_sig(f: &crate::catalog::Function, shown: &str) -> Arc<FuncDef> {
    let args: Vec<DataType> = f.args.iter().map(|a| a.1.clone()).collect();
    let mut d = (*func::candidate_def(f.oid, args, f.ret.clone())).clone();
    d.name = func::intern(shown);
    d.strict = false;
    if f.is_variadic() && !d.args.is_empty() {
        d.variadic = true;
        d.min_args = d.args.len();
    }
    d.ret = match &f.ret {
        DataType::AnyElement
        | DataType::AnyNonArray
        | DataType::AnyEnum
        | DataType::AnyCompatible
        | DataType::AnyCompatibleNonArray => func::Ret::Poly,
        DataType::AnyArray | DataType::AnyCompatibleArray => func::Ret::PolyArray,
        t => func::Ret::Fixed(t.clone()),
    };
    Arc::new(d)
}

/// A polymorphic function as this call uses it: its polymorphic parameter, result and
/// output column types replaced by the actual ones (taken from the arguments), resolving
/// the anyelement and anycompatible families separately (`enforce_generic_type_consistency`).
fn resolve_polymorphic(
    f: Arc<crate::catalog::Function>,
    params: &[(Option<String>, DataType, Option<String>)],
    args: &[Expr],
    written: &[DataType],
) -> Result<Arc<crate::catalog::Function>> {
    use DataType as D;
    use crate::types::range::{RangeType, builtin_for_subtype};
    let poly = |t: &DataType| t.is_polymorphic();
    if !poly(&f.ret) && !f.args.iter().any(|a| poly(&a.1)) && !f.out_cols.iter().any(|c| poly(&c.1))
    {
        return Ok(f);
    }
    let mut elem: Option<DataType> = None;
    let mut range: Option<Arc<RangeType>> = None;
    let mut compat: Option<DataType> = None;
    let mut compat_range: Option<Arc<RangeType>> = None;
    let (mut family1, mut family2, mut pseudo_array, mut poly_input) = (false, false, false, false);
    let (mut compat_rng, mut compat_mrng) = (
        f.ret == D::AnyCompatibleRange,
        f.ret == D::AnyCompatibleMultirange,
    );
    for (i, (p, a)) in params.iter().zip(args).enumerate() {
        match &p.1 {
            D::AnyElement | D::AnyNonArray | D::AnyEnum | D::AnyArray | D::AnyRange
            | D::AnyMultirange => family1 = true,
            D::AnyCompatible | D::AnyCompatibleNonArray | D::AnyCompatibleArray => family2 = true,
            D::AnyCompatibleRange => (family2, compat_rng) = (true, true),
            D::AnyCompatibleMultirange => (family2, compat_mrng) = (true, true),
            _ => continue,
        }
        let t = a.ty();
        if matches!(t, D::Unknown) || matches!(written.get(i), Some(D::Unknown)) {
            continue;
        }
        if matches!(p.1, D::AnyArray) && matches!(t, D::AnyArray) {
            pseudo_array = true;
            continue;
        }
        if t.is_polymorphic() {
            poly_input = true;
            continue;
        }
        let rng = match &t {
            D::Range(rt) | D::Multirange(rt) => Some(rt.clone()),
            _ => None,
        };
        match &p.1 {
            D::AnyElement | D::AnyNonArray | D::AnyEnum => {
                elem.get_or_insert(t);
            }
            D::AnyArray => {
                if let Some(e) = t.elem() {
                    elem.get_or_insert(e.clone());
                }
            }
            D::AnyRange | D::AnyMultirange => {
                if let Some(rt) = rng {
                    elem.get_or_insert(rt.subtype.clone());
                    range.get_or_insert(rt);
                }
            }
            D::AnyCompatible | D::AnyCompatibleNonArray => {
                compat.get_or_insert(t);
            }
            D::AnyCompatibleArray => {
                if let Some(e) = t.elem() {
                    compat.get_or_insert(e.clone());
                }
            }
            _ => {
                if let Some(rt) = rng {
                    compat.get_or_insert(rt.subtype.clone());
                    compat_range.get_or_insert(rt);
                }
            }
        }
    }
    let unknown = |what: &str| {
        Err(Error::new(
            code::DATATYPE_MISMATCH,
            format!("could not determine polymorphic type {what}because input has type unknown"),
        ))
    };
    if !poly_input {
        if pseudo_array
            && matches!(
                f.ret,
                D::AnyElement | D::AnyNonArray | D::AnyEnum | D::AnyRange | D::AnyMultirange
            )
        {
            return Err(Error::new(
                code::DATATYPE_MISMATCH,
                "cannot determine element type of \"anyarray\" argument",
            ));
        }
        if family1 && !pseudo_array && elem.is_none() {
            return unknown("");
        }
        if family2 && compat_range.is_none() {
            if compat_rng {
                return unknown("anycompatiblerange ");
            }
            if compat_mrng {
                return unknown("anycompatiblemultirange ");
            }
        }
    }
    let range = range.or_else(|| elem.as_ref().and_then(builtin_for_subtype));
    let compat_range = compat_range.or_else(|| compat.as_ref().and_then(builtin_for_subtype));
    let elem = elem.unwrap_or(D::Text);
    let compat = compat.unwrap_or(D::Text);
    let subst = |t: &DataType| match t {
        D::AnyElement | D::AnyNonArray | D::AnyEnum => elem.clone(),
        D::AnyArray if pseudo_array => D::AnyArray,
        D::AnyArray => D::array_of(elem.clone()),
        D::AnyRange => range.clone().map_or_else(|| t.clone(), D::Range),
        D::AnyMultirange => range.clone().map_or_else(|| t.clone(), D::Multirange),
        D::AnyCompatible | D::AnyCompatibleNonArray => compat.clone(),
        D::AnyCompatibleArray => D::array_of(compat.clone()),
        D::AnyCompatibleRange => compat_range.clone().map_or_else(|| t.clone(), D::Range),
        D::AnyCompatibleMultirange => compat_range
            .clone()
            .map_or_else(|| t.clone(), D::Multirange),
        t => t.clone(),
    };
    let mut g = (*f).clone();
    g.ret = subst(&g.ret);
    for a in &mut g.args {
        a.1 = subst(&a.1);
    }
    for c in &mut g.out_cols {
        c.1 = subst(&c.1);
    }
    Ok(Arc::new(g))
}

/// A function's OUT parameters as its record result's fields (an unnamed one is `columnN`).
fn out_fields(f: &crate::catalog::Function) -> Vec<(String, DataType)> {
    f.out_cols
        .iter()
        .enumerate()
        .map(|(i, (n, t))| {
            let n = if n.is_empty() {
                format!("column{}", i + 1)
            } else {
                n.clone()
            };
            (n, t.clone())
        })
        .collect()
}

/// ParseFuncOrColumn's checks of a call's argument names: none twice, and no positional
/// argument after a named one.
fn check_arg_names(arg_nodes: &[Node]) -> Result<()> {
    let mut seen: Vec<&str> = Vec::new();
    for a in arg_nodes {
        match inner(a) {
            Some(NodeEnum::NamedArgExpr(na)) => {
                if seen.contains(&na.name.as_str()) {
                    return Err(with_pos(
                        Error::new(
                            code::SYNTAX_ERROR,
                            format!("argument name \"{}\" used more than once", na.name),
                        ),
                        na.location,
                    ));
                }
                seen.push(&na.name);
            }
            _ if !seen.is_empty() => {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        "positional argument cannot follow named argument",
                    ),
                    super::ast::location(a),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// check_lateral_ref_ok: a reference to a relation a LATERAL item can't see.
fn lateral_ref_error(refname: &str, lat: u8, loc: i32) -> Error {
    let e = Error::new(
        code::INVALID_COLUMN_REFERENCE,
        format!("invalid reference to FROM-clause entry for table \"{refname}\""),
    );
    let e = if lat == super::LAT_TARGET {
        e.hint(format!(
            "There is an entry for table \"{refname}\", but it cannot be referenced from this part of the query."
        ))
    } else {
        e.detail("The combining JOIN type must be INNER or LEFT for a LATERAL reference.")
    };
    with_pos(e, loc)
}

/// varstr_levenshtein (insertion, deletion and substitution cost 1).
fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            cur[j] = sub.min(prev[j] + 1).min(cur[j - 1] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

/// A whole-row reference of a relation an outer join nulls (`find_rel_row`): NULL when the
/// row is null-extended, else the row: the row.
pub fn nullable_whole_row(e: &Expr) -> Option<&Expr> {
    let Expr::Case {
        operand: None,
        whens,
        else_: Some(row),
        ..
    } = e
    else {
        return None;
    };
    let [(test, Expr::Const(Value::Null, _))] = whens.as_slice() else {
        return None;
    };
    let Expr::Row(items, _) = &**row else {
        return None;
    };
    if !is_whole_row(row) {
        return None;
    }
    let tested = |x: &Expr| matches!(x, Expr::IsNull(c, false) if items.iter().any(|i| format!("{i:?}") == format!("{c:?}")));
    let ok = match test {
        Expr::And(parts) => parts.len() == items.len() && parts.iter().all(tested),
        t => tested(t),
    };
    ok.then_some(&**row)
}

/// A whole-row reference (`t.*`, a bare `t`): a ROW of a relation's columns typed as its
/// row (a ROW constructor's fields are `f1`, `f2`, …).
pub fn is_whole_row(e: &Expr) -> bool {
    let Expr::Row(items, DataType::Row(rt)) = e else {
        return false;
    };
    rt.fields.len() == items.len()
        && (rt.oid != 0
            || rt
                .fields
                .iter()
                .enumerate()
                .any(|(i, (n, _))| *n != format!("f{}", i + 1)))
}

pub(crate) fn null_test(arg: Expr, not: bool) -> Expr {
    if let Expr::Row(items, _) = &arg {
        // ROW(...) IS NULL: every field null; IS NOT NULL: every field non-null.
        let parts: Vec<Expr> = items
            .iter()
            .map(|i| Expr::IsNull(Box::new(i.clone()), not))
            .collect();
        return Expr::And(parts);
    }
    // (A composite's test looks at its fields.)
    if let Expr::Const(v, ty) = &arg
        && !matches!(ty, DataType::Row(_) | DataType::Record)
    {
        return Expr::Const(Value::Bool(v.is_null() != not), DataType::Bool);
    }
    Expr::IsNull(Box::new(arg), not)
}

fn missing_rel(rel: &str, loc: i32) -> Error {
    with_pos(
        Error::new(
            code::UNDEFINED_TABLE,
            format!("missing FROM-clause entry for table \"{rel}\""),
        ),
        loc,
    )
}

/// A numeric literal: int8 if it is an integer that fits, otherwise numeric.
fn numeric_const(s: &str) -> Result<Expr> {
    if !s.contains(['.', 'e', 'E']) {
        let digits = s.replace('_', "");
        if let Ok(i) = digits.parse::<i64>() {
            if let Ok(i4) = i32::try_from(i) {
                return Ok(Expr::Const(Value::Int4(i4), DataType::Int4));
            }
            return Ok(Expr::Const(Value::Int8(i), DataType::Int8));
        }
        if let Some(h) = digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("-0x"))
        {
            let neg = digits.starts_with('-');
            if let Ok(v) = i64::from_str_radix(h, 16) {
                let v = if neg { -v } else { v };
                return Ok(match i32::try_from(v) {
                    Ok(x) => Expr::Const(Value::Int4(x), DataType::Int4),
                    Err(_) => Expr::Const(Value::Int8(v), DataType::Int8),
                });
            }
        }
    }
    let n = Numeric::parse_pg(&s.replace('_', ""))?;
    Ok(Expr::Const(Value::numeric(n), DataType::NUMERIC))
}

// Window frame option bits (parsenodes.h).
const FRAMEOPTION_NONDEFAULT: i32 = 0x00001;
const FRAMEOPTION_ROWS: i32 = 0x00004;
const FRAMEOPTION_GROUPS: i32 = 0x00008;
const FRAMEOPTION_BETWEEN: i32 = 0x00010;
const FRAMEOPTION_START_UNBOUNDED_PRECEDING: i32 = 0x00020;
const FRAMEOPTION_END_UNBOUNDED_PRECEDING: i32 = 0x00040;
const FRAMEOPTION_START_UNBOUNDED_FOLLOWING: i32 = 0x00080;
const FRAMEOPTION_END_UNBOUNDED_FOLLOWING: i32 = 0x00100;
const FRAMEOPTION_START_CURRENT_ROW: i32 = 0x00200;
const FRAMEOPTION_END_CURRENT_ROW: i32 = 0x00400;
const FRAMEOPTION_START_OFFSET_PRECEDING: i32 = 0x00800;
const FRAMEOPTION_END_OFFSET_PRECEDING: i32 = 0x01000;
const FRAMEOPTION_START_OFFSET_FOLLOWING: i32 = 0x02000;
const FRAMEOPTION_END_OFFSET_FOLLOWING: i32 = 0x04000;
const FRAMEOPTION_EXCLUDE_CURRENT_ROW: i32 = 0x08000;
const FRAMEOPTION_EXCLUDE_GROUP: i32 = 0x10000;
const FRAMEOPTION_EXCLUDE_TIES: i32 = 0x20000;

#[allow(dead_code)]
fn _unused(_: CastContext, _: Option<&str>) {
    let _ = sval;
    let _ = ast::const_str;
}

/// The built-in window functions' signatures, to tell a call missing OVER from a wrong call.
fn window_sigs() -> &'static func::Registry {
    static R: std::sync::OnceLock<func::Registry> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        use DataType::{AnyCompatible, AnyElement, Float8, Int4, Int8};
        let mut r = func::Registry::default();
        let dummy: func::FnImpl = |_, _| Ok(Value::Null);
        for n in ["row_number", "rank", "dense_rank"] {
            r.func(n, vec![], Int8, dummy);
        }
        for n in ["percent_rank", "cume_dist"] {
            r.func(n, vec![], Float8, dummy);
        }
        r.func("ntile", vec![Int4], Int4, dummy);
        for n in ["lag", "lead"] {
            r.func_ret(n, vec![AnyElement], func::Ret::Poly, dummy);
            r.func_ret(n, vec![AnyElement, Int4], func::Ret::Poly, dummy);
            r.func_ret(
                n,
                vec![AnyCompatible, Int4, AnyCompatible],
                func::Ret::Poly,
                dummy,
            );
        }
        for n in ["first_value", "last_value"] {
            r.func_ret(n, vec![AnyElement], func::Ret::Poly, dummy);
        }
        r.func_ret("nth_value", vec![AnyElement, Int4], func::Ret::Poly, dummy);
        r
    })
}

/// What a call of `def` over `args` needs a collation for (`check_collation_set`'s
/// message), as an implementation that reports it missing.
fn needs_collation(def: &func::FuncDef, args: &[Expr]) -> Option<func::FnImpl> {
    fn stringy(t: &DataType) -> bool {
        match t {
            DataType::Array(e) => stringy(e),
            t => t.is_string() || matches!(t, DataType::Name),
        }
    }
    if def.srf.is_some() || !args.iter().any(|a| stringy(&a.ty())) {
        return None;
    }
    Some(match def.name {
        "=" | "<>" | "<" | "<=" | ">" | ">=" | "^@" | "strpos" | "position" | "replace"
        | "split_part" | "starts_with" | "text_larger" | "text_smaller" | "bttextcmp"
        | "btnamecmp" => indeterminate::<0>,
        "upper" => indeterminate::<1>,
        "lower" | "casefold" => indeterminate::<2>,
        "initcap" => indeterminate::<3>,
        "~~" | "!~~" | "like" | "not_like" | "textlike" | "textnlike" => indeterminate::<4>,
        "~~*" | "!~~*" | "ilike" | "not_ilike" => indeterminate::<5>,
        "~" | "!~" | "~*" | "!~*" | "similar" | "not_similar" | "regexp_replace"
        | "regexp_match" | "regexp_like" | "regexp_count" | "regexp_instr" | "regexp_substr"
        | "regexp_split_to_array" => indeterminate::<6>,
        _ => return None,
    })
}

/// A call whose inputs derive no collation, where it needs one.
fn indeterminate<const K: u8>(_: &[Value], _: &func::FnCtx) -> Result<Value> {
    let what = match K {
        0 => "string comparison",
        1 => "upper() function",
        2 => "lower() function",
        3 => "initcap() function",
        4 => "LIKE",
        5 => "ILIKE",
        7 => "string hashing",
        _ => "regular expression",
    };
    Err(Error::new(
        code::INDETERMINATE_COLLATION,
        format!("could not determine which collation to use for {what}"),
    )
    .hint("Use the COLLATE clause to set the collation explicitly."))
}

/// `eq` (an `=` over JOIN USING's columns, bound before their scopes are visible) as it
/// runs when the two sides' implicit collations conflict: failing for want of a collation.
pub(crate) fn indeterminate_eq(eq: Expr) -> Expr {
    match eq {
        Expr::Call(def, args, ty) => match needs_collation(&def, &args) {
            Some(imp) => Expr::Call(
                Arc::new(func::FuncDef {
                    imp,
                    srf: None,
                    ..(*def).clone()
                }),
                args,
                ty,
            ),
            None => Expr::Call(def, args, ty),
        },
        e => e,
    }
}

/// The call wrapping a hash join key whose `=` has no collation to compare with (its
/// inputs' implicit collations conflict): hashing a value fails, as `hashtext` does.
pub(crate) const INDETERMINATE_HASH: &str = "\u{1}hashtext";

/// The call wrapping a hash join key whose `=` compares under a nondeterministic
/// collation: its value as that collation's sort key (equal keys for the strings it finds
/// equal).
pub(crate) const NONDETERMINISTIC_HASH: &str = "\u{1}hashtext_nondet";

fn nondeterministic_key(a: &[Value], c: &func::FnCtx) -> Result<Value> {
    Ok(match c.coll {
        Some(coll) => crate::types::collation::key_value(coll, &a[0]),
        None => a[0].clone(),
    })
}

/// A hash key's operand of its `=`: (the operand, whether hashing it fails for want of a
/// collation, the nondeterministic collation it hashes under).
pub(crate) fn hash_key_operand(k: &Expr) -> (Expr, bool, Option<Arc<crate::types::collation::Collation>>) {
    match k {
        Expr::Call(d, args, _) if d.name == INDETERMINATE_HASH => (args[0].clone(), true, None),
        Expr::Call(d, args, _) if d.name == NONDETERMINISTIC_HASH => (args[0].clone(), false, d.coll.clone()),
        k => (k.clone(), false, None),
    }
}

/// `eq`, an `=` call, comparing under collation `coll` (a hash key's, rebuilt as a clause).
pub(crate) fn eq_under(eq: Expr, coll: Option<Arc<crate::types::collation::Collation>>) -> Expr {
    match (eq, coll) {
        (Expr::Call(d, args, ty), Some(c)) => Expr::Call(
            Arc::new(func::FuncDef {
                coll: Some(c),
                ..(*d).clone()
            }),
            args,
            ty,
        ),
        (eq, _) => eq,
    }
}

/// A hash join key taken from `eq`, an `=` call: wrapped so that hashing it fails when
/// the call has no collation, or hashes the collation's sort key when it is
/// nondeterministic.
pub(crate) fn hash_key(eq: &Expr, key: Expr) -> Expr {
    let Expr::Call(def, ..) = eq else {
        return key;
    };
    if def.coll.as_ref().is_some_and(|c| !c.deterministic) {
        let ty = key.ty();
        let d = func::FuncDef {
            name: NONDETERMINISTIC_HASH,
            args: vec![ty.clone()],
            ret: func::Ret::Fixed(ty.clone()),
            strict: true,
            imp: nondeterministic_key,
            srf: None,
            ..(**def).clone()
        };
        return Expr::Call(Arc::new(d), vec![key], ty);
    }
    if !std::ptr::fn_addr_eq(def.imp, indeterminate::<0> as func::FnImpl) {
        return key;
    }
    let ty = key.ty();
    let def = func::FuncDef {
        name: INDETERMINATE_HASH,
        args: vec![ty.clone()],
        ret: func::Ret::Fixed(ty.clone()),
        strict: false,
        imp: indeterminate_hash,
        srf: None,
        ..(**def).clone()
    };
    Expr::Call(Arc::new(def), vec![key], ty)
}

/// A grouping key with no collation: hashing a (non-null) value of it fails.
pub(crate) fn unhashable_key(key: Expr) -> Expr {
    let ty = key.ty();
    let Ok(eq) = resolve_binop("=", &ty, &ty) else {
        return key;
    };
    let def = func::FuncDef {
        name: INDETERMINATE_HASH,
        args: vec![ty.clone()],
        ret: func::Ret::Fixed(ty.clone()),
        strict: false,
        imp: indeterminate_hash,
        srf: None,
        ..(*eq.def).clone()
    };
    Expr::Call(Arc::new(def), vec![key], ty)
}

/// Whether a sort key's comparison is one that fails for want of a collation.
pub(crate) fn is_indeterminate(def: &func::FuncDef) -> bool {
    std::ptr::fn_addr_eq(def.imp, indeterminate::<0> as func::FnImpl)
}

fn indeterminate_hash(args: &[Value], _: &func::FnCtx) -> Result<Value> {
    match args.first() {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(_) => Err(Error::new(
            code::INDETERMINATE_COLLATION,
            "could not determine which collation to use for string hashing",
        )
        .hint("Use the COLLATE clause to set the collation explicitly.")),
    }
}

/// Joins on keys without a collation, as Postgres runs them: a merge join fails as it
/// starts (setting up the key's sort support), before any row is read, so EXPLAIN does too;
/// a nested loop compares (and fails comparing two values); a hash join fails hashing one.
pub(crate) fn check_merge_keys(ctx: &crate::exec::ExecCtx, plan: &mut Plan) -> Result<()> {
    // (Nothing to change: the plan stays as it is, its nodes too, as `ExecCtx::bound_trees`
    // needs.)
    if !has_indeterminate_keys(plan) {
        return check_full_joins(ctx, plan);
    }
    check_merge_keys_mut(ctx, plan)
}

/// A constant-false restriction of a FULL JOIN's own join relation marks it dummy before any
/// path is looked for (restriction_is_constant_false, after eval_const_expressions: `NOT
/// NULL` too): it never runs.
fn full_join_constant_false(ctx: &crate::exec::ExecCtx, e: &Expr) -> bool {
    let mut plain = e.columns().is_empty() && e.is_immutable() && !e.has_outer_refs();
    e.walk(&mut |x| plain &= !matches!(x, Expr::Subquery(_) | Expr::Param(..)));
    plain && matches!(crate::sql::expr::eval(e, &[], ctx), Ok(Value::Bool(false) | Value::Null))
}

/// `check_merge_keys`' FULL JOIN check alone.
fn check_full_joins(ctx: &crate::exec::ExecCtx, plan: &Plan) -> Result<()> {
    if let Plan::Filter { input, pred } = plan
        && let Plan::Join { kind: crate::sql::plan::JoinKind::Full, left, right, .. } = &**input
        && full_join_constant_false(ctx, pred)
    {
        check_full_joins(ctx, left)?;
        return check_full_joins(ctx, right);
    }
    if let Plan::Join { kind: crate::sql::plan::JoinKind::Full, cond, hash, nleft, params, .. } = plan
        && params.is_empty()
        && !super::optimize::full_join_supported(&ctx.catalog(), cond.as_ref(), hash.is_some(), *nleft)
    {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions",
        ));
    }
    match plan {
        Plan::Join { left, right, .. } | Plan::SetOp { left, right, .. } => {
            check_full_joins(ctx, left)?;
            check_full_joins(ctx, right)
        }
        Plan::With { ctes, body, .. } => {
            for (_, p) in ctes {
                check_full_joins(ctx, p)?;
            }
            check_full_joins(ctx, body)
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Agg { input, .. }
        | Plan::Window { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => check_full_joins(ctx, input),
        Plan::Append { inputs, .. } => inputs.iter().try_for_each(|p| check_full_joins(ctx, p)),
        _ => Ok(()),
    }
}

fn check_merge_keys_mut(ctx: &crate::exec::ExecCtx, plan: &mut Plan) -> Result<()> {
    let bad = |k: &Expr| matches!(k, Expr::Call(d, ..) if d.name == INDETERMINATE_HASH);
    // (A FULL JOIN under a constant-false restriction of its own: `full_join_constant_false`.)
    if let Plan::Filter { input, pred } = plan
        && full_join_constant_false(ctx, pred)
        && let Plan::Join { kind: crate::sql::plan::JoinKind::Full, left, right, .. } = Arc::make_mut(input)
    {
        check_merge_keys(ctx, Arc::make_mut(left))?;
        return check_merge_keys(ctx, Arc::make_mut(right));
    }
    // populate_joinrel_with_paths: a FULL JOIN no merge or hash join can run.
    if let Plan::Join { kind: crate::sql::plan::JoinKind::Full, cond, hash, nleft, params, .. } = &*plan
        && params.is_empty()
        && !super::optimize::full_join_supported(&ctx.catalog(), cond.as_ref(), hash.is_some(), *nleft)
    {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions",
        ));
    }
    match plan {
        Plan::Join {
            left,
            right,
            hash,
            meta,
            params,
            ..
        } => {
            check_merge_keys(ctx, Arc::make_mut(left))?;
            check_merge_keys(ctx, Arc::make_mut(right))?;
            if !params.is_empty() || !hash.as_ref().is_some_and(|(lk, rk)| lk.iter().chain(rk).any(bad)) {
                return Ok(());
            }
            use crate::sql::plan::JoinMethod as M;
            let method = match meta.method {
                Some(m) => m,
                // Not costed yet: what EXPLAIN chooses.
                None => {
                    // (Costed with the keys as they are, not as wrapped here.)
                    let mut plain = plan.clone();
                    if let Plan::Join {
                        hash: Some((lk, rk)),
                        ..
                    } = &mut plain
                    {
                        for k in lk.iter_mut().chain(rk.iter_mut()) {
                            if let Expr::Call(d, a, _) = k
                                && d.name == INDETERMINATE_HASH
                            {
                                *k = a.remove(0);
                            }
                        }
                    }
                    let est = crate::sql::explain::estimate(&Arc::new(plain), ctx);
                    match est.node_type {
                        "Merge Join" => M::Merge,
                        "Nested Loop" => M::NestLoop,
                        _ => M::Hash,
                    }
                }
            };
            let Plan::Join { hash, cond, nleft, .. } = plan else {
                unreachable!()
            };
            match method {
                M::Merge => Err(Error::new(
                    code::INDETERMINATE_COLLATION,
                    "could not determine which collation to use for string comparison",
                )
                .hint("Use the COLLATE clause to set the collation explicitly.")),
                M::NestLoop => {
                    // The keys go back into the join filter as the `=` they came from.
                    let (lk, rk) = hash.take().unwrap_or_default();
                    let unwrap = |k: Expr| match k {
                        Expr::Call(d, mut a, _) if d.name == INDETERMINATE_HASH => (Some(d), a.remove(0)),
                        k => (None, k),
                    };
                    let mut parts: Vec<Expr> = cond.take().into_iter().collect();
                    for (l, r) in lk.into_iter().zip(rk) {
                        let (d, l) = unwrap(l);
                        let (_, r) = unwrap(r);
                        let r = r.shift_cols(*nleft as isize);
                        let eq = match d {
                            Some(d) => {
                                let ty = l.ty();
                                let def = func::FuncDef {
                                    name: "=",
                                    args: vec![ty.clone(), ty],
                                    ret: func::Ret::Fixed(DataType::Bool),
                                    strict: true,
                                    imp: indeterminate::<0>,
                                    ..(*d).clone()
                                };
                                Expr::Call(Arc::new(def), vec![l, r], DataType::Bool)
                            }
                            None => {
                                let ty = l.ty();
                                let def = resolve_binop("=", &ty, &ty)?.def;
                                Expr::Call(def, vec![l, r], DataType::Bool)
                            }
                        };
                        parts.push(eq);
                    }
                    *cond = match parts.len() {
                        0 => None,
                        1 => parts.pop(),
                        _ => Some(Expr::And(parts)),
                    };
                    Ok(())
                }
                M::Hash => Ok(()),
            }
        }
        Plan::With { ctes, body, .. } => {
            for (_, p) in ctes {
                check_merge_keys(ctx, Arc::make_mut(p))?;
            }
            check_merge_keys(ctx, Arc::make_mut(body))
        }
        // Never true: the planner drops the input altogether (so nothing is hashed).
        Plan::Filter { pred, input }
            if {
                let never = |q: &Expr| matches!(q, Expr::Const(Value::Bool(false) | Value::Null, _));
                never(pred) || matches!(&*pred, Expr::And(parts) if parts.iter().any(never))
            } =>
        {
            if has_indeterminate_keys(input) {
                let ncols = super::plan_width(input);
                *plan = Plan::Empty { ncols, cols: None, info: None };
            }
            Ok(())
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Agg { input, .. }
        | Plan::Window { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => check_merge_keys(ctx, Arc::make_mut(input)),
        Plan::SetOp { left, right, .. } => {
            check_merge_keys(ctx, Arc::make_mut(left))?;
            check_merge_keys(ctx, Arc::make_mut(right))
        }
        Plan::Append { inputs, .. } => inputs
            .iter_mut()
            .try_for_each(|p| check_merge_keys(ctx, Arc::make_mut(p))),
        _ => Ok(()),
    }
}

/// Whether a join in `plan` has a key wrapped to fail hashing.
fn has_indeterminate_keys(plan: &Plan) -> bool {
    match plan {
        Plan::Join {
            left, right, hash, ..
        } => {
            hash.as_ref().is_some_and(|(lk, rk)| {
                lk.iter()
                    .chain(rk)
                    .any(|k| matches!(k, Expr::Call(d, ..) if d.name == INDETERMINATE_HASH))
            }) || has_indeterminate_keys(left)
                || has_indeterminate_keys(right)
        }
        Plan::With { ctes, body, .. } => {
            ctes.iter().any(|(_, p)| has_indeterminate_keys(p)) || has_indeterminate_keys(body)
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Agg { input, .. }
        | Plan::Window { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. } | Plan::Context { input, .. } => has_indeterminate_keys(input),
        Plan::SetOp { left, right, .. } => {
            has_indeterminate_keys(left) || has_indeterminate_keys(right)
        }
        Plan::Append { inputs, .. } => inputs.iter().any(|p| has_indeterminate_keys(p)),
        _ => false,
    }
}

/// A CASE's WHEN comparison with its constant operand in place of the CaseTest (not into
/// a nested CASE, whose tests are its own).
fn substitute_case_test(e: Expr, v: &Expr) -> Expr {
    match e {
        Expr::CaseTest(_) => v.clone(),
        e @ Expr::Case { .. } => e,
        e => e
            .map_children(&mut |c| Ok(substitute_case_test(c, v)))
            .unwrap_or_else(|_| unreachable!()),
    }
}

/// build_function_result_tupdesc: an unnamed OUT parameter's column is `column<N>`.
pub(super) fn out_col_name(n: &str, i: usize) -> String {
    if n.is_empty() {
        format!("column{}", i + 1)
    } else {
        n.to_string()
    }
}

/// What simplifying `e` (eval_const_expressions) would raise: the error of a constant call
/// or cast kept unfolded until the expression was known to be planned.
pub fn check_folds(e: &Expr, ctx: &crate::exec::ExecCtx) -> Result<()> {
    let mut err = None;
    e.walk(&mut |x| {
        if err.is_some() {
            return;
        }
        let foldable = match x {
            Expr::Call(def, args, _) => {
                def.volatility == 'i' && !def.is_srf() && args.iter().all(Expr::is_const)
            }
            Expr::Cast(a, ..) => a.is_const() && x.is_immutable(),
            _ => false,
        };
        if foldable && let Err(e) = eval(x, &[], ctx) {
            err = Some(e);
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The call a native subscripting routine runs as (fetch: `(value, key)`; assign:
/// `(value, key, element)`).
pub(crate) fn native_subscript_def(imp: func::FnImpl, args: Vec<DataType>, strict: bool) -> Arc<FuncDef> {
    let ret = args[0].clone();
    Arc::new(FuncDef {
        oid: 0,
        name: "subscript",
        min_args: args.len(),
        args,
        variadic: false,
        spread: false,
        ret: func::Ret::Fixed(if strict { DataType::Text } else { ret }),
        strict,
        volatility: 'i',
        imp,
        srf: None,
        out_cols: vec![],
        arg_names: vec![],
        defaults: vec![],
        coll: None,
    })
}

/// ParseFuncOrColumn: a parameter of type void is no argument at all (what clients calling a
/// function with OUT parameters bind for them, as pgjdbc's `{? = call f(?)}` does).
fn drop_void_params(args: &mut Vec<Expr>, names: &mut Vec<Option<String>>) {
    if !args.iter().any(|a| matches!(a, Expr::Param(_, DataType::Void))) {
        return;
    }
    let keep: Vec<bool> = args
        .iter()
        .map(|a| !matches!(a, Expr::Param(_, DataType::Void)))
        .collect();
    let mut k = keep.iter();
    args.retain(|_| *k.next().unwrap());
    let mut k = keep.iter();
    names.retain(|_| *k.next().unwrap());
}
