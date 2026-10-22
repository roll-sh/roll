//! Bound expressions and their evaluation.

use super::func::{FnCtx, FuncDef, call2};
use super::plan::Plan;
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::types::array::ArrayValue;
use crate::types::cast::cast_value;
use crate::types::{DataType, Value};
use crate::{bail, err};
use std::fmt;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolTest {
    IsTrue,
    IsNotTrue,
    IsFalse,
    IsNotFalse,
    IsUnknown,
    IsNotUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubqueryKind {
    /// `(SELECT x)`: one row, one column (or NULL).
    Scalar,
    Exists,
    /// `x op ANY (SELECT …)`
    Any,
    /// `x op ALL (SELECT …)`
    All,
    /// `ARRAY(SELECT …)`
    Array,
    /// `ROW(a, …) op (SELECT x, …)`, correlated: the subquery's one row (NULL without
    /// one) compared with `left` by `ops` (ROWCOMPARE_SUBLINK).
    RowCompare,
}

#[derive(Clone)]
pub struct Subquery {
    pub id: usize,
    pub kind: SubqueryKind,
    pub plan: Arc<Plan>,
    /// Values from the enclosing query (expressions in its scope) that the subquery reads as
    /// Outer(1, i). Empty = uncorrelated: run once per statement.
    pub params: Vec<Expr>,
    /// For ANY/ALL: the left operands and the comparison per column.
    pub left: Vec<Expr>,
    pub ops: Vec<Arc<FuncDef>>,
    pub ty: DataType,
    /// An inlined call of a user-defined function: its volatility.
    pub func: Option<char>,
    /// That call as EXPLAIN shows it (the planner inlines it or keeps the call).
    pub call: Option<Arc<FnShow>>,
    /// ANY / ALL over a user-defined operator's results (`x op c` computed in the
    /// subquery's last projection, tested against true): the subquery's own parameter
    /// count; EXPLAIN shows the comparison as Postgres's test expression,
    /// `x op (SubPlan n).col1`.
    pub test_op: Option<usize>,
    /// The sub-select's output column names (an IN / ANY sublink's; empty when not
    /// recorded): a subquery the semi join reads names them so.
    pub names: Arc<Vec<String>>,
}

/// A call of a user-defined function run as a subquery (`Subquery::func`), as the planner
/// leaves it: the call itself (its arguments are the subquery's first `nargs` params), or
/// (`inline`) its body put in place of it.
#[derive(Debug)]
pub struct FnShow {
    pub def: Arc<FuncDef>,
    pub nargs: usize,
    pub inline: bool,
    /// Its estimated execution cost, in cpu_operator_cost units (procost).
    pub cost: f64,
}

/// A domain's NOT NULL and CHECK constraints (its ancestors' too), applied when a value is
/// cast to the domain or stored in a column of it.
#[derive(Debug)]
pub struct DomainCoerce {
    /// The type: the domain, or its array type.
    pub oid: u32,
    pub name: String,
    /// The name as messages show it (schema-qualified when not on the search path).
    pub shown: String,
    /// The domain's schema (the error's SCHEMA NAME).
    pub schema: String,
    /// The value is an array of the domain: each element is checked.
    pub array: bool,
    pub not_null: bool,
    /// (constraint name, check over Col(0) = the value).
    pub checks: Vec<(String, Expr)>,
    /// Only marks the type a value is declared as (for `pg_typeof`), which Postgres has no
    /// node for: nothing is checked, and deparsing shows just the value.
    pub tag: bool,
    /// A domain over an array of a domain: that array type's coercion, applied first.
    pub base: Option<Arc<DomainCoerce>>,
}

impl DomainCoerce {
    pub fn check(&self, v: &Value, ctx: &ExecCtx) -> Result<()> {
        if self.tag {
            return Ok(());
        }
        if self.array {
            if let Value::Array(a) = v {
                for x in &a.items {
                    self.check_one(x, ctx)?;
                }
            }
            return Ok(());
        }
        self.check_one(v, ctx)
    }

    fn check_one(&self, v: &Value, ctx: &ExecCtx) -> Result<()> {
        if let Some(b) = &self.base {
            b.check(v, ctx)?;
        }
        if v.is_null() && self.not_null {
            return Err(Error::new(
                code::NOT_NULL_VIOLATION,
                format!("domain {} does not allow null values", self.shown),
            )
            .datatype(&self.schema, &self.name));
        }
        for (name, check) in &self.checks {
            if eval(check, std::slice::from_ref(v), ctx)? == Value::Bool(false) {
                return Err(Error::new(
                    code::CHECK_VIOLATION,
                    format!(
                        "value for domain {} violates check constraint \"{name}\"",
                        self.shown
                    ),
                )
                .datatype(&self.schema, &self.name)
                .constraint(name));
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub enum Expr {
    Const(Value, DataType),
    /// Column of the current input row.
    Col(usize, DataType),
    /// Column of an enclosing query's row: (levels up, column).
    Outer(usize, usize, DataType),
    /// `$n` (0-based).
    Param(usize, DataType),
    Call(Arc<FuncDef>, Vec<Expr>, DataType),
    Cast(Box<Expr>, DataType, DataType, bool),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    IsNull(Box<Expr>, bool),
    BoolTest(Box<Expr>, BoolTest),
    /// IS [NOT] DISTINCT FROM, with the type's `=`.
    IsDistinct(Box<Expr>, Box<Expr>, bool, Arc<FuncDef>),
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
        ty: DataType,
    },
    /// The CASE operand inside WHEN comparisons.
    CaseTest(DataType),
    Coalesce(Vec<Expr>, DataType),
    MinMax(Vec<Expr>, bool, DataType, Arc<FuncDef>),
    NullIf(Box<Expr>, Box<Expr>, Arc<FuncDef>, DataType),
    /// `x [NOT] IN (a, b, …)` with the comparison per element.
    InList(Box<Expr>, Vec<(Expr, Arc<FuncDef>)>, bool),
    /// `x op ANY/ALL (array)`.
    ArrayOp {
        left: Box<Expr>,
        right: Box<Expr>,
        op: Arc<FuncDef>,
        all: bool,
    },
    Subquery(Box<Subquery>),
    Array(Vec<Expr>, DataType),
    /// ROW(...): the fields, and the row type (`Record`, or `Row` with field names).
    Row(Vec<Expr>, DataType),
    /// Array subscripts / slices: (lower, upper, is_slice) per dimension.
    Subscript(Box<Expr>, Vec<(Option<Expr>, Option<Expr>, bool)>, DataType),
    /// Field of a composite (`(rec).f`).
    Field(Box<Expr>, usize, DataType),
    /// `base` with an element or slice (`subs`) or a field replaced by `value`: the new value of
    /// a column in `UPDATE ... SET a[i] = v` / `SET c.f = v`.
    Assign {
        base: Box<Expr>,
        subs: Vec<(Option<Expr>, Option<Expr>, bool)>,
        field: Option<usize>,
        value: Box<Expr>,
        ty: DataType,
    },
    /// Placeholder for an aggregate during binding.
    Agg(usize, DataType),
    /// Placeholder for a window function during binding.
    Window(usize, DataType),
    /// Placeholder for a grouping-set key during binding / GROUPING().
    Grouping(Vec<usize>),
    /// A SET-returning function used in the select list (expanded by ProjectSet).
    Srf(usize, DataType),
    /// DEFAULT in INSERT/UPDATE.
    Default(DataType),
    /// A cast to a domain: the base-type value, checked against the domain's constraints.
    CoerceToDomain(Box<Expr>, Arc<DomainCoerce>),
    /// A domain-typed value used as its base type: Postgres's implicit RelabelType, which
    /// deparsing shows (`(col)::integer`). Only built when deparsing.
    Relabel(Box<Expr>, DataType),
    /// A function call written in SQL-standard syntax (`POSITION(a IN b)`, `TRIM(…)`,
    /// `x AT TIME ZONE z`, …), which deparsing shows the same way. Only built when deparsing.
    SqlSyntax(Box<Expr>),
    /// `expr COLLATE name`: the collation's name, and where the clause is (-1: none).
    Collate(Box<Expr>, String, i32),
    /// XMLELEMENT, XMLFOREST, XMLPARSE, … and `IS DOCUMENT` (`XmlExpr`).
    Xml(Box<XmlExpr>),
    /// `ROW(a, b, …) op ROW(x, y, …)` for an ordering operator (RowCompareExpr).
    RowCompare(Box<RowCompare>),
}

/// A row comparison, column by column: the first pair that isn't equal decides it (NULL
/// when a pair has a NULL), `op` on the last pair when all before it are equal.
#[derive(Clone, Debug)]
pub struct RowCompare {
    /// `<`, `<=`, `>` or `>=` (or the pattern family's `~<~` …).
    pub op: &'static str,
    /// Per column: `l op r` (the operands as the comparison coerced them).
    pub cmps: Vec<Expr>,
    /// Per column: `l = r`.
    pub eqs: Vec<Expr>,
}

impl RowCompare {
    /// The left / right operands of each pair.
    pub fn sides(&self) -> (Vec<&Expr>, Vec<&Expr>) {
        let side = |k: usize| {
            self.cmps
                .iter()
                .map(|c| match c {
                    Expr::Call(_, a, _) if a.len() == 2 => &a[k],
                    c => c,
                })
                .collect()
        };
        (side(0), side(1))
    }
}

/// `XmlExprOp`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum XmlOp {
    Concat,
    Element,
    Forest,
    Parse,
    Pi,
    Root,
    Serialize,
    IsDocument,
}

/// `XmlExpr`: `name` and `arg_names` are already mapped to XML names.
#[derive(Clone, Debug)]
pub struct XmlExpr {
    pub op: XmlOp,
    pub name: Option<String>,
    pub named_args: Vec<Expr>,
    pub arg_names: Vec<String>,
    pub args: Vec<Expr>,
    /// DOCUMENT (rather than CONTENT), for XMLPARSE and XMLSERIALIZE.
    pub document: bool,
    pub indent: bool,
    /// XMLSERIALIZE's target type, only for deparsing.
    pub target: DataType,
}

impl Expr {
    pub fn ty(&self) -> DataType {
        match self {
            Expr::Const(_, t)
            | Expr::Col(_, t)
            | Expr::Outer(_, _, t)
            | Expr::Param(_, t)
            | Expr::Call(_, _, t) => t.clone(),
            Expr::Cast(_, _, to, _) => to.clone(),
            Expr::And(_)
            | Expr::Or(_)
            | Expr::Not(_)
            | Expr::IsNull(..)
            | Expr::BoolTest(..)
            | Expr::IsDistinct(..) => DataType::Bool,
            Expr::InList(..) | Expr::ArrayOp { .. } => DataType::Bool,
            Expr::Case { ty, .. }
            | Expr::CaseTest(ty)
            | Expr::Coalesce(_, ty)
            | Expr::MinMax(_, _, ty, _)
            | Expr::NullIf(_, _, _, ty) => ty.clone(),
            Expr::Subquery(s) => s.ty.clone(),
            Expr::Array(_, t) => t.clone(),
            Expr::Row(_, t) => t.clone(),
            Expr::Subscript(_, _, t)
            | Expr::Field(_, _, t)
            | Expr::Agg(_, t)
            | Expr::Window(_, t)
            | Expr::Srf(_, t) => t.clone(),
            Expr::Assign { ty, .. } => ty.clone(),
            Expr::RowCompare(_) => DataType::Bool,
            Expr::Grouping(_) => DataType::Int4,
            Expr::Default(t) => t.clone(),
            Expr::CoerceToDomain(e, _) => e.ty(),
            Expr::Relabel(_, t) => t.clone(),
            Expr::SqlSyntax(e) | Expr::Collate(e, ..) => e.ty(),
            Expr::Xml(x) => match x.op {
                XmlOp::Serialize => DataType::Text,
                XmlOp::IsDocument => DataType::Bool,
                _ => DataType::Xml,
            },
        }
    }

    pub fn null(ty: DataType) -> Expr {
        Expr::Const(Value::Null, ty)
    }

    pub fn bool(b: bool) -> Expr {
        Expr::Const(Value::Bool(b), DataType::Bool)
    }

    pub fn is_const(&self) -> bool {
        matches!(self, Expr::Const(..))
    }

    /// The named composite type this is a `ROW(...)` of, when it is one (a RowExpr of
    /// that type in Postgres).
    pub fn named_row(&self) -> Option<&crate::types::RowType> {
        match self {
            Expr::Row(_, DataType::Row(r)) if r.oid != 0 => Some(r),
            Expr::Cast(inner, _, DataType::Row(r), _)
                if r.oid != 0 && matches!(**inner, Expr::Row(..)) =>
            {
                Some(r)
            }
            _ => None,
        }
    }

    /// Visit every sub-expression (not descending into subquery plans).
    pub fn walk<'e>(&'e self, f: &mut dyn FnMut(&'e Expr)) {
        f(self);
        match self {
            Expr::Call(_, args, _)
            | Expr::And(args)
            | Expr::Or(args)
            | Expr::Coalesce(args, _)
            | Expr::Array(args, _)
            | Expr::Row(args, _) => {
                for a in args {
                    a.walk(f);
                }
            }
            Expr::MinMax(args, ..) => {
                for a in args {
                    a.walk(f);
                }
            }
            Expr::Cast(e, ..)
            | Expr::Not(e)
            | Expr::IsNull(e, _)
            | Expr::BoolTest(e, _)
            | Expr::Field(e, _, _)
            | Expr::CoerceToDomain(e, _)
            | Expr::Relabel(e, _)
            | Expr::SqlSyntax(e)
            | Expr::Collate(e, ..) => e.walk(f),
            Expr::IsDistinct(a, b, ..) | Expr::NullIf(a, b, ..) => {
                a.walk(f);
                b.walk(f);
            }
            Expr::Case {
                operand,
                whens,
                else_,
                ..
            } => {
                if let Some(o) = operand {
                    o.walk(f);
                }
                for (w, t) in whens {
                    w.walk(f);
                    t.walk(f);
                }
                if let Some(e) = else_ {
                    e.walk(f);
                }
            }
            Expr::InList(e, list, _) => {
                e.walk(f);
                for (x, _) in list {
                    x.walk(f);
                }
            }
            Expr::ArrayOp { left, right, .. } => {
                left.walk(f);
                right.walk(f);
            }
            Expr::Subquery(s) => {
                for l in s.left.iter().chain(&s.params) {
                    l.walk(f);
                }
            }
            Expr::Assign {
                base, subs, value, ..
            } => {
                base.walk(f);
                value.walk(f);
                for (a, b, _) in subs {
                    if let Some(a) = a {
                        a.walk(f);
                    }
                    if let Some(b) = b {
                        b.walk(f);
                    }
                }
            }
            Expr::Subscript(e, idx, _) => {
                e.walk(f);
                for (a, b, _) in idx {
                    if let Some(a) = a {
                        a.walk(f);
                    }
                    if let Some(b) = b {
                        b.walk(f);
                    }
                }
            }
            Expr::Xml(x) => {
                for a in x.named_args.iter().chain(&x.args) {
                    a.walk(f);
                }
            }
            Expr::RowCompare(rc) => {
                for a in rc.cmps.iter().chain(&rc.eqs) {
                    a.walk(f);
                }
            }
            _ => {}
        }
    }

    /// Apply `f` to each direct child (not descending further, not into subquery plans).
    pub fn map_children(self, f: &mut dyn FnMut(Expr) -> Result<Expr>) -> Result<Expr> {
        Ok(match self {
            Expr::Call(d, args, t) => {
                Expr::Call(d, args.into_iter().map(&mut *f).collect::<Result<_>>()?, t)
            }
            Expr::And(args) => Expr::And(args.into_iter().map(&mut *f).collect::<Result<_>>()?),
            Expr::Or(args) => Expr::Or(args.into_iter().map(&mut *f).collect::<Result<_>>()?),
            Expr::Coalesce(args, t) => {
                Expr::Coalesce(args.into_iter().map(&mut *f).collect::<Result<_>>()?, t)
            }
            Expr::Array(args, t) => {
                Expr::Array(args.into_iter().map(&mut *f).collect::<Result<_>>()?, t)
            }
            Expr::Row(args, t) => {
                Expr::Row(args.into_iter().map(&mut *f).collect::<Result<_>>()?, t)
            }
            Expr::MinMax(args, g, t, op) => Expr::MinMax(
                args.into_iter().map(&mut *f).collect::<Result<_>>()?,
                g,
                t,
                op,
            ),
            Expr::Cast(e, a, b, x) => Expr::Cast(Box::new(f(*e)?), a, b, x),
            Expr::Not(e) => Expr::Not(Box::new(f(*e)?)),
            Expr::CoerceToDomain(e, d) => Expr::CoerceToDomain(Box::new(f(*e)?), d),
            Expr::Relabel(e, t) => Expr::Relabel(Box::new(f(*e)?), t),
            Expr::SqlSyntax(e) => Expr::SqlSyntax(Box::new(f(*e)?)),
            Expr::Collate(e, c, l) => Expr::Collate(Box::new(f(*e)?), c, l),
            Expr::Xml(mut x) => {
                x.named_args = std::mem::take(&mut x.named_args)
                    .into_iter()
                    .map(&mut *f)
                    .collect::<Result<_>>()?;
                x.args = std::mem::take(&mut x.args)
                    .into_iter()
                    .map(&mut *f)
                    .collect::<Result<_>>()?;
                Expr::Xml(x)
            }
            Expr::RowCompare(mut rc) => {
                rc.cmps = std::mem::take(&mut rc.cmps)
                    .into_iter()
                    .map(&mut *f)
                    .collect::<Result<_>>()?;
                rc.eqs = std::mem::take(&mut rc.eqs)
                    .into_iter()
                    .map(&mut *f)
                    .collect::<Result<_>>()?;
                Expr::RowCompare(rc)
            }
            Expr::IsNull(e, n) => Expr::IsNull(Box::new(f(*e)?), n),
            Expr::BoolTest(e, k) => Expr::BoolTest(Box::new(f(*e)?), k),
            Expr::Field(e, i, t) => Expr::Field(Box::new(f(*e)?), i, t),
            Expr::IsDistinct(a, b, n, op) => {
                Expr::IsDistinct(Box::new(f(*a)?), Box::new(f(*b)?), n, op)
            }
            Expr::NullIf(a, b, op, t) => Expr::NullIf(Box::new(f(*a)?), Box::new(f(*b)?), op, t),
            Expr::Case {
                operand,
                whens,
                else_,
                ty,
            } => Expr::Case {
                operand: match operand {
                    Some(o) => Some(Box::new(f(*o)?)),
                    None => None,
                },
                whens: whens
                    .into_iter()
                    .map(|(w, t)| Ok((f(w)?, f(t)?)))
                    .collect::<Result<_>>()?,
                else_: match else_ {
                    Some(e) => Some(Box::new(f(*e)?)),
                    None => None,
                },
                ty,
            },
            Expr::InList(e, list, n) => Expr::InList(
                Box::new(f(*e)?),
                list.into_iter()
                    .map(|(x, op)| Ok((f(x)?, op)))
                    .collect::<Result<_>>()?,
                n,
            ),
            Expr::ArrayOp {
                left,
                right,
                op,
                all,
            } => Expr::ArrayOp {
                left: Box::new(f(*left)?),
                right: Box::new(f(*right)?),
                op,
                all,
            },
            Expr::Subquery(mut s) => {
                s.left = std::mem::take(&mut s.left)
                    .into_iter()
                    .map(&mut *f)
                    .collect::<Result<_>>()?;
                s.params = std::mem::take(&mut s.params)
                    .into_iter()
                    .map(&mut *f)
                    .collect::<Result<_>>()?;
                Expr::Subquery(s)
            }
            Expr::Assign {
                base,
                subs,
                field,
                value,
                ty,
            } => {
                let mut opt =
                    |e: Option<Expr>| -> Result<Option<Expr>> { e.map(&mut *f).transpose() };
                let subs = subs
                    .into_iter()
                    .map(|(a, b, s)| Ok((opt(a)?, opt(b)?, s)))
                    .collect::<Result<_>>()?;
                Expr::Assign {
                    base: Box::new(f(*base)?),
                    subs,
                    field,
                    value: Box::new(f(*value)?),
                    ty,
                }
            }
            Expr::Subscript(e, idx, t) => Expr::Subscript(
                Box::new(f(*e)?),
                idx.into_iter()
                    .map(|(a, b, s)| {
                        Ok((
                            match a {
                                Some(a) => Some(f(a)?),
                                None => None,
                            },
                            match b {
                                Some(b) => Some(f(b)?),
                                None => None,
                            },
                            s,
                        ))
                    })
                    .collect::<Result<_>>()?,
                t,
            ),
            e => e,
        })
    }

    /// Rewrite bottom-up.
    pub fn transform(self, f: &mut dyn FnMut(Expr) -> Result<Expr>) -> Result<Expr> {
        let e = self.map_children(&mut |c| c.transform(f))?;
        f(e)
    }

    /// Columns of the current row this expression reads.
    pub fn columns(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.walk(&mut |e| {
            if let Expr::Col(i, _) = e {
                out.push(*i);
            }
        });
        out
    }

    pub fn has_outer_refs(&self) -> bool {
        let mut found = false;
        self.walk(&mut |e| {
            if let Expr::Outer(..) = e {
                found = true
            }
        });
        found
    }

    pub fn is_volatile(&self) -> bool {
        let mut v = false;
        self.walk(&mut |e| match e {
            Expr::Call(d, ..) if d.volatility == 'v' => v = true,
            // A function call (run as its body): its declared volatility.
            Expr::Subquery(s) if matches!(s.func, Some('i' | 's')) => {}
            Expr::Subquery(_) => v = true,
            _ => {}
        });
        v
    }

    /// Whether the value depends only on the inputs (index expressions, generated columns).
    pub fn is_immutable(&self) -> bool {
        let mut ok = true;
        self.walk(&mut |e| match e {
            // JSON_EXISTS / JSON_VALUE / JSON_QUERY: as their path (contain_mutable_functions).
            Expr::Call(d, args, _) if crate::sql::func::sqljson::is_query_func(d) => {
                if crate::sql::func::sqljson::path_mutable(args) {
                    ok = false
                }
            }
            Expr::Call(d, ..) if d.volatility != 'i' => ok = false,
            Expr::Subquery(s) if s.func != Some('i') => ok = false,
            Expr::Cast(_, from, to, _) if !cast_immutable(from, to) => ok = false,
            _ => {}
        });
        ok
    }

    /// contain_mutable_functions_after_planning: as `is_immutable`, but a SQL function call
    /// the planner inlines counts as its body, not its declared volatility.
    pub fn is_immutable_after_planning(&self) -> bool {
        fn plan_ok(p: &crate::sql::plan::Plan) -> Option<bool> {
            use crate::sql::plan::Plan;
            match p {
                Plan::One => Some(true),
                Plan::Project { input, exprs } => {
                    Some(plan_ok(input)? && exprs.iter().all(Expr::is_immutable_after_planning))
                }
                Plan::Limit { input, .. } | Plan::Context { input, .. } => plan_ok(input),
                _ => None,
            }
        }
        let mut ok = true;
        self.walk(&mut |e| match e {
            // (Its arguments are walked as the subquery's params.)
            Expr::Subquery(s)
                if s.func.is_some() && s.call.as_ref().is_some_and(|c| c.inline) =>
            {
                ok &= plan_ok(&s.plan).unwrap_or(s.func == Some('i'));
            }
            e => ok &= e.is_immutable_self(),
        });
        ok
    }

    /// `is_immutable` for this node alone.
    fn is_immutable_self(&self) -> bool {
        match self {
            Expr::Call(d, args, _) if crate::sql::func::sqljson::is_query_func(d) => {
                !crate::sql::func::sqljson::path_mutable(args)
            }
            Expr::Call(d, ..) => d.volatility == 'i',
            Expr::Subquery(s) => s.func == Some('i'),
            Expr::Cast(_, from, to, _) => cast_immutable(from, to),
            _ => true,
        }
    }

    /// Structural equality (for matching GROUP BY expressions).
    pub fn same_as(&self, other: &Expr) -> bool {
        format!("{self:?}") == format!("{other:?}")
    }

    /// `same_as`, where two sublinks are equal when their queries are (Postgres's equal()
    /// of SubLinks: a GROUP BY key written twice).
    pub fn same_query_as(&self, other: &Expr) -> bool {
        if self.same_as(other) {
            return true;
        }
        match (self, other) {
            (Expr::Subquery(a), Expr::Subquery(b)) => {
                a.kind == b.kind
                    && a.ty == b.ty
                    && a.params.len() == b.params.len()
                    && a.params.iter().zip(&b.params).all(|(x, y)| x.same_query_as(y))
                    && a.left.len() == b.left.len()
                    && a.left.iter().zip(&b.left).all(|(x, y)| x.same_query_as(y))
                    && format!("{:?}", a.plan) == format!("{:?}", b.plan)
            }
            _ => false,
        }
    }

    /// Shift column references by `delta` (when an expression moves over a join's right side).
    pub fn shift_cols(self, delta: isize) -> Expr {
        self.transform(&mut |e| {
            Ok(match e {
                Expr::Col(i, t) => Expr::Col((i as isize + delta) as usize, t),
                e => e,
            })
        })
        .unwrap()
    }

    /// Replace column references through a mapping.
    pub fn remap_cols(self, map: &dyn Fn(usize) -> usize) -> Expr {
        self.transform(&mut |e| {
            Ok(match e {
                Expr::Col(i, t) => Expr::Col(map(i), t),
                e => e,
            })
        })
        .unwrap()
    }
}

impl fmt::Debug for Expr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Const(v, t) => write!(f, "{v:?}::{}", t.typname()),
            Expr::Col(i, _) => write!(f, "#{i}"),
            Expr::Outer(l, i, _) => write!(f, "^{l}#{i}"),
            Expr::Param(i, _) => write!(f, "${}", i + 1),
            Expr::Call(d, args, _) => write!(f, "{}({:?})", d.name, args),
            Expr::Cast(e, _, t, _) => write!(f, "({e:?})::{}", t.typname()),
            Expr::Relabel(e, t) => write!(f, "({e:?})::{}", t.typname()),
            Expr::SqlSyntax(e) => e.fmt(f),
            Expr::Collate(e, c, _) => write!(f, "({e:?} COLLATE {c})"),
            Expr::Xml(x) => write!(
                f,
                "XML{:?}({:?}, {:?}, {:?}, {:?})",
                x.op, x.name, x.arg_names, x.named_args, x.args
            ),
            Expr::RowCompare(rc) => write!(f, "ROWCMP{}({:?}, {:?})", rc.op, rc.cmps, rc.eqs),
            Expr::And(a) => write!(f, "AND{a:?}"),
            Expr::Or(a) => write!(f, "OR{a:?}"),
            Expr::Not(e) => write!(f, "NOT({e:?})"),
            Expr::IsNull(e, n) => write!(f, "({e:?}) IS {}NULL", if *n { "NOT " } else { "" }),
            Expr::BoolTest(e, k) => write!(f, "({e:?}) {k:?}"),
            Expr::IsDistinct(a, b, n, _) => write!(
                f,
                "({a:?}) IS {}DISTINCT FROM ({b:?})",
                if *n { "NOT " } else { "" }
            ),
            Expr::Case {
                operand,
                whens,
                else_,
                ..
            } => write!(f, "CASE {operand:?} {whens:?} ELSE {else_:?}"),
            Expr::CaseTest(_) => write!(f, "CASETEST"),
            Expr::Coalesce(a, _) => write!(f, "COALESCE{a:?}"),
            Expr::MinMax(a, g, _, _) => write!(f, "{}{a:?}", if *g { "GREATEST" } else { "LEAST" }),
            Expr::NullIf(a, b, _, _) => write!(f, "NULLIF({a:?},{b:?})"),
            Expr::InList(e, l, n) => {
                write!(
                    f,
                    "({e:?}) {}IN {:?}",
                    if *n { "NOT " } else { "" },
                    l.iter().map(|x| &x.0).collect::<Vec<_>>()
                )
            }
            Expr::ArrayOp {
                left,
                right,
                op,
                all,
            } => {
                write!(
                    f,
                    "({left:?}) {} {}({right:?})",
                    op.name,
                    if *all { "ALL" } else { "ANY" }
                )
            }
            // A function's body run as a subquery is that call (equal calls are equal
            // expressions, as GROUP BY / ORDER BY matching wants).
            Expr::Subquery(s) => match &s.call {
                Some(c) => write!(f, "CALL {}({:?})", c.def.name, s.params),
                None => write!(f, "SUBQUERY#{}", s.id),
            },
            Expr::Array(a, _) => write!(f, "ARRAY{a:?}"),
            Expr::Row(a, _) => write!(f, "ROW{a:?}"),
            Expr::Subscript(e, i, _) => write!(f, "({e:?})[{}]", i.len()),
            Expr::Field(e, i, _) => write!(f, "({e:?}).{i}"),
            Expr::Assign {
                base,
                subs,
                field,
                value,
                ..
            } => write!(f, "ASSIGN({base:?}, {}, {field:?}, {value:?})", subs.len()),
            Expr::Agg(i, _) => write!(f, "AGG#{i}"),
            Expr::Window(i, _) => write!(f, "WIN#{i}"),
            Expr::Grouping(c) => write!(f, "GROUPING{c:?}"),
            Expr::Srf(i, _) => write!(f, "SRF#{i}"),
            Expr::Default(_) => write!(f, "DEFAULT"),
            Expr::CoerceToDomain(e, d) => write!(f, "({e:?})::{}", d.name),
        }
    }
}

fn truthy(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        _ => Some(false),
    }
}

/// A call of a variadic built-in with `VARIADIC arr`: the array's elements (of any
/// dimensions) become the trailing arguments. A NULL array gives NULL, except that format()
/// treats it as no arguments.
fn eval_spread(
    def: &FuncDef,
    args: &[Expr],
    ty: &DataType,
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<Value> {
    let fixed = args.len() - 1;
    let mut vals = Vec::with_capacity(args.len());
    for a in &args[..fixed] {
        let v = eval(a, row, ctx)?;
        if def.strict && v.is_null() {
            return Ok(Value::Null);
        }
        vals.push(v);
    }
    let elem = args[fixed].ty().elem().cloned().unwrap_or(DataType::Text);
    if def.name == "satisfies_hash_partition" {
        let items = match eval(&args[fixed], row, ctx)? {
            Value::Array(a) => Some(a.items.clone()),
            _ => None,
        };
        let types = args[..fixed].to_vec();
        return crate::sql::func::system::satisfies_hash_partition_variadic(
            &vals,
            items,
            &elem,
            &FnCtx {
                exec: ctx,
                ret: ty,
                args: &types,
                coll: def.coll.as_deref(),
            },
        );
    }
    let items = match eval(&args[fixed], row, ctx)? {
        Value::Array(a) => a.items.clone(),
        Value::Null if def.name == "format" => Vec::new(),
        Value::Null => return Ok(Value::Null),
        v => {
            return Err(crate::error::internal(format!(
                "VARIADIC argument {v:?} is not an array"
            )));
        }
    };
    let mut types = args[..fixed].to_vec();
    types.extend(std::iter::repeat_n(
        Expr::Const(Value::Null, elem),
        items.len(),
    ));
    vals.extend(items);
    (def.imp)(
        &vals,
        &FnCtx {
            exec: ctx,
            ret: ty,
            args: &types,
            coll: def.coll.as_deref(),
        },
    )
}

/// Evaluate `e` over `row`.
pub fn eval(e: &Expr, row: &[Value], ctx: &ExecCtx) -> Result<Value> {
    match e {
        Expr::Const(v, _) => Ok(v.clone()),
        Expr::Col(i, _) => row
            .get(*i)
            .cloned()
            .ok_or_else(|| crate::error::internal(format!("column #{i} out of range"))),
        Expr::Relabel(e, _) | Expr::SqlSyntax(e) | Expr::Collate(e, ..) => eval(e, row, ctx),
        Expr::Xml(x) => crate::sql::func::xml::eval_xml_expr(x, row, ctx),
        Expr::RowCompare(rc) => {
            // ExecEvalRowCompare: the first pair not equal decides.
            let n = rc.cmps.len();
            for i in 0..n {
                if i + 1 < n {
                    match eval(&rc.eqs[i], row, ctx)? {
                        Value::Bool(true) => continue,
                        Value::Bool(false) => {}
                        _ => return Ok(Value::Null),
                    }
                }
                return eval(&rc.cmps[i], row, ctx);
            }
            Ok(Value::Null)
        }
        Expr::Outer(level, i, _) => ctx.outer_value(*level, *i),
        Expr::Param(i, _) => ctx.param(*i),
        Expr::Call(def, args, ty) if def.spread => eval_spread(def, args, ty, row, ctx),
        Expr::Call(def, args, ty) => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                let v = eval(a, row, ctx)?;
                if def.strict && v.is_null() {
                    return Ok(Value::Null);
                }
                vals.push(v);
            }
            // Range bounds compare under the collation the call carries.
            let _g = def
                .coll
                .as_ref()
                .filter(|_| {
                    matches!(ty, DataType::Range(_) | DataType::Multirange(_))
                        || vals
                            .iter()
                            .any(|v| matches!(v, Value::Range(_) | Value::Multirange(_)))
                })
                .map(|c| crate::types::range::under(Some(c.clone())));
            let v = (def.imp)(
                &vals,
                &FnCtx {
                    exec: ctx,
                    ret: ty,
                    args,
                    coll: def.coll.as_deref(),
                },
            )?;
            // A void result is a non-null empty value, as in Postgres.
            if matches!(ty, DataType::Void) && v.is_null() {
                return Ok(Value::Text(Arc::from("")));
            }
            Ok(v)
        }
        Expr::Cast(inner, from, to, explicit) => {
            let v = eval(inner, row, ctx)?;
            if v.is_null() {
                return Ok(Value::Null);
            }
            cast_value(v, from, to, ctx.fmt(), *explicit).map_err(|e| ctx.cast_error(e, from, to))
        }
        Expr::And(args) => {
            let mut saw_null = false;
            for a in args {
                match truthy(&eval(a, row, ctx)?) {
                    Some(false) => return Ok(Value::Bool(false)),
                    None => saw_null = true,
                    Some(true) => {}
                }
            }
            Ok(if saw_null {
                Value::Null
            } else {
                Value::Bool(true)
            })
        }
        Expr::Or(args) => {
            let mut saw_null = false;
            for a in args {
                match truthy(&eval(a, row, ctx)?) {
                    Some(true) => return Ok(Value::Bool(true)),
                    None => saw_null = true,
                    Some(false) => {}
                }
            }
            Ok(if saw_null {
                Value::Null
            } else {
                Value::Bool(false)
            })
        }
        Expr::Not(a) => Ok(match truthy(&eval(a, row, ctx)?) {
            Some(b) => Value::Bool(!b),
            None => Value::Null,
        }),
        Expr::IsNull(a, negated) => {
            let v = eval(a, row, ctx)?;
            // ROW(...) IS NULL is true only if every field is null.
            let is_null = match &v {
                Value::Record(fields) => {
                    if *negated {
                        return Ok(Value::Bool(fields.iter().all(|f| !f.is_null())));
                    }
                    fields.iter().all(Value::is_null)
                }
                v => v.is_null(),
            };
            Ok(Value::Bool(is_null != *negated))
        }
        Expr::BoolTest(a, kind) => {
            let v = truthy(&eval(a, row, ctx)?);
            Ok(Value::Bool(match kind {
                BoolTest::IsTrue => v == Some(true),
                BoolTest::IsNotTrue => v != Some(true),
                BoolTest::IsFalse => v == Some(false),
                BoolTest::IsNotFalse => v != Some(false),
                BoolTest::IsUnknown => v.is_none(),
                BoolTest::IsNotUnknown => v.is_some(),
            }))
        }
        Expr::IsDistinct(a, b, negated, eq) => {
            let x = eval(a, row, ctx)?;
            let y = eval(b, row, ctx)?;
            let distinct = match (x.is_null(), y.is_null()) {
                (true, true) => false,
                (true, false) | (false, true) => true,
                _ => call2(eq, x, y, &a.ty(), ctx)? != Value::Bool(true),
            };
            Ok(Value::Bool(distinct != *negated))
        }
        Expr::Case {
            operand,
            whens,
            else_,
            ..
        } => {
            let test = match operand {
                Some(o) => Some(eval(o, row, ctx)?),
                None => None,
            };
            for (cond, result) in whens {
                let c = match &test {
                    Some(t) => ctx.with_case_value(t.clone(), || eval(cond, row, ctx))?,
                    None => eval(cond, row, ctx)?,
                };
                if truthy(&c) == Some(true) {
                    return eval(result, row, ctx);
                }
            }
            match else_ {
                Some(e) => eval(e, row, ctx),
                None => Ok(Value::Null),
            }
        }
        Expr::CaseTest(_) => ctx.case_value(),
        Expr::Coalesce(args, _) => {
            for a in args {
                let v = eval(a, row, ctx)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Ok(Value::Null)
        }
        Expr::MinMax(args, greatest, _, cmp) => {
            let mut best: Option<Value> = None;
            for a in args {
                let v = eval(a, row, ctx)?;
                if v.is_null() {
                    continue;
                }
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        // cmp is the type's `>` (greatest) or `<` (least).
                        if call2(cmp, v.clone(), b.clone(), &a.ty(), ctx)? == Value::Bool(true) {
                            v
                        } else {
                            b
                        }
                    }
                });
            }
            let _ = greatest;
            Ok(best.unwrap_or(Value::Null))
        }
        Expr::NullIf(a, b, eq, _) => {
            let x = eval(a, row, ctx)?;
            let y = eval(b, row, ctx)?;
            if !x.is_null()
                && !y.is_null()
                && call2(eq, x.clone(), y, &a.ty(), ctx)? == Value::Bool(true)
            {
                return Ok(Value::Null);
            }
            Ok(x)
        }
        Expr::InList(e, list, negated) => {
            let x = eval(e, row, ctx)?;
            if x.is_null() {
                return Ok(Value::Null);
            }
            let mut saw_null = false;
            for (item, eq) in list {
                let y = eval(item, row, ctx)?;
                if y.is_null() {
                    saw_null = true;
                    continue;
                }
                if call2(eq, x.clone(), y, &e.ty(), ctx)? == Value::Bool(true) {
                    return Ok(Value::Bool(!*negated));
                }
            }
            Ok(if saw_null {
                Value::Null
            } else {
                Value::Bool(*negated)
            })
        }
        Expr::ArrayOp {
            left,
            right,
            op,
            all,
        } => {
            let x = eval(left, row, ctx)?;
            let arr = eval(right, row, ctx)?;
            if arr.is_null() {
                return Ok(Value::Null);
            }
            let Value::Array(a) = arr else {
                bail!(
                    DATATYPE_MISMATCH,
                    "op ANY/ALL (array) requires array on right side"
                );
            };
            array_op(&x, &a.items, op, &left.ty(), *all, ctx)
        }
        Expr::Subquery(s) => ctx.eval_subquery(s, row),
        Expr::Array(items, ty) => {
            let elem = ty.elem().cloned().unwrap_or(DataType::Text);
            let mut vals = Vec::with_capacity(items.len());
            for i in items {
                vals.push(eval(i, row, ctx)?);
            }
            build_array(elem, vals)
        }
        Expr::Row(items, _) => {
            let mut vals = Vec::with_capacity(items.len());
            for i in items {
                vals.push(eval(i, row, ctx)?);
            }
            Ok(Value::Record(Arc::new(vals)))
        }
        Expr::Subscript(base, idx, _) => {
            let v = eval(base, row, ctx)?;
            let Value::Array(a) = v else {
                return Ok(match v {
                    Value::Null => Value::Null,
                    Value::Jsonb(j) => {
                        // jsonb subscripting: j['key'] / j[0]
                        let mut cur = Some((*j).clone());
                        for (lo, _, _) in idx {
                            let k = match lo {
                                Some(e) => eval(e, row, ctx)?,
                                None => Value::Null,
                            };
                            cur = match (cur, k) {
                                (Some(c), Value::Text(s)) => c.get(&s).cloned(),
                                (Some(c), k) if k.as_i64().is_some() => {
                                    c.index(k.as_i64().unwrap()).cloned()
                                }
                                _ => None,
                            };
                        }
                        match cur {
                            Some(j) => Value::Jsonb(Arc::new(j)),
                            None => Value::Null,
                        }
                    }
                    Value::Geo(g) => {
                        // Fixed-length: one 0-based subscript, else NULL.
                        let [(Some(e), _, false)] = &idx[..] else {
                            return Ok(Value::Null);
                        };
                        match eval(e, row, ctx)?.as_i64() {
                            Some(i) => crate::types::geo::subscript(&g, i).unwrap_or(Value::Null),
                            None => Value::Null,
                        }
                    }
                    _ => bail!(
                        DATATYPE_MISMATCH,
                        "cannot subscript type because it does not support subscripting"
                    ),
                });
            };
            let mut bounds = Vec::with_capacity(idx.len());
            let mut any_slice = false;
            for (lo, hi, slice) in idx {
                let l = match lo {
                    Some(e) => match eval(e, row, ctx)? {
                        Value::Null => return Ok(Value::Null),
                        v => Some(v.as_i64().ok_or_else(|| {
                            err!(DATATYPE_MISMATCH, "array subscript must have type integer")
                        })?),
                    },
                    None => None,
                };
                let h = match hi {
                    Some(e) => match eval(e, row, ctx)? {
                        Value::Null => return Ok(Value::Null),
                        v => Some(v.as_i64().ok_or_else(|| {
                            err!(DATATYPE_MISMATCH, "array subscript must have type integer")
                        })?),
                    },
                    None => None,
                };
                any_slice |= *slice;
                bounds.push((l, h, *slice));
            }
            if any_slice {
                return Ok(array_slice(&a, &bounds));
            }
            let idx: Vec<i64> = bounds.iter().map(|b| b.0.unwrap_or(1)).collect();
            Ok(a.get(&idx))
        }
        Expr::Assign {
            base,
            subs,
            field,
            value,
            ty,
        } => {
            let cur = eval(base, row, ctx)?;
            let v = eval(value, row, ctx)?;
            if let Some(i) = field {
                let mut fields = match cur {
                    Value::Record(f) => (*f).clone(),
                    _ => vec![
                        Value::Null;
                        if let DataType::Row(f) = ty {
                            f.len()
                        } else {
                            i + 1
                        }
                    ],
                };
                fields[*i] = v;
                return Ok(Value::Record(Arc::new(fields)));
            }
            if matches!(ty, DataType::Jsonb) {
                return jsonb_assign(cur, subs, v, row, ctx);
            }
            let mut bounds = Vec::with_capacity(subs.len());
            for (lo, hi, slice) in subs {
                let int = |e: &Option<Expr>| -> Result<Option<i64>> {
                    match e {
                        Some(e) => match eval(e, row, ctx)? {
                            Value::Null => bail!(
                                NULL_VALUE_NOT_ALLOWED,
                                "array subscript in assignment must not be null"
                            ),
                            v => Ok(Some(v.as_i64().ok_or_else(|| {
                                err!(DATATYPE_MISMATCH, "array subscript must have type integer")
                            })?)),
                        },
                        None => Ok(None),
                    }
                };
                bounds.push((int(lo)?, int(hi)?, *slice));
            }
            if ty.fixed_elem().is_some() {
                let Value::Geo(g) = cur else { return Ok(cur) };
                if bounds.len() != 1 {
                    bail!(ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts");
                }
                // A NULL element leaves a fixed-length value as it was.
                if v.is_null() {
                    return Ok(Value::Geo(g));
                }
                return crate::types::geo::assign(&g, bounds[0].0.unwrap_or(0), &v);
            }
            let elem = match ty {
                DataType::Array(e) => (**e).clone(),
                _ => DataType::Unknown,
            };
            let cur = match cur {
                Value::Array(a) => (*a).clone(),
                _ => ArrayValue::empty(elem),
            };
            array_assign(cur, &bounds, v)
        }
        Expr::Field(e, i, _) => match eval(e, row, ctx)? {
            Value::Record(fields) => Ok(fields.get(*i).cloned().unwrap_or(Value::Null)),
            Value::Null => Ok(Value::Null),
            _ => bail!(
                WRONG_OBJECT_TYPE,
                "column notation .f applied to a non-composite value"
            ),
        },
        Expr::Agg(..) | Expr::Window(..) | Expr::Srf(..) | Expr::Grouping(_) => Err(
            crate::error::internal(format!("unplanned placeholder {e:?}")),
        ),
        Expr::Default(_) => Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "DEFAULT is not allowed in this context",
        )),
        Expr::CoerceToDomain(e, d) => {
            let v = eval(e, row, ctx)?;
            d.check(&v, ctx)?;
            Ok(v)
        }
    }
}

pub fn eval_bool(e: &Expr, row: &[Value], ctx: &ExecCtx) -> Result<bool> {
    Ok(truthy(&eval(e, row, ctx)?) == Some(true))
}

/// `x op ANY/ALL (items)` with SQL null semantics.
pub fn array_op(
    x: &Value,
    items: &[Value],
    op: &FuncDef,
    ty: &DataType,
    all: bool,
    ctx: &ExecCtx,
) -> Result<Value> {
    if items.is_empty() {
        return Ok(Value::Bool(all));
    }
    if x.is_null() {
        return Ok(Value::Null);
    }
    let mut saw_null = false;
    for y in items {
        if y.is_null() {
            saw_null = true;
            continue;
        }
        let r = call2(op, x.clone(), y.clone(), ty, ctx)?;
        match (r, all) {
            (Value::Bool(true), false) => return Ok(Value::Bool(true)),
            (Value::Bool(false), true) => return Ok(Value::Bool(false)),
            (Value::Null, _) => saw_null = true,
            _ => {}
        }
    }
    Ok(if saw_null {
        Value::Null
    } else {
        Value::Bool(all)
    })
}

/// ARRAY[...]: nested arrays of equal shape become a multi-dimensional array.
pub fn build_array(elem: DataType, vals: Vec<Value>) -> Result<Value> {
    if vals.is_empty() {
        return Ok(Value::Array(Arc::new(ArrayValue::empty(elem))));
    }
    let nested = vals.iter().any(|v| matches!(v, Value::Array(_)));
    // (int2vector and oidvector values are elements, not sub-arrays)
    if !nested || elem.is_array() || matches!(elem, DataType::Int2Vector | DataType::OidVector) {
        return Ok(Value::Array(Arc::new(ArrayValue::from_vec(elem, vals))));
    }
    // Multi-dimensional: all sub-arrays must have the same dimensions.
    let mut dims: Option<Vec<(usize, i32)>> = None;
    let mut items = Vec::new();
    for v in &vals {
        match v {
            Value::Array(a) => {
                match &dims {
                    None => dims = Some(a.dims.clone()),
                    Some(d) if *d != a.dims => {
                        bail!(
                            ARRAY_SUBSCRIPT_ERROR,
                            "multidimensional arrays must have array expressions with matching dimensions"
                        )
                    }
                    _ => {}
                }
                items.extend(a.items.iter().cloned());
            }
            Value::Null => {
                bail!(
                    ARRAY_SUBSCRIPT_ERROR,
                    "multidimensional arrays must have array expressions with matching dimensions"
                )
            }
            _ => bail!(
                DATATYPE_MISMATCH,
                "cannot mix arrays and scalars in ARRAY constructor"
            ),
        }
    }
    let inner = dims.unwrap_or_default();
    if inner.is_empty() {
        return Ok(Value::Array(Arc::new(ArrayValue::empty(elem))));
    }
    let mut all_dims = vec![(vals.len(), 1)];
    all_dims.extend(inner.iter().map(|(l, _)| (*l, 1)));
    Ok(Value::Array(Arc::new(ArrayValue {
        elem,
        dims: all_dims,
        items,
    })))
}

/// `j[...] = v` on jsonb (jsonb_subscript_assign).
fn jsonb_assign(
    cur: Value,
    subs: &[(Option<Expr>, Option<Expr>, bool)],
    v: Value,
    row: &[Value],
    ctx: &ExecCtx,
) -> Result<Value> {
    let mut path = Vec::with_capacity(subs.len());
    let mut expect_array = false;
    for (i, (s, _, _)) in subs.iter().enumerate() {
        let Some(s) = s else { continue };
        let x = eval(s, row, ctx)?;
        if x.is_null() {
            bail!(
                NULL_VALUE_NOT_ALLOWED,
                "jsonb subscript in assignment must not be null"
            );
        }
        if i == 0 {
            expect_array = x.as_i64().is_some() && !matches!(x, Value::Text(_));
        }
        path.push(match &x {
            Value::Text(t) => t.to_string(),
            other => other.as_i64().map(|n| n.to_string()).unwrap_or_default(),
        });
    }
    let value = match v {
        Value::Jsonb(j) => (*j).clone(),
        _ => crate::types::json::Jsonb::Null,
    };
    let root = match &cur {
        Value::Jsonb(j) => Some(&**j),
        _ => None,
    };
    let out = crate::types::json::set_element(root, &path, value, expect_array)?;
    Ok(Value::Jsonb(Arc::new(out)))
}

/// Postgres's MaxArraySize.
const MAX_ARRAY_ITEMS: i64 = 134_217_727;

/// `a[subs] = v`: replace an element or a slice. One-dimensional arrays grow (padding with
/// NULLs); others must already cover the target.
fn array_assign(
    mut a: ArrayValue,
    bounds: &[(Option<i64>, Option<i64>, bool)],
    v: Value,
) -> Result<Value> {
    let slice = bounds.iter().any(|b| b.2);
    // Per dimension: the target range [lo, hi].
    let mut ranges: Vec<(i64, i64)> = Vec::with_capacity(bounds.len());
    for (d, (l, h, s)) in bounds.iter().enumerate() {
        let r = if !slice {
            (l.unwrap(), l.unwrap())
        } else if !s {
            (1, l.unwrap())
        } else {
            let cur = a
                .dims
                .get(d)
                .map(|(len, lb)| (*lb as i64, *lb as i64 + *len as i64 - 1));
            match (l, h, cur) {
                (Some(l), Some(h), _) => (*l, *h),
                (l, h, Some((clo, chi))) => (l.unwrap_or(clo), h.unwrap_or(chi)),
                _ => {
                    return Err(Error::new(code::ARRAY_SUBSCRIPT_ERROR, "array slice subscript must provide both boundaries").detail(
                        "When assigning to a slice of an empty array value, slice boundaries must be fully specified.",
                    ));
                }
            }
        };
        if r.0 > r.1 {
            return Err(Error::new(
                code::ARRAY_SUBSCRIPT_ERROR,
                "upper bound cannot be less than lower bound",
            ));
        }
        ranges.push(r);
    }
    let src: Vec<Value> = if slice {
        match v {
            Value::Null => return Ok(Value::Array(Arc::new(a))),
            Value::Array(s) => s.items.clone(),
            other => vec![other],
        }
    } else {
        vec![v]
    };
    let count: i64 = ranges
        .iter()
        .map(|(l, h)| h - l + 1)
        .fold(1i64, |a, b| a.saturating_mul(b));
    if a.dims.is_empty() && count > MAX_ARRAY_ITEMS {
        return Err(Error::new(
            code::PROGRAM_LIMIT_EXCEEDED,
            format!("array size exceeds the maximum allowed ({MAX_ARRAY_ITEMS})"),
        ));
    }
    if (src.len() as i64) < count {
        return Err(Error::new(
            code::ARRAY_SUBSCRIPT_ERROR,
            "source array too small",
        ));
    }
    let out_of_range = || Error::new(code::ARRAY_SUBSCRIPT_ERROR, "array subscript out of range");
    if a.dims.is_empty() {
        let mut dims = Vec::with_capacity(ranges.len());
        for (l, h) in &ranges {
            let lb = i32::try_from(*l).map_err(|_| out_of_range())?;
            dims.push(((h - l + 1) as usize, lb));
        }
        a.dims = dims;
        a.items = src.into_iter().take(count as usize).collect();
        return Ok(Value::Array(Arc::new(a)));
    }
    if ranges.len() != a.dims.len() && !(slice && ranges.len() < a.dims.len()) {
        return Err(Error::new(
            code::ARRAY_SUBSCRIPT_ERROR,
            "wrong number of array subscripts",
        ));
    }
    for d in ranges.len()..a.dims.len() {
        let (len, lb) = a.dims[d];
        ranges.push((lb as i64, lb as i64 + len as i64 - 1));
    }
    if a.dims.len() == 1 {
        // Grow to cover the range, padding with NULLs.
        let (len, lb) = a.dims[0];
        let (lo, hi) = ranges[0];
        let (cur_lo, cur_hi) = (lb as i64, lb as i64 + len as i64 - 1);
        let new_lo = lo.min(cur_lo);
        let new_hi = hi.max(cur_hi);
        if new_hi - new_lo + 1 > MAX_ARRAY_ITEMS {
            return Err(Error::new(
                code::PROGRAM_LIMIT_EXCEEDED,
                format!("array size exceeds the maximum allowed ({MAX_ARRAY_ITEMS})"),
            ));
        }
        let mut items = vec![Value::Null; (new_hi - new_lo + 1) as usize];
        for (i, v) in a.items.drain(..).enumerate() {
            items[(cur_lo - new_lo) as usize + i] = v;
        }
        for (k, v) in src.into_iter().take(count as usize).enumerate() {
            items[(lo - new_lo) as usize + k] = v;
        }
        a.dims = vec![(
            items.len(),
            i32::try_from(new_lo).map_err(|_| out_of_range())?,
        )];
        a.items = items;
        return Ok(Value::Array(Arc::new(a)));
    }
    for ((l, h), (len, lb)) in ranges.iter().zip(&a.dims) {
        if *l < *lb as i64 || *h > *lb as i64 + *len as i64 - 1 {
            return Err(out_of_range());
        }
    }
    let strides: Vec<usize> = (0..a.dims.len())
        .map(|d| a.dims[d + 1..].iter().map(|x| x.0).product())
        .collect();
    let mut src = src.into_iter();
    fn rec(
        a: &mut ArrayValue,
        ranges: &[(i64, i64)],
        strides: &[usize],
        d: usize,
        off: usize,
        src: &mut std::vec::IntoIter<Value>,
    ) {
        let lb = a.dims[d].1 as i64;
        for i in ranges[d].0..=ranges[d].1 {
            let o = off + (i - lb) as usize * strides[d];
            if d + 1 == ranges.len() {
                a.items[o] = src.next().unwrap_or(Value::Null);
            } else {
                rec(a, ranges, strides, d + 1, o, src);
            }
        }
    }
    rec(&mut a, &ranges, &strides, 0, 0, &mut src);
    Ok(Value::Array(Arc::new(a)))
}

fn array_slice(a: &ArrayValue, bounds: &[(Option<i64>, Option<i64>, bool)]) -> Value {
    if a.dims.is_empty() || bounds.len() > a.dims.len() {
        return Value::Array(Arc::new(ArrayValue::empty(a.elem.clone())));
    }
    // Per dimension: clamp [lo, hi] to the array's bounds.
    let mut ranges = Vec::new();
    for (d, (len, lb)) in a.dims.iter().enumerate() {
        let (lo, hi) = match bounds.get(d) {
            Some((l, h, slice)) => {
                if *slice {
                    (
                        l.unwrap_or(*lb as i64),
                        h.unwrap_or(*lb as i64 + *len as i64 - 1),
                    )
                } else {
                    // A non-slice subscript in a slice expression means 1..n.
                    (1, l.unwrap_or(1))
                }
            }
            None => (*lb as i64, *lb as i64 + *len as i64 - 1),
        };
        let lo = lo.max(*lb as i64);
        let hi = hi.min(*lb as i64 + *len as i64 - 1);
        if lo > hi {
            return Value::Array(Arc::new(ArrayValue::empty(a.elem.clone())));
        }
        ranges.push(((lo - *lb as i64) as usize, (hi - *lb as i64) as usize));
    }
    let mut items = Vec::new();
    let strides: Vec<usize> = (0..a.dims.len())
        .map(|d| a.dims[d + 1..].iter().map(|x| x.0).product())
        .collect();
    fn rec(
        a: &ArrayValue,
        ranges: &[(usize, usize)],
        strides: &[usize],
        d: usize,
        off: usize,
        out: &mut Vec<Value>,
    ) {
        for i in ranges[d].0..=ranges[d].1 {
            let o = off + i * strides[d];
            if d + 1 == ranges.len() {
                out.push(a.items[o].clone());
            } else {
                rec(a, ranges, strides, d + 1, o, out);
            }
        }
    }
    rec(a, &ranges, &strides, 0, 0, &mut items);
    let dims = ranges.iter().map(|(l, h)| (h - l + 1, 1)).collect();
    Value::Array(Arc::new(ArrayValue {
        elem: a.elem.clone(),
        dims,
        items,
    }))
}

/// Whether a cast runs only immutable functions: Postgres's casts through stable I/O or
/// conversion functions (text to date, timestamp to timestamptz, an enum to text) aren't.
pub fn cast_immutable(from: &DataType, to: &DataType) -> bool {
    use DataType as T;
    if matches!(from, T::Unknown) || from.same_base(to) {
        return true;
    }
    let text = |t: &DataType| {
        matches!(
            t,
            T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name | T::Char
        )
    };
    // Types whose output (`out`) or input function is stable.
    let stable_out = |t: &DataType| {
        matches!(
            t,
            T::Date
                | T::Timestamp(_)
                | T::Timestamptz(_)
                | T::Interval(_)
                | T::Money
                | T::Range(_)
                | T::Multirange(_)
                | T::Enum(..)
                | T::Record
                | T::Row(_)
                | T::Array(_)
        )
    };
    let stable_in = |t: &DataType| {
        stable_out(t) || matches!(t, T::Time(_) | T::Timetz(_) | T::Xml | T::Regclass)
    };
    match (from, to) {
        (T::Array(a), T::Array(b)) => cast_immutable(a, b),
        (T::Int4 | T::Int8 | T::Numeric(_), T::Money) | (T::Money, T::Numeric(_)) => false,
        (T::Date | T::Timestamp(_), T::Timestamptz(_)) | (T::Time(_), T::Timetz(_)) => false,
        (T::Timestamptz(_), T::Date | T::Time(_) | T::Timestamp(_) | T::Timetz(_)) => false,
        (f, t) if text(t) && !text(f) => !stable_out(f),
        (f, t) if text(f) && !text(t) => !stable_in(t),
        _ => true,
    }
}

/// The named composite types `ROW(...)` expressions in `e` build, with their column types
/// as they are now (see `Constraint::row_types`).
pub fn named_row_types(e: &Expr) -> Vec<(u32, Vec<DataType>)> {
    let mut out: Vec<(u32, Vec<DataType>)> = Vec::new();
    e.walk(&mut |x| {
        if let Some(r) = x.named_row()
            && !out.iter().any(|(o, _)| *o == r.oid)
        {
            out.push((r.oid, r.fields.iter().map(|(_, t)| t.clone()).collect()));
        }
    });
    out
}

/// A stored check expression whose `ROW(...)` no longer fits its composite type (see
/// `check_row_types`): one failing as Postgres's does when it is first evaluated
/// (ExecRelCheck initializes the expression then, so a command checking no row is fine).
pub fn with_row_type_check(e: Expr, recorded: &[(u32, Vec<DataType>)]) -> Expr {
    match check_row_types(&e, recorded) {
        Ok(()) => e,
        Err(err) => raising(err, DataType::Bool),
    }
}

/// Whether `e` is a `raising` expression.
pub fn is_raising(e: &Expr) -> bool {
    matches!(e, Expr::Call(f, _, _) if f.oid == 0 && f.name == "raise_error")
}

/// A column of a multiple-column assignment's subquery (`SET (a, b) = (SELECT …)`): a field
/// of a scalar subquery returning several columns (a MULTIEXPR SubPlan, run once per row).
pub fn multi_assign_field(e: &Expr) -> Option<(&Subquery, usize)> {
    match e {
        Expr::Field(base, k, _) => match &**base {
            Expr::Subquery(sq)
                if sq.kind == SubqueryKind::Scalar
                    && sq.call.is_none()
                    && crate::sql::binder::plan_width(&sq.plan) > 1 =>
            {
                Some((sq, *k))
            }
            _ => None,
        },
        _ => None,
    }
}

/// What `get_rte_attribute_name` calls a column dropped since a stored query read it.
pub const DROPPED_COLUMN: &str = "?dropped?column?";

/// A stored view's reference to column `attnum` of a function's composite result, dropped
/// since, as printing the view sees it: `sibling` is another column of the same relation
/// (named `sibling_name`), which says how its relation's columns are qualified. Evaluated, it
/// fails as the view's execution does.
pub fn dropped_column(attnum: i16, sibling: Expr, sibling_name: &str, ty: DataType) -> Expr {
    fn raise(args: &[Value], _: &FnCtx) -> Result<Value> {
        let n = args.get(2).and_then(Value::as_str).unwrap_or_default().to_string();
        Err(Error::new(
            code::UNDEFINED_COLUMN,
            format!("attribute {n} of type record has been dropped"),
        ))
    }
    let def = FuncDef {
        oid: 0,
        name: DROPPED_COLUMN,
        args: vec![sibling.ty(), DataType::Text, DataType::Text],
        variadic: false,
        spread: false,
        ret: super::func::Ret::Fixed(ty.clone()),
        strict: false,
        volatility: 'v',
        imp: raise,
        min_args: 3,
        srf: None,
        out_cols: Vec::new(),
        arg_names: Vec::new(),
        defaults: Vec::new(),
        coll: None,
    };
    let args = vec![
        sibling,
        Expr::Const(Value::text(sibling_name.to_string()), DataType::Text),
        Expr::Const(Value::text(attnum.to_string()), DataType::Text),
    ];
    Expr::Call(Arc::new(def), args, ty)
}

/// `raising`, shown by EXPLAIN as `shown` (a dropped column a stored view still reads, by
/// its name there).
pub fn raising_shown(err: Error, ty: DataType, shown: Option<String>) -> Expr {
    let e = raising(err, ty);
    match (e, shown) {
        (Expr::Call(d, mut args, t), Some(n)) => {
            args.push(Expr::Const(Value::text(n), DataType::Text));
            Expr::Call(d, args, t)
        }
        (e, _) => e,
    }
}

/// An expression of type `ty` failing with `err` when it is evaluated (an error Postgres
/// raises when it initializes or runs the expression, not when it plans it).
pub fn raising(err: Error, ty: DataType) -> Expr {
    fn raise(args: &[Value], _: &FnCtx) -> Result<Value> {
        let text = |i: usize| args.get(i).and_then(Value::as_str).map(str::to_string);
        let code = match text(0).as_deref() {
            Some(code::UNDEFINED_COLUMN) => code::UNDEFINED_COLUMN,
            _ => code::DATATYPE_MISMATCH,
        };
        let mut e = Error::new(code, text(1).unwrap_or_default());
        if let Some(d) = text(2) {
            e = e.detail(d);
        }
        Err(e)
    }
    let def = FuncDef {
        oid: 0,
        name: "raise_error",
        args: vec![DataType::Text, DataType::Text, DataType::Text],
        variadic: false,
        spread: false,
        ret: super::func::Ret::Fixed(ty.clone()),
        strict: false,
        volatility: 'v',
        imp: raise,
        min_args: 3,
        srf: None,
        out_cols: Vec::new(),
        arg_names: Vec::new(),
        defaults: Vec::new(),
        coll: None,
    };
    let text = |s: Option<String>| match s {
        Some(s) => Expr::Const(Value::text(s), DataType::Text),
        None => Expr::Const(Value::Null, DataType::Text),
    };
    let args = vec![
        text(Some(err.code.to_string())),
        text(Some(err.message)),
        text(err.detail),
    ];
    Expr::Call(Arc::new(def), args, ty)
}

/// ExecInitExprRec's check of a RowExpr: its arguments (of the column types `recorded`
/// when the expression was stored) must still have the composite's column types.
pub fn check_row_types(e: &Expr, recorded: &[(u32, Vec<DataType>)]) -> crate::error::Result<()> {
    let mut bad = None;
    e.walk(&mut |x| {
        let Some(r) = x.named_row() else {
            return;
        };
        // (A column added or dropped since: the columns no longer line up, left alone.)
        let Some((_, was)) = recorded
            .iter()
            .find(|(o, f)| *o == r.oid && f.len() == r.fields.len())
        else {
            return;
        };
        for (old, (_, now)) in was.iter().zip(&r.fields) {
            if bad.is_none() && old.oid() != 0 && now.oid() != 0 && old.oid() != now.oid() {
                bad = Some(format!(
                    "ROW() column has type {} instead of type {}",
                    old.without_typmod().display(),
                    now.without_typmod().display()
                ));
            }
        }
    });
    match bad {
        Some(m) => Err(crate::error::Error::new(crate::error::code::DATATYPE_MISMATCH, m)),
        None => Ok(()),
    }
}
