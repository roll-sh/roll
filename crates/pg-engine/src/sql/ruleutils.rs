//! Expression deparsing the way Postgres's ruleutils.c prints stored trees (`pg_get_expr`,
//! `pg_get_constraintdef`, `pg_get_indexdef`). Introspection tools parse this text, so the
//! constant labels, cast display and parenthesisation follow `get_rule_expr` exactly.
//!
//! The input is an expression bound with `Binder::deparse` set, so casts and calls are kept as
//! written. Anything without a faithful rendering returns `None` and callers fall back to the
//! stored source text.

use super::expr::{BoolTest, Expr, XmlExpr, XmlOp};
use super::plan::{AggCall, SortKey};
use crate::catalog::{Catalog, quote_ident};
use crate::exec::ExecCtx;
use crate::types::text::format_value;
use crate::types::{DataType, Value};

mod query;

const INDENT: i32 = 4;

thread_local! {
    /// `print_function_sqlbody`: the function's name and argument names, as its parameters
    /// print.
    static FN_ARGS: std::cell::RefCell<Option<(String, Vec<Option<String>>)>> = const { std::cell::RefCell::new(None) };
    /// Query levels with range tables around what prints (named parameters are qualified
    /// inside one).
    static RTABLES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// No `PRETTYFLAG_INDENT`: keywords don't start new lines.
    static NO_INDENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The flags not at rest (`scoped::unsettled_for_test`).
pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    if FN_ARGS.with_borrow(|a| a.is_some()) {
        out.push("ruleutils::FN_ARGS");
    }
    if RTABLES.get() != 0 {
        out.push("ruleutils::RTABLES");
    }
    if NO_INDENT.get() {
        out.push("ruleutils::NO_INDENT");
    }
}

/// How parameter `$i+1` prints: by name in a SQL function body.
fn param_name(i: usize) -> String {
    let named = FN_ARGS.with_borrow(|a| {
        let (f, args) = a.as_ref()?;
        let n = args.get(i)?.as_deref().filter(|n| !n.is_empty())?;
        Some(if RTABLES.get() > 0 {
            format!("{}.{}", quote_ident(f), quote_ident(n))
        } else {
            quote_ident(n)
        })
    });
    named.unwrap_or_else(|| format!("${}", i + 1))
}

/// Deparse `e` whose `Col(i)` print as `cols[i]` (already quoted / qualified). `pretty` is the
/// functions' `pretty` argument; indentation is always on, as in Postgres.
pub fn deparse(e: &Expr, cols: &[String], ctx: &ExecCtx, pretty: bool) -> Option<String> {
    let cat = ctx.catalog();
    let mut d = Deparser {
        ctx,
        cat: &cat,
        cols,
        aggs: &[],
        pretty,
        indent: 0,
        buf: String::new(),
        outer: &[],
        subplans: None,
        view: None,
        nested: None,
    };
    d.expr(e, false)?;
    Some(d.buf)
}

/// How EXPLAIN shows a subquery: `SubPlan n` / `InitPlan n`, and whether it is hashed.
#[derive(Clone, Debug)]
pub struct SubplanLabel {
    pub name: String,
    pub init: bool,
    pub hashed: bool,
    /// A correlated EXISTS run as its hashed alternative (convert_EXISTS_to_ANY): the
    /// outer expressions and operators of the ANY it became.
    pub exists_any: Option<(Vec<Expr>, Vec<std::sync::Arc<crate::sql::func::FuncDef>>)>,
}

/// Deparse for EXPLAIN: `outer[l - 1]` names the columns of `Outer(l, _)`, and subqueries
/// print as their subplans.
pub fn deparse_explain(
    e: &Expr,
    cols: &[String],
    outer: &[Vec<String>],
    aggs: &[AggCall],
    subplans: &std::collections::HashMap<usize, SubplanLabel>,
    ctx: &ExecCtx,
) -> Option<String> {
    let cat = ctx.catalog();
    let mut d = Deparser {
        ctx,
        cat: &cat,
        cols,
        aggs,
        pretty: false,
        indent: 0,
        buf: String::new(),
        outer,
        subplans: Some(subplans),
        view: None,
        nested: None,
    };
    // (deparse_expression: no PRETTYFLAG_INDENT, CASE stays on one line.)
    {
        let _g = crate::scoped::set(&NO_INDENT, true, "ruleutils::NO_INDENT");
        d.expr(e, false)?;
    }
    Some(d.buf)
}

/// `qual.col` as printed (each part quoted when it needs to be): its parts.
fn split_qualified(s: &str) -> Option<(&str, &str)> {
    let at = if s.ends_with('"') {
        // The column part is quoted: its opening quote.
        let b = s.as_bytes();
        let mut i = b.len() - 1;
        loop {
            i = i.checked_sub(1)?;
            if b[i] == b'"' {
                if i > 0 && b[i - 1] == b'"' {
                    i -= 1;
                    continue;
                }
                break;
            }
        }
        i.checked_sub(1).filter(|&d| b[d] == b'.')?
    } else {
        s.rfind('.')?
    };
    Some((&s[..at], &s[at + 1..]))
}

/// An identifier as printed (maybe quoted): its name.
fn unquote_ident(s: &str) -> String {
    match s.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
        Some(x) => x.replace("\"\"", "\""),
        None => s.to_string(),
    }
}

/// Whether an index expression prints as a function call (no extra parentheses needed).
pub fn looks_like_function(e: &Expr) -> bool {
    match e {
        Expr::Call(def, ..) => !is_operator(def.name),
        Expr::NullIf(..) | Expr::SqlSyntax(_) | Expr::Coalesce(..) | Expr::MinMax(..) => true,
        _ => false,
    }
}

/// Postgres has cross-type integer and float operators (`int24eq`, `float48gt`, …), so the
/// widening casts our binder inserts for them don't appear in its trees.
fn cross_type_arg<'e>(op: &str, e: &'e Expr) -> &'e Expr {
    if !matches!(
        op,
        "=" | "<>" | "<" | "<=" | ">" | ">=" | "+" | "-" | "*" | "/"
    ) {
        return e;
    }
    match e {
        Expr::Cast(inner, from, to, false)
            if matches!(
                (from, to),
                (DataType::Int2, DataType::Int4 | DataType::Int8)
                    | (DataType::Int4, DataType::Int8)
                    | (DataType::Float4, DataType::Float8)
            ) =>
        {
            inner
        }
        e => e,
    }
}

impl Deparser<'_, '_> {
    fn builtin_op_name(&self, def: &crate::sql::func::FuncDef) -> &'static str {
        let name = def.name;
        if name.starts_with("OPERATOR(") || self.cat.operators.is_empty() || !(1..=2).contains(&def.args.len()) {
            return name;
        }
        // (Only the built-in itself: a CREATE OPERATOR over a built-in function keeps its name.)
        let res = match def.args.as_slice() {
            [l, r] => crate::sql::func::resolve_binop(name, l, r),
            [t] => crate::sql::func::resolve_prefix(name, t),
            _ => return name,
        };
        if !res.is_ok_and(|r| r.def.oid == def.oid) {
            return name;
        }
        let path = self.cat.routine_path(&self.ctx.search_path);
        let Some(pc) = path.iter().position(|&s| s == crate::catalog::PG_CATALOG_OID) else {
            return name;
        };
        let (left, right) = match def.args.as_slice() {
            [l, r] => (Some(l), r),
            [r] => (None, r),
            _ => return name,
        };
        let hidden = self.cat.operators.values().any(|o| {
            o.name == name
                && path.iter().position(|&s| s == o.schema).is_some_and(|i| i < pc)
                && o.left.as_ref().map(|t| t.oid()) == left.map(|t| t.oid())
                && o.right.same_base(right)
        });
        if hidden {
            crate::sql::func::intern(&format!("OPERATOR(pg_catalog.{name})"))
        } else {
            name
        }
    }
}

fn is_operator(name: &str) -> bool {
    !name.starts_with(|c: char| c.is_alphabetic() || c == '_') || name.starts_with("OPERATOR(")
}

/// SQL-standard value functions, printed as keywords.
fn sql_value_function(name: &str) -> Option<&'static str> {
    Some(match name {
        "current_date" => "CURRENT_DATE",
        "current_time" => "CURRENT_TIME",
        "current_timestamp" => "CURRENT_TIMESTAMP",
        "localtime" => "LOCALTIME",
        "localtimestamp" => "LOCALTIMESTAMP",
        "current_role" => "CURRENT_ROLE",
        "current_user" => "CURRENT_USER",
        "user" => "USER",
        "session_user" => "SESSION_USER",
        "current_catalog" => "CURRENT_CATALOG",
        "current_schema" => "CURRENT_SCHEMA",
        _ => return None,
    })
}

/// Node classes `isSimpleNode` distinguishes.
#[derive(Clone, Copy, PartialEq)]
enum Kind<'s> {
    /// Words, function-like syntax and CASE: never need parentheses.
    Simple,
    /// A binary operator, with its symbol.
    BinOp(&'s str),
    /// A prefix operator.
    PrefixOp,
    /// NullTest, BooleanTest, DistinctExpr.
    Test,
    Not,
    And,
    Or,
    /// ScalarArrayOpExpr and anything else Postgres doesn't list.
    Other,
}

/// Parent node classes, for the same decision.
#[derive(Clone, Copy, PartialEq)]
enum Parent<'s> {
    /// A binary operator; whether the child is its left operand.
    BinOp(&'s str, bool),
    /// A cast (FuncExpr / RelabelType / CoerceViaIO with a cast format).
    Cast,
    Not,
    And,
    Or,
    Other,
}

fn kind(e: &Expr) -> Kind<'_> {
    match e {
        Expr::Call(def, args, _) if is_operator(def.name) => {
            if args.len() == 1 {
                Kind::PrefixOp
            } else {
                Kind::BinOp(def.name)
            }
        }
        Expr::IsNull(..) | Expr::BoolTest(..) => Kind::Test,
        Expr::IsDistinct(l, r, not, _) => {
            // (A composite's null test is a test either way.)
            if *not
                && !(matches!(&**r, Expr::Const(Value::Null, _))
                    && matches!(l.ty(), DataType::Row(_) | DataType::Record))
            {
                Kind::Not
            } else {
                Kind::Test
            }
        }
        Expr::Not(_) => Kind::Not,
        Expr::And(_) => Kind::And,
        Expr::Or(_) => Kind::Or,
        Expr::InList(_, list, _) if list.len() == 1 => {
            Kind::BinOp(if matches!(e, Expr::InList(_, _, true)) {
                "<>"
            } else {
                "="
            })
        }
        Expr::InList(..) | Expr::ArrayOp { .. } => Kind::Other,
        Expr::Subquery(_) => Kind::Test,
        _ => Kind::Simple,
    }
}

fn is_simple(node: Kind, parent: Parent, pretty: bool) -> bool {
    // Boolean operators bind looser than anything below them.
    let separated = |p: Parent| matches!(p, Parent::And | Parent::Or | Parent::Not);
    match node {
        Kind::Simple => true,
        Kind::BinOp(op) if pretty && matches!(parent, Parent::BinOp(..)) => {
            let Parent::BinOp(pop, first) = parent else {
                unreachable!()
            };
            let prio = |o: &str| match o {
                "+" | "-" => Some(false),
                "*" | "/" | "%" => Some(true),
                _ => None,
            };
            let (Some(hi), Some(phi)) = (prio(op), prio(pop)) else {
                return false;
            };
            if hi && !phi {
                return true;
            }
            if !hi && phi {
                return false;
            }
            // Same priority: (a - b) - c needs none, a - (b - c) does.
            first
        }
        Kind::PrefixOp if matches!(parent, Parent::BinOp(..)) => false,
        Kind::BinOp(_) | Kind::PrefixOp | Kind::Test => separated(parent),
        Kind::Not | Kind::And | Kind::Or => match parent {
            Parent::Not | Parent::And | Parent::Or => {
                pretty
                    && match node {
                        Kind::Not | Kind::And => matches!(parent, Parent::And | Parent::Or),
                        _ => parent == Parent::Or,
                    }
            }
            _ => false,
        },
        Kind::Other => false,
    }
}

struct Deparser<'d, 'a> {
    ctx: &'d ExecCtx<'a>,
    cat: &'d Catalog,
    cols: &'d [String],
    /// Aggregates `Expr::Agg(i)` stands for.
    aggs: &'d [AggCall],
    pretty: bool,
    indent: i32,
    buf: String,
    /// EXPLAIN: names of enclosing queries' columns, innermost first.
    outer: &'d [Vec<String>],
    /// EXPLAIN: subquery id → its subplan.
    subplans: Option<&'d std::collections::HashMap<usize, SubplanLabel>>,
    /// A view's query level: its window functions and range-table names.
    view: Option<&'d query::ViewLevel>,
    /// A view's query printer, for its sublinks and FROM subqueries.
    nested: Option<&'d mut dyn query::Nested>,
}

impl Deparser<'_, '_> {
    /// The relation a whole-row reference reads (its range-table name), when `e` is one.
    fn whole_row(&self, e: &Expr) -> Option<String> {
        let Expr::Row(items, _) = e else {
            return None;
        };
        if !crate::sql::binder::is_whole_row(e) {
            return None;
        }
        // One of an enclosing query's relations (a LATERAL item's, a sublink's): its
        // columns as that query shows them share the relation's name.
        if let Some(Expr::Outer(l, ..)) = items.first() {
            let names = self.outer.get(l.checked_sub(1)?)?;
            let mut qual = None;
            for x in items {
                let Expr::Outer(m, j, _) = x else {
                    return None;
                };
                let (q, _) = split_qualified(names.get(*j)?)?;
                if m != l || qual.is_some_and(|p| p != q) {
                    return None;
                }
                qual = Some(q);
            }
            return qual.map(unquote_ident);
        }
        let pos: Vec<usize> = items
            .iter()
            .map(|x| match x {
                Expr::Col(i, _) => Some(*i),
                _ => None,
            })
            .collect::<Option<_>>()?;
        if let Some(level) = self.view
            && let Some((n, _)) = level.rows.iter().find(|(_, p)| *p == pos)
        {
            return Some(n.clone());
        }
        // EXPLAIN: a table's whole-row reference is one still (a view's became a ROW()).
        if self.subplans.is_some()
            && !matches!(e.ty(), DataType::Row(rt) if rt.oid != 0
                && self.cat.tables.values().any(|t| t.rowtype == rt.oid))
        {
            return None;
        }
        // A stored expression's whole-row reference to its relation.
        if self.subplans.is_none()
            && self.view.is_none()
            && let DataType::Row(rt) = e.ty()
            && rt.oid != 0
            && let Some(name) = STORED_TABLE.with(|s| {
                s.borrow()
                    .as_ref()
                    .filter(|(o, _)| *o == rt.oid)
                    .map(|(_, n)| n.clone())
            })
        {
            return Some(name);
        }
        // Else (OLD / NEW of a rule, an enclosing query's relation): its columns as shown
        // share the relation's name.
        let mut qual = None;
        for i in &pos {
            let (q, _) = split_qualified(self.cols.get(*i)?)?;
            if qual.is_some_and(|p| p != q) {
                return None;
            }
            qual = Some(q);
        }
        qual.map(unquote_ident)
    }

    /// A subquery as EXPLAIN shows it (ruleutils' SubPlan / InitPlan output).
    fn subplan(&mut self, sq: &crate::sql::expr::Subquery) -> Option<()> {
        use crate::sql::expr::SubqueryKind as K;
        let label = self.subplans?.get(&sq.id)?.clone();
        let reference = if label.init {
            format!("({}).col1", label.name)
        } else if label.hashed {
            format!("(hashed {}).col1", label.name)
        } else {
            format!("({})", label.name)
        };
        if let Some((left, ops)) = &label.exists_any {
            self.buf.push_str("(ANY (");
            for (i, (l, op)) in left.iter().zip(ops).enumerate() {
                if i > 0 {
                    self.buf.push_str(" AND ");
                }
                let r = reference.replace(").col1", &format!(").col{}", i + 1));
                self.paren(l, true, Parent::BinOp(op.name, true))?;
                self.buf.push_str(&format!(" {} {r}", op.name));
            }
            self.buf.push_str("))");
            return Some(());
        }
        // A user-defined operator's comparison the subquery computes: Postgres's test
        // expression, the operator's function inlined over the left operand and the
        // subquery's column.
        if sq.test_op.is_some()
            && matches!(sq.kind, K::Any | K::All)
            && let crate::sql::plan::Plan::Project { exprs, .. } = &*sq.plan
            && let [test] = &exprs[..]
        {
            let at = self.cols.len();
            let test = test
                .clone()
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Col(0, t) => Expr::Col(at, t),
                        x => x,
                    })
                })
                .ok()?
                .transform(&mut |x| {
                    Ok(match x {
                        Expr::Outer(1, k, t) => sq.params.get(k).cloned().unwrap_or(Expr::Outer(1, k, t)),
                        x => x,
                    })
                })
                .ok()?;
            let mut cols: Vec<String> = self.cols.to_vec();
            cols.push(if label.init || label.hashed {
                reference
            } else {
                format!("({}).col1", label.name)
            });
            let mut d = Deparser {
                ctx: self.ctx,
                cat: self.cat,
                cols: &cols,
                aggs: self.aggs,
                pretty: self.pretty,
                indent: self.indent,
                buf: String::new(),
                outer: self.outer,
                subplans: self.subplans,
                view: self.view,
                nested: None,
            };
            d.expr(&test, true)?;
            let text = d.buf;
            self.buf.push_str(if sq.kind == K::Any { "(ANY " } else { "(ALL " });
            self.buf.push_str(&text);
            self.buf.push(')');
            return Some(());
        }
        match sq.kind {
            K::Scalar => self.buf.push_str(&if label.init {
                reference
            } else {
                format!("({})", label.name)
            }),
            K::Array => self.buf.push_str(&if label.init {
                reference
            } else {
                format!("ARRAY({})", label.name)
            }),
            // ROWCOMPARE: its test expression, the subquery's columns `(SubPlan n).colk`
            // (parenthesized as a SubPlan's).
            K::RowCompare => {
                let op = sq.ops.first()?.name;
                let col = |i: usize| format!("({}).col{}", label.name, i + 1);
                if !label.init {
                    self.buf.push('(');
                }
                if matches!(op, "=" | "<>") || sq.left.len() == 1 {
                    let many = sq.left.len() > 1;
                    if many {
                        self.buf.push('(');
                    }
                    for (i, (l, o)) in sq.left.iter().zip(&sq.ops).enumerate() {
                        if i > 0 {
                            self.buf.push_str(if op == "=" { " AND " } else { " OR " });
                        }
                        self.buf.push('(');
                        self.paren(l, true, Parent::BinOp(o.name, true))?;
                        self.buf.push_str(&format!(" {} {})", o.name, col(i)));
                    }
                    if many {
                        self.buf.push(')');
                    }
                } else {
                    self.buf.push_str("(ROW(");
                    for (i, l) in sq.left.iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.expr(l, true)?;
                    }
                    self.buf.push_str(&format!(") {op} ROW("));
                    for i in 0..sq.left.len() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.buf.push_str(&col(i));
                    }
                    self.buf.push_str("))");
                }
                if !label.init {
                    self.buf.push(')');
                }
            }
            K::Exists if label.init => self.buf.push_str(&reference),
            K::Exists => self.buf.push_str(&format!("EXISTS({})", label.name)),
            K::Any | K::All => {
                self.buf.push_str(if sq.kind == K::Any {
                    "(ANY ("
                } else {
                    "(ALL ("
                });
                // (Several columns: each comparison parenthesized, ANDed.)
                let many = sq.left.len() > 1;
                for (i, (l, op)) in sq.left.iter().zip(&sq.ops).enumerate() {
                    if i > 0 {
                        self.buf.push_str(" AND ");
                    }
                    let r = if label.init || label.hashed {
                        reference.replace(").col1", &format!(").col{}", i + 1))
                    } else {
                        format!("({}).col{}", label.name, i + 1)
                    };
                    if many {
                        self.buf.push('(');
                    }
                    self.paren(l, true, Parent::BinOp(op.name, true))?;
                    self.buf.push_str(&format!(" {} {r}", op.name));
                    if many {
                        self.buf.push(')');
                    }
                }
                self.buf.push_str("))");
            }
        }
        Some(())
    }

    /// `appendContextKeyword`: a new line at the current indentation (plus `plus`).
    fn keyword(&mut self, s: &str, before: i32, after: i32, plus: i32) {
        query::keyword(&mut self.buf, &mut self.indent, s, before, after, plus);
    }

    /// `get_rule_expr_paren`.
    fn paren(&mut self, e: &Expr, showimplicit: bool, parent: Parent) -> Option<()> {
        // (A COLLATE clause is no simple node to isSimpleNode, unless a plan folded it
        // into its constant or relabeled the value.)
        let k = match e {
            Expr::Collate(..) if self.subplans.is_none() => Kind::Other,
            _ => kind(e),
        };
        let need = self.pretty && !is_simple(k, parent, self.pretty);
        if need {
            self.buf.push('(');
        }
        self.expr(e, showimplicit)?;
        if need {
            self.buf.push(')');
        }
        Some(())
    }

    /// `get_func_sql_syntax`: the functions a call in SQL-standard syntax shows that way.
    /// False for the others, which show as plain calls.
    fn sql_syntax(&mut self, name: &str, args: &[Expr], ret: &DataType) -> Option<bool> {
        if let Some(kw) = sql_value_function(name) {
            self.buf.push_str(kw);
            if !args.is_empty() {
                self.buf.push_str(&format!("({})", ret.typmod()));
            }
            return Some(true);
        }
        let word = |e: &Expr| match e {
            Expr::Const(Value::Text(s), DataType::Text) => Some(s.to_string()),
            _ => None,
        };
        let ty = |i: usize| args.get(i).map(Expr::ty);
        match (name, args) {
            ("timezone", [zone, v]) => {
                self.buf.push('(');
                self.paren(v, false, Parent::Cast)?;
                self.buf.push_str(" AT TIME ZONE ");
                self.paren(zone, false, Parent::Cast)?;
                self.buf.push(')');
            }
            ("timezone", [v]) => {
                self.buf.push('(');
                self.paren(v, false, Parent::Cast)?;
                self.buf.push_str(" AT LOCAL)");
            }
            ("overlaps", [a, b, c, d]) => {
                self.buf.push_str("((");
                self.expr(a, false)?;
                self.buf.push_str(", ");
                self.expr(b, false)?;
                self.buf.push_str(") OVERLAPS (");
                self.expr(c, false)?;
                self.buf.push_str(", ");
                self.expr(d, false)?;
                self.buf.push_str("))");
            }
            ("extract", [field, v]) => {
                self.buf.push_str("EXTRACT(");
                self.buf.push_str(&quote_ident(&word(field)?));
                self.buf.push_str(" FROM ");
                self.expr(v, false)?;
                self.buf.push(')');
            }
            ("is_normalized", [v, rest @ ..]) => {
                self.buf.push('(');
                self.paren(v, false, Parent::Cast)?;
                self.buf.push_str(" IS");
                if let [form] = rest {
                    self.buf.push(' ');
                    self.buf.push_str(&word(form)?);
                }
                self.buf.push_str(" NORMALIZED)");
            }
            ("pg_collation_for", [v]) => {
                self.buf.push_str("COLLATION FOR (");
                self.expr(v, false)?;
                self.buf.push(')');
            }
            ("xmlexists", [path, doc]) => {
                // Extra parentheses: the arguments are c_exprs.
                self.buf.push_str("XMLEXISTS((");
                self.expr(path, false)?;
                self.buf.push_str(") PASSING (");
                self.expr(doc, false)?;
                self.buf.push_str("))");
            }
            ("normalize", [v, rest @ ..]) => {
                self.buf.push_str("NORMALIZE(");
                self.expr(v, false)?;
                if let [form] = rest {
                    self.buf.push_str(", ");
                    self.buf.push_str(&word(form)?);
                }
                self.buf.push(')');
            }
            ("overlay", [v, placing, from, rest @ ..]) => {
                self.buf.push_str("OVERLAY(");
                self.expr(v, false)?;
                self.buf.push_str(" PLACING ");
                self.expr(placing, false)?;
                self.buf.push_str(" FROM ");
                self.expr(from, false)?;
                if let [count] = rest {
                    self.buf.push_str(" FOR ");
                    self.expr(count, false)?;
                }
                self.buf.push(')');
            }
            ("position", [s, sub]) => {
                self.buf.push_str("POSITION((");
                self.expr(sub, false)?;
                self.buf.push_str(") IN (");
                self.expr(s, false)?;
                self.buf.push_str("))");
            }
            ("substring", [v, from, rest @ ..])
                if ty(1) == Some(DataType::Int4) && rest.len() <= 1 =>
            {
                self.buf.push_str("SUBSTRING(");
                self.expr(v, false)?;
                self.buf.push_str(" FROM ");
                self.expr(from, false)?;
                if let [count] = rest {
                    self.buf.push_str(" FOR ");
                    self.expr(count, false)?;
                }
                self.buf.push(')');
            }
            ("substring", [v, pat, esc]) if ty(1) == Some(DataType::Text) => {
                self.buf.push_str("SUBSTRING(");
                self.expr(v, false)?;
                self.buf.push_str(" SIMILAR ");
                self.expr(pat, false)?;
                self.buf.push_str(" ESCAPE ");
                self.expr(esc, false)?;
                self.buf.push(')');
            }
            ("btrim" | "ltrim" | "rtrim", [v, rest @ ..]) if rest.len() <= 1 => {
                self.buf.push_str(match name {
                    "btrim" => "TRIM(BOTH",
                    "ltrim" => "TRIM(LEADING",
                    _ => "TRIM(TRAILING",
                });
                if let [chars] = rest {
                    self.buf.push(' ');
                    self.expr(chars, false)?;
                }
                self.buf.push_str(" FROM ");
                self.expr(v, false)?;
                self.buf.push(')');
            }
            ("system_user", []) => self.buf.push_str("SYSTEM_USER"),
            _ => return Some(false),
        }
        Some(true)
    }

    fn open(&mut self) {
        if !self.pretty {
            self.buf.push('(');
        }
    }

    fn close(&mut self) {
        if !self.pretty {
            self.buf.push(')');
        }
    }

    /// `get_rule_expr` for an `XmlExpr`.
    fn xml_expr(&mut self, x: &XmlExpr) -> Option<()> {
        use crate::types::xml::map_xml_name_to_sql_identifier as unmap;
        self.buf.push_str(match x.op {
            XmlOp::Concat => "XMLCONCAT(",
            XmlOp::Element => "XMLELEMENT(",
            XmlOp::Forest => "XMLFOREST(",
            XmlOp::Parse => "XMLPARSE(",
            XmlOp::Pi => "XMLPI(",
            XmlOp::Root => "XMLROOT(",
            XmlOp::Serialize => "XMLSERIALIZE(",
            XmlOp::IsDocument => "",
        });
        if matches!(x.op, XmlOp::Parse | XmlOp::Serialize) {
            self.buf
                .push_str(if x.document { "DOCUMENT " } else { "CONTENT " });
        }
        let mut comma = false;
        if let Some(n) = &x.name {
            self.buf.push_str("NAME ");
            self.buf.push_str(&quote_ident(&unmap(n)));
            comma = true;
        }
        if !x.named_args.is_empty() {
            if x.op != XmlOp::Forest {
                if comma {
                    self.buf.push_str(", ");
                }
                self.buf.push_str("XMLATTRIBUTES(");
                comma = false;
            }
            for (e, n) in x.named_args.iter().zip(&x.arg_names) {
                if comma {
                    self.buf.push_str(", ");
                }
                self.expr(e, true)?;
                self.buf.push_str(" AS ");
                self.buf.push_str(&quote_ident(&unmap(n)));
                comma = true;
            }
            if x.op != XmlOp::Forest {
                self.buf.push(')');
            }
        }
        if !x.args.is_empty() {
            if comma {
                self.buf.push_str(", ");
            }
            // A NULL constant, possibly coerced.
            let null_const = |e: &Expr| match e {
                Expr::Const(v, _) => v.is_null(),
                Expr::Cast(inner, ..) => matches!(&**inner, Expr::Const(v, _) if v.is_null()),
                _ => false,
            };
            match x.op {
                XmlOp::Parse => {
                    self.expr(&x.args[0], true)?;
                    let preserve = matches!(x.args.get(1), Some(Expr::Const(Value::Bool(true), _)));
                    self.buf.push_str(if preserve {
                        " PRESERVE WHITESPACE"
                    } else {
                        " STRIP WHITESPACE"
                    });
                }
                XmlOp::Root => {
                    self.expr(&x.args[0], true)?;
                    self.buf.push_str(", VERSION ");
                    if null_const(&x.args[1]) {
                        self.buf.push_str("NO VALUE");
                    } else {
                        self.expr(&x.args[1], false)?;
                    }
                    match x.args.get(2) {
                        Some(Expr::Const(Value::Int4(0), _)) => {
                            self.buf.push_str(", STANDALONE YES")
                        }
                        Some(Expr::Const(Value::Int4(1), _)) => {
                            self.buf.push_str(", STANDALONE NO")
                        }
                        Some(Expr::Const(Value::Int4(2), _)) => {
                            self.buf.push_str(", STANDALONE NO VALUE")
                        }
                        _ => {}
                    }
                }
                XmlOp::IsDocument => {
                    if self.pretty {
                        self.buf.push('(');
                    }
                    for (i, a) in x.args.iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.expr(a, false)?;
                    }
                    if self.pretty {
                        self.buf.push(')');
                    }
                }
                _ => self.list(&x.args)?,
            }
        }
        if x.op == XmlOp::Serialize {
            self.buf.push_str(" AS ");
            self.buf.push_str(&self.type_name(&x.target));
            self.buf
                .push_str(if x.indent { " INDENT" } else { " NO INDENT" });
        }
        if x.op == XmlOp::IsDocument {
            self.buf.push_str(" IS DOCUMENT");
        } else {
            self.buf.push(')');
        }
        Some(())
    }

    fn list(&mut self, items: &[Expr]) -> Option<()> {
        for (i, a) in items.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.expr(a, true)?;
        }
        Some(())
    }

    /// `get_rule_expr_toplevel`: a whole-row reference standing alone (a target, a ROW() or
    /// VALUES item) is cast to its row type, else `t.*` would read as its columns.
    fn expr_toplevel(&mut self, e: &Expr) -> Option<()> {
        self.expr_top(e, true)
    }

    /// `expr_toplevel`, showing implicit coercions or not.
    fn expr_top(&mut self, e: &Expr, showimplicit: bool) -> Option<()> {
        // An enclosing query's whole-row reference.
        if let Expr::Outer(l, i, ty) = e
            && let Some(name) = self.outer.get(l.checked_sub(1)?).and_then(|n| n.get(*i))
            && name.ends_with(".*")
        {
            let ty = match ty {
                t @ DataType::Row(_) if t.row_oid().is_some() => self.type_name(t),
                _ => "record".to_string(),
            };
            self.buf.push_str(&format!("{name}::{ty}"));
            return Some(());
        }
        match self.whole_row(e) {
            Some(name) => {
                let ty = match e.ty() {
                    t @ DataType::Row(_) if t.row_oid().is_some() => self.type_name(&t),
                    _ => "record".to_string(),
                };
                self.buf.push_str(&format!("{}.*::{ty}", quote_ident(&name)));
                Some(())
            }
            None => self.expr(e, showimplicit),
        }
    }

    fn binop(&mut self, op: &str, l: &Expr, r: &Expr) -> Option<()> {
        let (l, r) = (cross_type_arg(op, l), cross_type_arg(op, r));
        self.open();
        self.paren(l, true, Parent::BinOp(op, true))?;
        self.buf.push(' ');
        self.buf.push_str(op);
        self.buf.push(' ');
        self.paren(r, true, Parent::BinOp(op, false))?;
        self.close();
        Some(())
    }

    /// `get_rule_expr`.
    fn expr(&mut self, e: &Expr, showimplicit: bool) -> Option<()> {
        if let Some(row) = crate::sql::binder::nullable_whole_row(e) {
            return self.expr(row, showimplicit);
        }
        match e {
            Expr::Const(v, t) => self.constant(v, t, 0),
            Expr::Col(i, _) => self.buf.push_str(self.cols.get(*i)?),
            // A column a stored view reads that has been dropped since: Postgres's name for
            // it, qualified as the other columns of its relation are.
            Expr::Call(d, args, _) if d.oid == 0 && d.name == crate::sql::expr::DROPPED_COLUMN => {
                let (Some(Expr::Col(i, _)), Some(Expr::Const(Value::Text(name), _))) =
                    (args.first(), args.get(1))
                else {
                    return None;
                };
                let shown = self.cols.get(*i)?;
                let own = quote_ident(name);
                let qual = shown.strip_suffix(&own)?;
                if !qual.is_empty() && !qual.ends_with('.') {
                    return None;
                }
                self.buf.push_str(qual);
                self.buf.push_str(&quote_ident(crate::sql::expr::DROPPED_COLUMN));
            }
            Expr::Agg(i, _) => self.aggregate(self.aggs.get(*i)?)?,
            Expr::Param(i, _) => self.buf.push_str(&param_name(*i)),
            // A JOIN USING column coerced to the merged type: shown by name.
            Expr::Cast(_, _, _, false) if self.view.is_some() && self.merged_column(e) => {}
            // JSON_ARRAYAGG / JSON_OBJECTAGG ... RETURNING a type other than json[b].
            Expr::Cast(inner, DataType::Json | DataType::Jsonb, to, _)
                if matches!(&**inner, Expr::Agg(i, _)
                    if self.aggs.get(*i).is_some_and(|a| a.json_ctor)) =>
            {
                let Expr::Agg(i, _) = &**inner else {
                    return None;
                };
                self.json_aggregate_as(self.aggs.get(*i)?, Some(to))?;
            }
            Expr::Cast(inner, from, to, explicit) => {
                // Our binder's implicit length coercions (unified CASE/COALESCE branches) have
                // no counterpart in Postgres's trees.
                if !*explicit && (!showimplicit || from.same_base(to)) {
                    return self.paren(inner, false, Parent::Cast);
                }
                // `get_coercion_expr`.
                if let Expr::Const(v, t) = &**inner
                    && t.same_base(to)
                    && t.typmod() < 0
                {
                    self.constant(v, t, -1);
                } else if *explicit
                    && matches!(&**inner, Expr::Row(..))
                    && self.whole_row(inner).is_none()
                {
                    // A RowExpr marked as cast (COERCE_EXPLICIT_CAST): `ROW(...)::t`.
                    self.expr(inner, showimplicit)?;
                } else {
                    self.open();
                    self.paren(inner, false, Parent::Cast)?;
                    self.close();
                }
                self.buf.push_str("::");
                self.buf.push_str(&self.type_name(to));
            }
            Expr::Relabel(inner, to) => {
                // Outside deparsing only a folded CASE keeps its type this way (and a
                // domain's checked value read as its base type, which shows).
                if (!showimplicit || self.subplans.is_some())
                    && !(self.subplans.is_some() && matches!(&**inner, Expr::CoerceToDomain(_, d) if !d.tag))
                {
                    return self.paren(inner, false, Parent::Cast);
                }
                self.open();
                self.paren(inner, false, Parent::Cast)?;
                self.close();
                self.buf.push_str("::");
                self.buf.push_str(&self.type_name(to));
            }
            // The planner folds a COLLATE into its constant, or relabels the value.
            Expr::Collate(inner, coll, _) if self.subplans.is_some() => match &**inner {
                Expr::Const(v, t) => {
                    self.constant(v, t, 0);
                    if t.default_collation() != Some(coll.as_str()) {
                        self.buf.push_str(" COLLATE ");
                        self.buf.push_str(&quote_ident(coll));
                    }
                }
                inner if showimplicit => {
                    self.open();
                    self.paren(inner, false, Parent::Cast)?;
                    self.close();
                    self.buf.push_str("::");
                    self.buf.push_str(&self.type_name(&inner.ty()));
                }
                inner => self.paren(inner, false, Parent::Cast)?,
            },
            Expr::Collate(inner, coll, _) => {
                self.open();
                self.paren(inner, showimplicit, Parent::Cast)?;
                self.buf.push_str(" COLLATE ");
                self.buf.push_str(&quote_ident(coll));
                self.close();
            }
            Expr::SqlSyntax(inner) => match &**inner {
                Expr::Call(def, args, ty) if self.sql_syntax(def.name, args, ty)? => {}
                inner => self.expr(inner, showimplicit)?,
            },
            // A binary-coercible cast (RelabelType): shown as written.
            Expr::Call(def, args, ty)
                if (def.name == crate::sql::binder::BINARY_COERCION
                    || def.name == crate::sql::binder::BINARY_COERCION_EXPLICIT)
                    && args.len() == 1 =>
            {
                let explicit = def.name == crate::sql::binder::BINARY_COERCION_EXPLICIT;
                return self.expr(
                    &Expr::Cast(Box::new(args[0].clone()), args[0].ty(), ty.clone(), explicit),
                    showimplicit,
                );
            }
            // A row comparison kept whole (RowCompareExpr).
            Expr::Call(def, args, _)
                if def.oid == 0
                    && is_operator(def.name)
                    && matches!(args.as_slice(), [Expr::Row(..), Expr::Row(..)]) =>
            {
                self.buf.push('(');
                for (k, side) in args.iter().enumerate() {
                    if k > 0 {
                        self.buf.push_str(&format!(" {} ", def.name));
                    }
                    let Expr::Row(items, _) = side else {
                        return None;
                    };
                    self.buf.push_str("ROW(");
                    for (i, a) in items.iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.expr_toplevel(a)?;
                    }
                    self.buf.push(')');
                }
                self.buf.push(')');
            }
            // A hash key that can't be hashed shows as the key.
            Expr::Call(def, args, _)
                if def.name == crate::sql::binder::INDETERMINATE_HASH
                    || def.name == crate::sql::binder::NONDETERMINISTIC_HASH =>
            {
                return self.expr(&args[0], showimplicit);
            }
            Expr::Call(def, args, ty) if crate::sql::func::sqljson::is_sqljson(def) => {
                self.sqljson(def.name, args, ty)?
            }
            Expr::Call(def, args, _) => {
                if is_operator(def.name) {
                    // generate_operator_name: a built-in one the search path wouldn't find
                    // by its name (a user operator of the same name and types earlier on
                    // the path) is qualified.
                    let shown = self.builtin_op_name(def);
                    let def = &std::sync::Arc::new(crate::sql::func::FuncDef { name: shown, ..(**def).clone() });
                    match args.as_slice() {
                        [a] => {
                            self.open();
                            self.buf.push_str(def.name);
                            self.buf.push(' ');
                            self.paren(a, true, Parent::Other)?;
                            self.close();
                        }
                        // anytextcat / textanycat are SQL functions (`$1::text || $2`) the
                        // planner inlines: a plan shows the other side cast to text.
                        [l, r]
                            if def.name == "||"
                                && self.subplans.is_some()
                                && def.args.iter().any(|t| matches!(t, DataType::AnyNonArray)) =>
                        {
                            let as_text = |x: &Expr| -> Expr {
                                if x.ty() == DataType::Text {
                                    return x.clone();
                                }
                                match x {
                                    Expr::Const(v, t) if !v.is_null() => Expr::Const(
                                        Value::text(format_value(v, t, self.ctx.fmt())),
                                        DataType::Text,
                                    ),
                                    x => Expr::Cast(
                                        Box::new(x.clone()),
                                        x.ty(),
                                        DataType::Text,
                                        true,
                                    ),
                                }
                            };
                            let (l, r) = (as_text(l), as_text(r));
                            self.binop(def.name, &l, &r)?
                        }
                        [l, r] => self.binop(def.name, l, r)?,
                        _ => return None,
                    }
                    return Some(());
                }
                let name = match self.cat.functions.get(&def.oid) {
                    Some(f) if f.name == def.name => {
                        self.cat.function_name(f, &self.ctx.search_path)
                    }
                    // (information_schema's own, qualified unless it is on the search path:
                    // generate_function_name.)
                    _ if crate::sql::func::monitor::INFORMATION_SCHEMA_FUNCS.contains(&def.name)
                        && !self.ctx.search_path.iter().any(|s| s == "information_schema") =>
                    {
                        format!("information_schema.{}", quote_ident(def.name))
                    }
                    _ => quote_ident(def.name),
                };
                self.buf.push_str(&name);
                self.buf.push('(');
                if def.spread
                    && let Some((last, rest)) = args.split_last()
                {
                    self.list(rest)?;
                    if !rest.is_empty() {
                        self.buf.push_str(", ");
                    }
                    self.buf.push_str("VARIADIC ");
                    self.expr(last, true)?;
                } else if self.cat.functions.contains_key(&def.oid)
                    && def.arg_names.len() == args.len()
                    && def.arg_names.iter().any(|n| !n.is_empty())
                {
                    // A call in named notation (`f(a, flag => x)`).
                    for (i, (a, n)) in args.iter().zip(&def.arg_names).enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        if !n.is_empty() {
                            self.buf.push_str(&format!("{} => ", quote_ident(n)));
                        }
                        self.expr(a, true)?;
                    }
                } else if self.subplans.is_some()
                    && !self.cat.functions.contains_key(&def.oid)
                    && args.len() < def.args.len()
                    && def.args.len() - args.len() <= def.defaults.len()
                {
                    // A plan's call has the defaults of the arguments left out
                    // (expand_function_arguments).
                    let from = def.defaults.len() - (def.args.len() - args.len());
                    let mut all = args.to_vec();
                    for (v, t) in def.defaults[from..].iter().zip(&def.args[args.len()..]) {
                        all.push(Expr::Const(v.clone(), t.clone()));
                    }
                    self.list(&all)?;
                } else {
                    self.list(args)?;
                }
                self.buf.push(')');
            }
            Expr::And(args) | Expr::Or(args) => {
                let (word, parent) = if matches!(e, Expr::And(_)) {
                    (" AND ", Parent::And)
                } else {
                    (" OR ", Parent::Or)
                };
                self.open();
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(word);
                    }
                    self.paren(a, false, parent)?;
                }
                self.close();
            }
            Expr::Not(a) => {
                self.open();
                self.buf.push_str("NOT ");
                self.paren(a, false, Parent::Not)?;
                self.close();
            }
            Expr::IsNull(a, not) => {
                self.open();
                self.paren(a, true, Parent::Other)?;
                self.buf
                    .push_str(if *not { " IS NOT NULL" } else { " IS NULL" });
                self.close();
            }
            Expr::BoolTest(a, t) => {
                self.open();
                self.paren(a, false, Parent::Other)?;
                self.buf.push_str(match t {
                    BoolTest::IsTrue => " IS TRUE",
                    BoolTest::IsNotTrue => " IS NOT TRUE",
                    BoolTest::IsFalse => " IS FALSE",
                    BoolTest::IsNotFalse => " IS NOT FALSE",
                    BoolTest::IsUnknown => " IS UNKNOWN",
                    BoolTest::IsNotUnknown => " IS NOT UNKNOWN",
                });
                self.close();
            }
            // A composite's null test (NullTest, argisrow false).
            Expr::IsDistinct(l, r, not, _)
                if matches!(&**r, Expr::Const(Value::Null, _))
                    && matches!(l.ty(), DataType::Row(_) | DataType::Record) =>
            {
                self.open();
                self.paren(l, true, Parent::Other)?;
                self.buf.push_str(if *not {
                    " IS NOT DISTINCT FROM NULL"
                } else {
                    " IS DISTINCT FROM NULL"
                });
                self.close();
            }
            Expr::IsDistinct(l, r, not, _) => {
                // IS NOT DISTINCT FROM is stored as NOT (… IS DISTINCT FROM …).
                if *not {
                    self.open();
                    self.buf.push_str("NOT ");
                }
                self.open();
                self.paren(l, true, Parent::Other)?;
                self.buf.push_str(" IS DISTINCT FROM ");
                self.paren(r, true, Parent::Other)?;
                self.close();
                if *not {
                    self.close();
                }
            }
            Expr::InList(x, list, not) => {
                let op = if *not { "<>" } else { "=" };
                if let [(only, _)] = list.as_slice() {
                    return self.binop(op, x, only);
                }
                self.open();
                self.paren(cross_type_arg(op, x), true, Parent::Other)?;
                self.buf.push_str(&format!(
                    " {op} {} (ARRAY[",
                    if *not { "ALL" } else { "ANY" }
                ));
                for (i, (a, _)) in list.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    self.expr(a, true)?;
                }
                self.buf.push_str("])");
                self.close();
            }
            Expr::ArrayOp {
                left,
                right,
                op,
                all,
            } => {
                self.open();
                self.paren(cross_type_arg(op.name, left), true, Parent::Other)?;
                self.buf.push_str(&format!(
                    " {} {} (",
                    op.name,
                    if *all { "ALL" } else { "ANY" }
                ));
                self.paren(right, true, Parent::Other)?;
                // A bare scalar sub-SELECT would read back as an ANY/ALL sublink: Postgres
                // adds a dummy coercion. (A plan's SubPlan or InitPlan Param isn't one.)
                if self.subplans.is_none()
                    && let Expr::Subquery(sq) = &**right
                    && matches!(sq.kind, crate::sql::expr::SubqueryKind::Scalar)
                {
                    let ty = self.type_name(&sq.ty);
                    self.buf.push_str("::");
                    self.buf.push_str(&ty);
                }
                self.buf.push(')');
                self.close();
            }
            Expr::Case {
                operand,
                whens,
                else_,
                ty,
            } => {
                self.keyword("CASE", 0, INDENT, 0);
                if let Some(o) = operand {
                    self.buf.push(' ');
                    self.expr(o, true)?;
                }
                let space = |d: &mut Self| {
                    if NO_INDENT.get() {
                        d.buf.push(' ');
                    }
                };
                for (w, r) in whens {
                    space(self);
                    self.keyword("WHEN ", 0, 0, 0);
                    match (operand, w) {
                        // Only the right side of the `CASE x WHEN v` comparison.
                        (Some(_), Expr::Call(_, args, _))
                            if args.len() == 2 && matches!(args[0], Expr::CaseTest(_)) =>
                        {
                            self.expr(&args[1], false)?
                        }
                        (Some(_), _) => return None,
                        (None, w) => self.expr(w, false)?,
                    }
                    self.buf.push_str(" THEN ");
                    self.expr(r, true)?;
                }
                space(self);
                self.keyword("ELSE ", 0, 0, 0);
                match else_ {
                    Some(x) => self.expr(x, true)?,
                    None => self.constant(&Value::Null, ty, 0),
                }
                space(self);
                self.keyword("END", -INDENT, 0, 0);
            }
            Expr::Coalesce(..) if self.merged_column(e) => {}
            Expr::Xml(x) => self.xml_expr(x)?,
            Expr::RowCompare(rc) => {
                let (ls, rs) = rc.sides();
                self.buf.push_str("(ROW(");
                for (i, a) in ls.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    self.expr(a, true)?;
                }
                self.buf.push_str(&format!(") {} ROW(", rc.op));
                for (i, a) in rs.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    self.expr(a, true)?;
                }
                self.buf.push_str("))");
            }
            Expr::Coalesce(args, _) => {
                self.buf.push_str("COALESCE(");
                self.list(args)?;
                self.buf.push(')');
            }
            Expr::MinMax(args, greatest, _, _) => {
                self.buf
                    .push_str(if *greatest { "GREATEST(" } else { "LEAST(" });
                self.list(args)?;
                self.buf.push(')');
            }
            Expr::NullIf(a, b, _, _) => {
                self.buf.push_str("NULLIF(");
                self.expr(a, true)?;
                self.buf.push_str(", ");
                self.expr(b, true)?;
                self.buf.push(')');
            }
            Expr::Array(items, ty) => {
                self.buf.push_str("ARRAY");
                self.array_items(items)?;
                // An empty ARRAY[] needs its type spelled out.
                if items.is_empty() {
                    self.buf.push_str("::");
                    self.buf.push_str(&self.type_name(ty));
                }
            }
            // (The planner's eval_const_expressions makes a ROW() of constants one.)
            Expr::Row(_, ty) if self.subplans.is_some() && const_row(e).is_some() => {
                self.constant(&const_row(e)?, ty, 0);
            }
            Expr::Row(items, ty) => match self.whole_row(e) {
                // (A partitioned table's whole row in a plan: its partition's row converted
                // to the parent's type, a ConvertRowtypeExpr.)
                Some(name)
                    if self.subplans.is_some()
                        && matches!(ty, DataType::Row(rt) if rt.oid != 0
                            && self.cat.tables.values().any(|t| t.rowtype == rt.oid && t.partkey.is_some())) =>
                {
                    let tn = self.type_name(ty);
                    self.buf.push_str(&format!("(({}.*)::{tn})", quote_ident(&name)));
                }
                Some(name) => {
                    self.buf.push_str(&quote_ident(&name));
                    self.buf.push_str(".*");
                }
                None => {
                    self.buf.push_str("ROW(");
                    for (i, a) in items.iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.expr_toplevel(a)?;
                    }
                    self.buf.push(')');
                }
            },
            Expr::Subscript(base, idx, _) => {
                let bare = matches!(**base, Expr::Col(..) | Expr::Field(..));
                if !bare {
                    self.buf.push('(');
                }
                self.expr(base, true)?;
                if !bare {
                    self.buf.push(')');
                }
                for (lo, hi, slice) in idx {
                    self.buf.push('[');
                    if let Some(lo) = lo {
                        self.expr(lo, true)?;
                    }
                    if *slice {
                        self.buf.push(':');
                        if let Some(hi) = hi {
                            self.expr(hi, true)?;
                        }
                    }
                    self.buf.push(']');
                }
            }
            // A multiple-column assignment's subquery's column (EXPLAIN): `(SubPlan n).colk`.
            Expr::Field(_, i, _)
                if self.subplans.is_some() && crate::sql::expr::multi_assign_field(e).is_some() =>
            {
                let (sq, _) = crate::sql::expr::multi_assign_field(e)?;
                let label = self.subplans?.get(&sq.id)?;
                self.buf.push_str(&format!("({}).col{}", label.name, i + 1));
            }
            Expr::Field(base, i, _) => {
                let DataType::Row(fields) = base.ty() else {
                    return None;
                };
                let name = quote_ident(&fields.get(*i)?.0);
                let bare = matches!(**base, Expr::Subscript(..) | Expr::Field(..));
                if !bare {
                    self.buf.push('(');
                }
                self.expr(base, true)?;
                if !bare {
                    self.buf.push(')');
                }
                self.buf.push('.');
                self.buf.push_str(&name);
            }
            // An assignment SubscriptingRef / FieldStore (an UPDATE's new value): the
            // container (in parentheses unless a column), its subscripts or field, those of
            // the assignments nested in the value (processIndirection), ` := `, the value.
            Expr::Assign {
                base,
                subs,
                field,
                value,
                ..
            } => {
                let bare = matches!(**base, Expr::Col(..) | Expr::Outer(..) | Expr::Field(..));
                if !bare {
                    self.buf.push('(');
                }
                self.expr(base, true)?;
                if !bare {
                    self.buf.push(')');
                }
                let mut at: (&Expr, &[(Option<Expr>, Option<Expr>, bool)], Option<usize>, &Expr) =
                    (base, subs, *field, value);
                loop {
                    let (b, ss, f, v) = at;
                    for (lo, hi, slice) in ss {
                        self.buf.push('[');
                        if let Some(lo) = lo {
                            self.expr(lo, true)?;
                        }
                        if *slice {
                            self.buf.push(':');
                            if let Some(hi) = hi {
                                self.expr(hi, true)?;
                            }
                        }
                        self.buf.push(']');
                    }
                    if let Some(f) = f {
                        let DataType::Row(fields) = b.ty() else {
                            return None;
                        };
                        self.buf.push('.');
                        self.buf.push_str(&quote_ident(&fields.get(f)?.0));
                    }
                    match v {
                        Expr::Assign {
                            base, subs, field, value, ..
                        } => at = (base, subs, *field, value),
                        v => {
                            self.buf.push_str(" := ");
                            self.expr(v, true)?;
                            break;
                        }
                    }
                }
            }
            Expr::Default(_) => self.buf.push_str("DEFAULT"),
            Expr::CoerceToDomain(inner, d) if d.tag => self.expr(inner, showimplicit)?,
            Expr::CoerceToDomain(inner, d) => {
                self.open();
                self.paren(inner, false, Parent::Cast)?;
                self.close();
                self.buf.push_str("::");
                let name = match self.cat.domain_type(d.oid) {
                    Some((x, array)) => format!(
                        "{}{}",
                        self.cat
                            .qualified_name(x.schema, &x.name, &self.ctx.search_path),
                        if array { "[]" } else { "" }
                    ),
                    None => quote_ident(&d.name),
                };
                self.buf.push_str(&name);
            }
            Expr::Outer(level, i, _) => self
                .buf
                .push_str(self.outer.get(level.checked_sub(1)?)?.get(*i)?),
            Expr::Subquery(sq) if self.subplans.is_none() => self.sublink(sq)?,
            Expr::Subquery(sq) => self.subplan(sq)?,
            Expr::Window(i, _) => self.window_func(*i)?,
            Expr::Grouping(pos) => self.grouping(pos)?,
            Expr::Srf(i, _) => self.srf(*i)?,
            _ => return None,
        }
        Some(())
    }

    /// `get_agg_expr`.
    /// A SQL/JSON value argument: `text FORMAT JSON` where it is read as json.
    fn json_value_arg(&mut self, e: &Expr) -> Option<()> {
        match e {
            // bytea FORMAT JSON [ENCODING …]: read through convert_from (a `name` encoding
            // was written, a `text` one is the default).
            Expr::Cast(inner, DataType::Text, DataType::Json, explicit)
                if matches!(&**inner, Expr::Call(d, a, _) if d.name == "convert_from" && a.len() == 2) =>
            {
                let explicit = *explicit;
                let Expr::Call(_, a, _) = &**inner else {
                    return None;
                };
                self.expr(&a[0], false)?;
                self.buf.push_str(" FORMAT JSON");
                // (The default, UTF8, isn't shown when not written: the window path's
                // explicit cast.)
                if let Expr::Const(Value::Text(enc), DataType::Name) = &a[1]
                    && (!explicit || !enc.eq_ignore_ascii_case("utf8"))
                {
                    self.buf.push_str(" ENCODING ");
                    self.buf.push_str(enc);
                }
            }
            Expr::Cast(inner, DataType::Text, DataType::Json, false) => {
                self.expr(inner, false)?;
                self.buf.push_str(" FORMAT JSON");
            }
            e => self.expr(e, false)?,
        }
        Some(())
    }

    /// An object key: a literal shows without its type.
    fn json_key(&mut self, e: &Expr) -> Option<()> {
        match e {
            Expr::Const(v, t) if matches!(t, DataType::Text) => self.constant(v, t, -1),
            e => self.expr(e, false)?,
        }
        Some(())
    }

    /// `get_json_constructor` / `get_json_expr` and IS JSON.
    fn sqljson(&mut self, name: &str, args: &[Expr], ty: &DataType) -> Option<()> {
        use crate::sql::func::sqljson::{self as sj, Behavior, FuncOpts, Op, Wrapper};
        let flags = |e: &Expr| match e {
            Expr::Const(Value::Int8(i), _) => *i,
            _ => 0,
        };
        let returning = |d: &mut Self| {
            d.buf.push_str(" RETURNING ");
            d.buf.push_str(&d.type_name(ty));
        };
        match name {
            "JSON_OBJECT" | "JSON_ARRAY" => {
                let f = flags(&args[0]);
                self.buf.push_str(name);
                self.buf.push('(');
                if name == "JSON_OBJECT" {
                    for (i, pair) in args[1..].chunks(2).enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.json_key(&pair[0])?;
                        self.buf.push_str(" : ");
                        self.json_value_arg(pair.get(1)?)?;
                    }
                    if f & sj::ABSENT_ON_NULL != 0 {
                        self.buf.push_str(" ABSENT ON NULL");
                    }
                } else {
                    for (i, a) in args[1..].iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.json_value_arg(a)?;
                    }
                    if f & sj::ABSENT_ON_NULL == 0 {
                        self.buf.push_str(" NULL ON NULL");
                    }
                }
                if f & sj::UNIQUE_KEYS != 0 {
                    self.buf.push_str(" WITH UNIQUE KEYS");
                }
                returning(self);
                self.buf.push(')');
            }
            "JSON" => {
                self.buf.push_str("JSON(");
                self.json_value_arg(&args[1])?;
                if flags(&args[0]) & sj::UNIQUE_KEYS != 0 {
                    self.buf.push_str(" WITH UNIQUE KEYS");
                }
                self.buf.push(')');
            }
            "JSON_SCALAR" => {
                self.buf.push_str("JSON_SCALAR(");
                self.expr(&args[0], false)?;
                self.buf.push(')');
            }
            "JSON_SERIALIZE" => {
                self.buf.push_str("JSON_SERIALIZE(");
                self.json_value_arg(&args[0])?;
                returning(self);
                self.buf.push(')');
            }
            "IS JSON" => {
                let f = flags(&args[0]);
                self.open();
                self.paren(&args[1], false, Parent::Other)?;
                self.buf.push_str(" IS JSON");
                self.buf.push_str(match f & 3 {
                    sj::IS_OBJECT => " OBJECT",
                    sj::IS_ARRAY => " ARRAY",
                    sj::IS_SCALAR => " SCALAR",
                    _ => "",
                });
                if f & 4 != 0 {
                    self.buf.push_str(" WITH UNIQUE KEYS");
                }
                self.close();
            }
            _ => {
                let o = FuncOpts::unpack(flags(&args[2]));
                self.buf.push_str(name);
                self.buf.push('(');
                match &args[0] {
                    // EXPLAIN shows the planned tree, where a constant context item is
                    // already jsonb.
                    Expr::Const(Value::Text(t) | Value::Json(t), _) if self.subplans.is_some() => {
                        let j = crate::types::json::parse_jsonb(t).ok()?;
                        self.constant(&Value::Jsonb(std::sync::Arc::new(j)), &DataType::Jsonb, 0);
                    }
                    e => self.json_value_arg(e)?,
                }
                self.buf.push_str(", ");
                match &args[1] {
                    Expr::Const(v, t) => self.constant(v, t, -1),
                    e => self.expr(e, false)?,
                }
                if let Expr::Const(Value::Array(names), _) = &args[5]
                    && !names.items.is_empty()
                {
                    self.buf.push_str(" PASSING ");
                    for (i, (n, v)) in names
                        .items
                        .iter()
                        .zip(&args[sj::FUNC_FIXED_ARGS..])
                        .enumerate()
                    {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.json_value_arg(v)?;
                        self.buf.push_str(" AS ");
                        self.buf
                            .push_str(&quote_ident(n.as_str().unwrap_or_default()));
                    }
                }
                if o.op != Op::Exists || !matches!(ty, DataType::Bool) {
                    returning(self);
                }
                if o.op == Op::Query {
                    self.buf.push_str(match o.wrapper {
                        Wrapper::Conditional => " WITH CONDITIONAL WRAPPER",
                        Wrapper::Unconditional => " WITH UNCONDITIONAL WRAPPER",
                        _ => " WITHOUT WRAPPER",
                    });
                    self.buf.push_str(if o.omit_quotes {
                        " OMIT QUOTES"
                    } else {
                        " KEEP QUOTES"
                    });
                }
                let default = FuncOpts::default_behavior(o.op);
                for (b, e, word) in [
                    (o.on_empty, &args[3], "EMPTY"),
                    (o.on_error, &args[4], "ERROR"),
                ] {
                    if b == default {
                        continue;
                    }
                    self.buf.push(' ');
                    self.buf.push_str(b.word());
                    if b == Behavior::Default {
                        self.buf.push(' ');
                        self.expr(e, false)?;
                    }
                    self.buf.push_str(" ON ");
                    self.buf.push_str(word);
                }
                self.buf.push(')');
            }
        }
        Some(())
    }

    fn aggregate(&mut self, a: &AggCall) -> Option<()> {
        // An ordered-set aggregate: its direct arguments (casts shown), then WITHIN GROUP.
        if !a.direct.is_empty() {
            if a.user.is_some() {
                self.buf.push_str(a.def.sig.name);
            } else {
                self.buf.push_str(&quote_ident(a.def.sig.name));
            }
            self.buf.push('(');
            self.list(&a.direct)?;
            self.buf.push_str(") WITHIN GROUP (ORDER BY ");
            if a.order_by.is_empty() {
                self.list(&a.args)?;
            }
            for (i, k) in a.order_by.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.expr(&k.expr, false)?;
                self.sort_suffix(k);
            }
            self.buf.push(')');
            if let Some(f) = &a.filter {
                self.buf.push_str(" FILTER (WHERE ");
                self.expr(f, false)?;
                self.buf.push(')');
            }
            return Some(());
        }
        if a.json_ctor {
            return self.json_aggregate(a);
        }
        if a.user.is_some() {
            // Quoted (and qualified) when bound.
            self.buf.push_str(a.def.sig.name);
        } else {
            self.buf.push_str(&quote_ident(a.def.sig.name));
        }
        self.buf.push('(');
        if a.distinct {
            self.buf.push_str("DISTINCT ");
        }
        if a.args.is_empty() {
            self.buf.push('*');
        } else {
            self.list(&a.args)?;
        }
        if !a.order_by.is_empty() {
            self.buf.push_str(" ORDER BY ");
            for (i, k) in a.order_by.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.expr(&k.expr, true)?;
                self.sort_suffix(k);
            }
        }
        self.buf.push(')');
        if let Some(f) = &a.filter {
            self.buf.push_str(" FILTER (WHERE ");
            self.expr(f, false)?;
            self.buf.push(')');
        }
        Some(())
    }

    /// JSON_ARRAYAGG / JSON_OBJECTAGG.
    fn json_aggregate(&mut self, a: &AggCall) -> Option<()> {
        self.json_aggregate_as(a, None)
    }

    /// JSON_ARRAYAGG / JSON_OBJECTAGG, RETURNING `ret` when its json is cast to that.
    fn json_aggregate_as(&mut self, a: &AggCall, ret: Option<&DataType>) -> Option<()> {
        let name = a.def.sig.name;
        let object = name.contains("object");
        self.buf.push_str(if object {
            "JSON_OBJECTAGG("
        } else {
            "JSON_ARRAYAGG("
        });
        if object {
            self.expr(a.args.first()?, false)?;
            self.buf.push_str(" : ");
            self.json_value_arg(a.args.get(1)?)?;
        } else {
            self.json_value_arg(a.args.first()?)?;
        }
        if !a.order_by.is_empty() {
            self.buf.push_str(" ORDER BY ");
            for (i, k) in a.order_by.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.expr(&k.expr, true)?;
                self.sort_suffix(k);
            }
        }
        let strict = name.ends_with("_strict");
        if object && strict {
            self.buf.push_str(" ABSENT ON NULL");
        } else if !object && !strict {
            self.buf.push_str(" NULL ON NULL");
        }
        if name.contains("_unique") {
            self.buf.push_str(" WITH UNIQUE KEYS");
        }
        match ret {
            Some(t) => {
                self.buf.push_str(" RETURNING ");
                self.buf.push_str(&self.type_name(t));
            }
            None => self.buf.push_str(if name.starts_with("jsonb") {
                " RETURNING jsonb"
            } else {
                " RETURNING json"
            }),
        }
        self.buf.push(')');
        if let Some(f) = &a.filter {
            self.buf.push_str(" FILTER (WHERE ");
            self.expr(f, false)?;
            self.buf.push(')');
        }
        Some(())
    }

    fn sort_suffix(&mut self, k: &SortKey) {
        // A non-default family's ordering operator (text_pattern_ops, record_image_ops).
        if let Some(op) = &k.using
            && op.coll.is_none()
            && matches!(op.name, "~<~" | "*<")
        {
            let shown = match (op.name, k.desc) {
                ("~<~", true) => "~>~",
                ("*<", true) => "*>",
                (n, _) => n,
            };
            self.buf.push_str(" USING ");
            self.buf.push_str(shown);
            self.buf.push_str(if k.nulls_first { " NULLS FIRST" } else { " NULLS LAST" });
            return;
        }
        if k.desc {
            self.buf.push_str(" DESC");
            if !k.nulls_first {
                self.buf.push_str(" NULLS LAST");
            }
        } else if k.nulls_first {
            self.buf.push_str(" NULLS FIRST");
        }
    }

    /// `get_rule_sortgroupclause`: GROUP BY / ORDER BY items. Constants get a type label and
    /// function-like expressions parentheses, so they can't be read as output-column references.
    fn sort_group_item(&mut self, e: &Expr) -> Option<()> {
        match e {
            Expr::Col(..) => self.expr(e, false),
            Expr::Const(v, t) => {
                self.constant(v, t, 1);
                Some(())
            }
            e => {
                let paren = self.pretty
                    || matches!(e, Expr::Call(d, ..) if !is_operator(d.name))
                    || matches!(e, Expr::Agg(..) | Expr::Window(..))
                    || matches!(e, Expr::SqlSyntax(c) if !matches!(&**c, Expr::Call(d, ..) if sql_value_function(d.name).is_some()));
                if paren {
                    self.buf.push('(');
                }
                self.expr(e, true)?;
                if paren {
                    self.buf.push(')');
                }
                Some(())
            }
        }
    }

    /// The elements of an ARRAY[] (nested ones are ArrayExprs too, so they print as
    /// `ARRAY[...]`).
    fn array_items(&mut self, items: &[Expr]) -> Option<()> {
        self.buf.push('[');
        self.list(items)?;
        self.buf.push(']');
        Some(())
    }

    /// `format_type_with_typemod`: like `display`, except that `bpchar` without a length
    /// stays `bpchar` (`character` alone would mean `character(1)`).
    fn type_name(&self, t: &DataType) -> String {
        match t {
            DataType::Bpchar(m) if *m < 0 => "bpchar".into(),
            // (`bit` alone would read back as bit(1).)
            DataType::Bit(m) if *m < 0 => "\"bit\"".into(),
            DataType::Array(e) if matches!(**e, DataType::Bit(m) if m < 0) => "\"bit\"[]".into(),
            DataType::Array(e) if matches!(**e, DataType::Bpchar(m) if m < 0) => "bpchar[]".into(),
            // (User types qualified unless visible, as format_type_be has them.)
            t => self.cat.format_type(t, None, &self.ctx.search_path),
        }
    }

    /// `get_const_expr`: `showtype` -1 never labels, 0 labels when the literal alone would be
    /// read as another type.
    fn constant(&mut self, v: &Value, t: &DataType, showtype: i32) {
        if v.is_null() {
            self.buf.push_str("NULL");
            if showtype >= 0 {
                self.buf.push_str("::");
                self.buf.push_str(&self.type_name(t));
            }
            return;
        }
        let text = format_value(v, t, self.ctx.fmt());
        let mut label = match t {
            DataType::Int4 => {
                if text.starts_with('-') {
                    self.buf.push_str(&format!("'{text}'"));
                    true
                } else {
                    self.buf.push_str(&text);
                    false
                }
            }
            DataType::Numeric(_) => {
                if text.starts_with(|c: char| c.is_ascii_digit()) && text.contains(['e', 'E', '.'])
                {
                    self.buf.push_str(&text);
                    false
                } else {
                    self.buf.push_str(&format!("'{text}'"));
                    true
                }
            }
            DataType::Bool => {
                self.buf.push_str(if matches!(v, Value::Bool(true)) {
                    "true"
                } else {
                    "false"
                });
                false
            }
            _ => {
                self.buf.push('\'');
                self.buf.push_str(&text.replace('\'', "''"));
                self.buf.push('\'');
                true
            }
        };
        if showtype < 0 {
            return;
        }
        match t {
            DataType::Bool | DataType::Unknown => label = false,
            DataType::Int4 => {}
            DataType::Numeric(m) => label |= *m >= 0,
            _ => label = true,
        }
        if label || showtype > 0 {
            self.buf.push_str("::");
            self.buf.push_str(&self.type_name(t));
        }
    }
}

/// What a stored expression is, for binding it back.
#[derive(Clone, Copy, PartialEq)]
pub enum Stored<'t> {
    /// A column default / generation expression: assignment-coerced to the column type.
    Default(&'t DataType),
    /// A CHECK constraint or index predicate.
    Condition,
    /// An index key or a bare `pg_get_expr` input.
    Plain,
}

/// Bind stored expression text over `table` for display; `None` when it can't be rendered
/// faithfully (callers show the stored text).
pub fn bind_stored(
    ctx: &ExecCtx,
    table: &std::sync::Arc<crate::catalog::Table>,
    node: &pg_query::protobuf::Node,
    kind: Stored,
) -> Option<Expr> {
    use super::binder::{Binder, Clause};
    let mut b = Binder::new(ctx, vec![]);
    b.deparse = true;
    let clause = match kind {
        Stored::Default(_) => Clause::Default,
        Stored::Condition => Clause::Check,
        Stored::Plain => Clause::IndexExpr,
    };
    let e = b.bind_over_table(table, node, clause).ok()?;
    match kind {
        Stored::Default(ty) => b.coerce_assign(e, ty, "").ok(),
        _ => Some(e),
    }
}

/// Stored expression text (or a comma-separated list, like `pg_index.indexprs`) over `table`,
/// printed the way Postgres prints its stored tree.
pub fn stored_expr(
    ctx: &ExecCtx,
    table: &std::sync::Arc<crate::catalog::Table>,
    sql: &str,
    kind: Stored,
    pretty: bool,
) -> Option<String> {
    let cols = column_names(table);
    let mut out = Vec::new();
    // (Its whole-row reference shows as `table.*`.)
    let prev = STORED_TABLE.replace(Some((table.rowtype, table.name.clone())));
    let _restore = Restore(prev);
    for node in super::binder::ast::parse_exprs(sql).ok()? {
        let plain = bind_stored(ctx, table, &node, kind).and_then(|e| deparse(&e, &cols, ctx, pretty));
        // With a subquery (a policy's), printed as the view printer prints one.
        out.push(match plain {
            Some(t) => t,
            None => query::stored_expr_query(ctx, table, &node, pretty)?,
        });
    }
    Some(out.join(", "))
}

thread_local! {
    /// The relation (row type, name) a stored expression being printed is over.
    static STORED_TABLE: std::cell::RefCell<Option<(u32, String)>> =
        const { std::cell::RefCell::new(None) };
}

/// Puts `STORED_TABLE` back when dropped.
struct Restore(Option<(u32, String)>);

impl Drop for Restore {
    fn drop(&mut self) {
        STORED_TABLE.set(self.0.take());
    }
}

/// The type of a stored expression (an index key, say), bound against `table`.
pub fn stored_expr_type(
    ctx: &ExecCtx,
    table: &std::sync::Arc<crate::catalog::Table>,
    sql: &str,
    kind: Stored,
) -> Option<crate::types::DataType> {
    let node = super::binder::ast::parse_exprs(sql)
        .ok()?
        .into_iter()
        .next()?;
    Some(bind_stored(ctx, table, &node, kind)?.ty())
}

/// A domain's CHECK expression as `pg_get_constraintdef` prints it (the value as VALUE).
pub fn domain_check(
    ctx: &ExecCtx,
    base: &crate::types::DataType,
    sql: &str,
    pretty: bool,
) -> Option<String> {
    let mut b = super::binder::Binder::new(ctx, vec![]);
    b.deparse = true;
    let node = super::binder::ast::parse_expr(sql).ok()?;
    let e = b.bind_domain_check(&node, base).ok()?;
    deparse(&e, &["VALUE".to_string()], ctx, pretty)
}

/// A trigger's WHEN condition as `pg_get_triggerdef` prints it (columns as `old.x` / `new.x`).
pub fn trigger_when(
    ctx: &ExecCtx,
    t: &std::sync::Arc<crate::catalog::Table>,
    sql: &str,
    pretty: bool,
) -> Option<String> {
    let mut b = super::binder::Binder::new(ctx, vec![]);
    b.deparse = true;
    let e = b.bind_trigger_when(t, sql).ok()?;
    let mut cols: Vec<String> = t
        .columns
        .iter()
        .map(|c| format!("old.{}", quote_ident(&c.name)))
        .collect();
    cols.extend(
        t.columns
            .iter()
            .map(|c| format!("new.{}", quote_ident(&c.name))),
    );
    deparse(&e, &cols, ctx, pretty)
}

pub fn column_names(t: &crate::catalog::Table) -> Vec<String> {
    t.columns.iter().map(|c| quote_ident(&c.name)).collect()
}

const STD: i32 = 8;
const JOIN: i32 = 4;

/// A view's definition as `pg_get_viewdef` prints it (`get_query_def`): one select-list item per
/// line, FROM / WHERE / GROUP BY … on their own lines. `None` for query shapes it doesn't cover.
pub fn view_def(ctx: &ExecCtx, v: &crate::catalog::ViewDef, pretty: bool) -> Option<String> {
    view_def_wrap(ctx, v, pretty, 0)
}

/// `view_def` wrapping select-list and FROM items only past column `wrap` (0: always).
pub fn view_def_wrap(
    ctx: &ExecCtx,
    v: &crate::catalog::ViewDef,
    pretty: bool,
    wrap: usize,
) -> Option<String> {
    query::view_query(
        ctx,
        &v.query,
        v.columns.iter().map(|c| c.0.clone()).collect(),
        pretty,
        wrap,
        (!v.rowtype_cols.is_empty()).then(|| std::sync::Arc::new(v.rowtype_cols.clone())),
    )
}

/// A view query's text as `pg_get_viewdef` prints it, `cols` being the view's column names.
pub fn query_def(ctx: &ExecCtx, sql: &str, cols: Vec<String>, pretty: bool) -> Option<String> {
    query::view_query(ctx, sql, cols, pretty, 0, None)
}

/// `pg_get_ruledef` of a CREATE RULE rule (`make_ruledef`). `None` for actions it can't print.
pub fn rule_def(ctx: &ExecCtx, r: &crate::catalog::Rule, pretty: bool) -> Option<String> {
    query::rule_def(ctx, r, pretty)
}

/// `pg_get_function_sqlbody`: a SQL-standard function body (`RETURN …` / `BEGIN ATOMIC … END`).
pub fn function_body(ctx: &ExecCtx, f: &crate::catalog::Function) -> Option<String> {
    query::function_body(ctx, f)
}

/// `get_json_table`: a JSON_TABLE call's text. `show` renders its expressions; `pretty`, the
/// indentation level it starts at when laid out over lines; `planned`, EXPLAIN's view (a
/// constant context item is already jsonb).
pub(crate) fn json_table_text(
    call: &super::plan::SrfCall,
    show: &mut dyn FnMut(&Expr) -> Option<String>,
    type_name: &dyn Fn(&DataType) -> String,
    pretty: Option<i32>,
    planned: bool,
) -> Option<String> {
    use crate::sql::func::sqljson::{Behavior, FuncOpts, Op, PathSpec, TableSpec, Wrapper};
    let Expr::Const(Value::Text(spec), _) = call.args.get(1)? else {
        return None;
    };
    let spec: TableSpec = serde_json::from_str(spec).ok()?;
    let lit = |path: &str| {
        let p = crate::types::jsonpath::normalize(path).unwrap_or_else(|_| path.to_string());
        format!("'{}'", p.replace('\'', "''"))
    };
    struct Out<'s> {
        buf: String,
        pretty: Option<i32>,
        level: i32,
        show: &'s mut dyn FnMut(&Expr) -> Option<String>,
    }
    impl Out<'_> {
        /// appendContextKeyword: a new line at the current indentation when pretty.
        fn kw(&mut self, s: &str) {
            if self.pretty.is_some() {
                while self.buf.ends_with(' ') {
                    self.buf.pop();
                }
                self.buf.push('\n');
                self.buf.push_str(&" ".repeat(self.level.max(0) as usize));
            }
            self.buf.push_str(s);
        }
    }
    let mut o = Out {
        buf: String::from("JSON_TABLE("),
        pretty,
        level: pretty.unwrap_or(0) + 4,
        show,
    };
    o.kw("");
    let ctx = match &call.args[0] {
        Expr::Const(Value::Text(t) | Value::Json(t), _) if planned => {
            let j = crate::types::json::parse_jsonb(t).ok()?;
            format!("'{}'::jsonb", j.to_string().replace('\'', "''"))
        }
        e => (o.show)(e)?,
    };
    o.buf.push_str(&ctx);
    o.buf.push_str(", ");
    o.buf.push_str(&lit(&spec.root.path));
    o.buf
        .push_str(&format!(" AS {}", quote_ident(&spec.root.name)));
    if let Expr::Const(Value::Array(names), _) = &call.args[2]
        && !names.items.is_empty()
    {
        o.buf.push(' ');
        o.kw("PASSING ");
        o.level += 4;
        for (i, n) in names.items.iter().enumerate() {
            if i > 0 {
                o.buf.push_str(", ");
            }
            o.kw("");
            let v = (o.show)(call.args.get(3 + i)?)?;
            o.buf.push_str(&format!(
                "{v} AS {}",
                quote_ident(n.as_str().unwrap_or_default())
            ));
        }
        o.level -= 4;
    }
    fn columns(
        o: &mut Out,
        p: &PathSpec,
        call: &super::plan::SrfCall,
        type_name: &dyn Fn(&DataType) -> String,
        lit: &dyn Fn(&str) -> String,
    ) -> Option<()> {
        o.buf.push(' ');
        o.kw("COLUMNS (");
        o.level += 4;
        for (i, c) in p.columns.iter().enumerate() {
            if i > 0 {
                o.buf.push(',');
            }
            if o.pretty.is_some() {
                o.kw("");
            } else if i > 0 {
                o.buf.push(' ');
            }
            o.buf.push_str(&quote_ident(&c.name));
            let Some(q) = &c.query else {
                o.buf.push_str(" FOR ORDINALITY");
                continue;
            };
            o.buf.push(' ');
            match &c.type_shown {
                Some(t) => o.buf.push_str(t),
                None => o.buf.push_str(&type_name(&c.ty)),
            }
            let opts = FuncOpts::unpack(q.opts);
            if opts.op == Op::Exists {
                o.buf.push_str(" EXISTS");
            } else if opts.op == Op::Query && c.ty.category() == crate::types::Category::String {
                o.buf.push_str(" FORMAT JSON");
            }
            o.buf.push_str(" PATH ");
            o.buf.push_str(&lit(&q.path));
            if opts.op == Op::Query {
                o.buf.push_str(match opts.wrapper {
                    Wrapper::Conditional => " WITH CONDITIONAL WRAPPER",
                    Wrapper::Unconditional => " WITH UNCONDITIONAL WRAPPER",
                    _ => " WITHOUT WRAPPER",
                });
                o.buf.push_str(if opts.omit_quotes {
                    " OMIT QUOTES"
                } else {
                    " KEEP QUOTES"
                });
            }
            let default = FuncOpts::default_behavior(opts.op);
            for (b, d, word) in [
                (opts.on_empty, q.empty_default, "EMPTY"),
                (opts.on_error, q.error_default, "ERROR"),
            ] {
                if b == default {
                    continue;
                }
                o.buf.push(' ');
                o.buf.push_str(b.word());
                if b == Behavior::Default {
                    let e = (o.show)(call.args.get(d?)?)?;
                    o.buf.push(' ');
                    o.buf.push_str(&e);
                }
                o.buf.push_str(" ON ");
                o.buf.push_str(word);
            }
        }
        for (i, n) in p.nested.iter().enumerate() {
            if i > 0 || !p.columns.is_empty() {
                o.buf.push(',');
            }
            o.buf.push(' ');
            o.kw("NESTED PATH ");
            o.buf.push_str(&lit(&n.path));
            o.buf.push_str(&format!(" AS {}", quote_ident(&n.name)));
            columns(o, n, call, type_name, lit)?;
        }
        o.level -= 4;
        o.kw(")");
        Some(())
    }
    columns(&mut o, &spec.root, call, type_name, &lit)?;
    if spec.error_on_error {
        o.buf.push_str(" ERROR ON ERROR");
    }
    o.level -= 4;
    o.kw(")");
    Some(o.buf)
}

/// A ROW() of constants (nested ones too) as the record value it makes.
fn const_row(e: &Expr) -> Option<Value> {
    match e {
        Expr::Const(v, _) => Some(v.clone()),
        Expr::Row(items, _) => Some(Value::Record(std::sync::Arc::new(
            items.iter().map(const_row).collect::<Option<Vec<_>>>()?,
        ))),
        _ => None,
    }
}
