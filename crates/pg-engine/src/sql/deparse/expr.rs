//! Expressions: `a_expr`, `b_expr` and `c_expr`, with the C deparser's precedence rules for
//! parentheses, function calls, casts, type names and constants.

use super::{Ctx, D, Mode, R, Unhandled, en, node, str_of, str_opt};
use crate::stack;
use pg_query::NodeEnum as E;
use pg_query::protobuf::{self as pb, Node};

/// Precedence of the gram.y rule a node prints as, lowest first (`DeparseExprPrec`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Prec {
    Or,
    And,
    Not,
    Is,
    Comparison,
    Between,
    Op,
    Add,
    Mul,
    Exp,
    At,
    Collate,
    UMinus,
    Atom,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Side {
    Left,
    Right,
}

fn op_prec(op: &[Node]) -> Prec {
    if op.len() != 1 {
        return Prec::Op;
    }
    match str_opt(&op[0]).unwrap_or("") {
        "+" | "-" => Prec::Add,
        "*" | "/" | "%" => Prec::Mul,
        "^" => Prec::Exp,
        "<" | ">" | "=" | "<=" | ">=" | "<>" | "!=" => Prec::Comparison,
        _ => Prec::Op,
    }
}

fn subquery_op_prec(op: &[Node]) -> Prec {
    if op.len() == 1 && matches!(str_opt(&op[0]), Some("~~" | "!~~" | "~~*" | "!~~*")) {
        return Prec::Between;
    }
    op_prec(op)
}

/// `isSqlSyntaxFuncCall`.
pub(super) fn sql_syntax(f: &pb::FuncCall, name: &str) -> bool {
    en::<pb::CoercionForm>(f.funcformat) == pb::CoercionForm::CoerceSqlSyntax
        && f.funcname.len() == 2
        && str_opt(&f.funcname[0]) == Some("pg_catalog")
        && str_opt(&f.funcname[1]) == Some(name)
}

/// `getExprPrecedence`.
fn prec(n: &Node) -> Prec {
    match &n.node {
        Some(E::AExpr(a)) => prec_of_a_expr(a),
        Some(E::BoolExpr(b)) => match en::<pb::BoolExprType>(b.boolop) {
            pb::BoolExprType::Undefined | pb::BoolExprType::AndExpr => Prec::And,
            pb::BoolExprType::NotExpr => Prec::Not,
            pb::BoolExprType::OrExpr => Prec::Or,
        },
        Some(E::NullTest(_) | E::BooleanTest(_) | E::JsonIsPredicate(_)) => Prec::Is,
        Some(E::CollateClause(_)) => Prec::Collate,
        Some(E::XmlExpr(x)) if en::<pb::XmlExprOp>(x.op) == pb::XmlExprOp::IsDocument => Prec::Is,
        Some(E::SubLink(s)) => {
            let t = en::<pb::SubLinkType>(s.sub_link_type);
            if t == pb::SubLinkType::AnySublink && s.oper_name.is_empty() {
                Prec::Between
            } else if matches!(t, pb::SubLinkType::AnySublink | pb::SubLinkType::AllSublink) {
                subquery_op_prec(&s.oper_name).min(Prec::Op)
            } else {
                Prec::Atom
            }
        }
        Some(E::FuncCall(f)) => {
            if sql_syntax(f, "timezone") && !f.args.is_empty() && f.args.len() <= 2 {
                Prec::At
            } else if sql_syntax(f, "is_normalized") {
                Prec::Is
            } else {
                Prec::Atom
            }
        }
        _ => Prec::Atom,
    }
}

fn subquery_comparison(n: &Node) -> bool {
    use pb::AExprKind as K;
    match &n.node {
        Some(E::AExpr(a)) => matches!(en::<K>(a.kind), K::AexprOpAny | K::AexprOpAll | K::AexprIn),
        Some(E::SubLink(s)) => matches!(
            en::<pb::SubLinkType>(s.sub_link_type),
            pb::SubLinkType::AnySublink | pb::SubLinkType::AllSublink
        ),
        _ => false,
    }
}

fn parens_for_prec(n: &Node, parent: Prec, side: Side) -> bool {
    let p = prec(n);
    if side == Side::Left && subquery_comparison(n) && p == parent {
        return false;
    }
    if p != parent {
        return p < parent;
    }
    match parent {
        Prec::Is | Prec::Comparison | Prec::Between => true,
        Prec::Not | Prec::UMinus => false,
        _ => side == Side::Right,
    }
}

/// `needsParensAsBExpr`.
pub(super) fn parens_as_b(n: &Node) -> bool {
    use pb::AExprKind as K;
    match &n.node {
        None => false,
        Some(E::AExpr(a)) => !matches!(
            en::<K>(a.kind),
            K::Undefined | K::AexprOp | K::AexprDistinct | K::AexprNotDistinct | K::AexprNullif
        ),
        Some(E::XmlExpr(x)) if en::<pb::XmlExprOp>(x.op) == pb::XmlExprOp::IsDocument => false,
        Some(E::FuncCall(f)) if sql_syntax(f, "overlaps") && f.args.len() == 4 => true,
        _ => prec(n) != Prec::Atom,
    }
}

/// `needsParensAsCExpr`.
pub(super) fn parens_as_c(n: &Node) -> bool {
    prec(n) != Prec::Atom || parens_as_b(n)
}

/// Node kinds `func_expr` takes.
fn is_func_expr(e: &E) -> bool {
    matches!(
        e,
        E::FuncCall(_)
            | E::SqlvalueFunction(_)
            | E::MinMaxExpr(_)
            | E::CoalesceExpr(_)
            | E::XmlExpr(_)
            | E::XmlSerialize(_)
            | E::JsonObjectAgg(_)
            | E::JsonArrayAgg(_)
            | E::JsonObjectConstructor(_)
            | E::JsonArrayConstructor(_)
            | E::JsonArrayQueryConstructor(_)
    )
}

impl D {
    /// `deparseExpr` ("a_expr"); an empty node prints nothing.
    pub(super) fn expr(&mut self, n: &Node, ctx: Ctx) -> R {
        if let Some(seen) = &mut self.seen
            && n.node.is_some()
        {
            seen.push(n);
        }
        stack::grow(|| self.expr_inner(n, ctx))
    }

    fn expr_inner(&mut self, n: &Node, ctx: Ctx) -> R {
        let Some(e) = &n.node else {
            return Ok(());
        };
        match e {
            E::ColumnRef(_)
            | E::AConst(_)
            | E::ParamRef(_)
            | E::AIndirection(_)
            | E::CaseExpr(_)
            | E::AArrayExpr(_)
            | E::RowExpr(_)
            | E::GroupingFunc(_) => self.cexpr(n),
            E::SubLink(s) => self.sublink(s),
            E::TypeCast(t) => self.type_cast(t, Ctx::None),
            E::CollateClause(c) => self.collate_clause(c),
            E::AExpr(a) => self.a_expr(a, if ctx == Ctx::BExpr { Ctx::BExpr } else { Ctx::AExpr }),
            E::BoolExpr(b) => self.bool_expr(b),
            E::NullTest(t) => {
                self.operand(node(&t.arg)?, Prec::Is, Side::Left, Ctx::AExpr)?;
                self.s(match en::<pb::NullTestType>(t.nulltesttype) {
                    pb::NullTestType::IsNotNull => " IS NOT NULL",
                    _ => " IS NULL",
                });
                Ok(())
            }
            E::BooleanTest(t) => {
                use pb::BoolTestType as B;
                self.operand(node(&t.arg)?, Prec::Is, Side::Left, Ctx::AExpr)?;
                self.s(match en::<B>(t.booltesttype) {
                    B::Undefined | B::IsTrue => " IS TRUE",
                    B::IsNotTrue => " IS NOT TRUE",
                    B::IsFalse => " IS FALSE",
                    B::IsNotFalse => " IS NOT FALSE",
                    B::IsUnknown => " IS UNKNOWN",
                    B::IsNotUnknown => " IS NOT UNKNOWN",
                });
                Ok(())
            }
            E::JsonIsPredicate(j) => self.json_is_predicate(j),
            E::SetToDefault(_) => {
                self.s("DEFAULT");
                Ok(())
            }
            E::MergeSupportFunc(_) => {
                self.s("merge_action() ");
                Ok(())
            }
            E::JsonParseExpr(j) => self.json_parse_expr(j),
            E::JsonScalarExpr(j) => self.json_scalar_expr(j),
            E::JsonSerializeExpr(j) => self.json_serialize_expr(j),
            E::JsonFuncExpr(j) => self.json_func_expr(j),
            e if is_func_expr(e) => self.func_expr(e, ctx),
            _ => Err(Unhandled),
        }
    }

    /// `deparseFuncExpr`.
    pub(super) fn func_expr(&mut self, e: &E, ctx: Ctx) -> R {
        match e {
            E::FuncCall(f) => self.func_call(f),
            E::SqlvalueFunction(f) => {
                self.sql_value_function(f);
                Ok(())
            }
            E::MinMaxExpr(m) => {
                self.s(match en::<pb::MinMaxOp>(m.op) {
                    pb::MinMaxOp::IsLeast => "LEAST(",
                    _ => "GREATEST(",
                });
                self.expr_list(&m.args)?;
                self.c(')');
                Ok(())
            }
            E::CoalesceExpr(c) => {
                self.s("COALESCE(");
                self.expr_list(&c.args)?;
                self.c(')');
                Ok(())
            }
            E::XmlExpr(x) => self.xml_expr(x, ctx),
            E::XmlSerialize(x) => self.xml_serialize(x),
            E::JsonObjectAgg(j) => self.json_object_agg(j),
            E::JsonArrayAgg(j) => self.json_array_agg(j),
            E::JsonObjectConstructor(j) => self.json_object_constructor(j),
            E::JsonArrayConstructor(j) => self.json_array_constructor(j),
            E::JsonArrayQueryConstructor(j) => self.json_array_query_constructor(j),
            _ => Err(Unhandled),
        }
    }

    /// `deparseBExpr`.
    pub(super) fn bexpr(&mut self, n: &Node) -> R {
        if n.node.is_none() {
            return Err(Unhandled);
        }
        if parens_as_b(n) {
            self.c('(');
            self.expr(n, Ctx::AExpr)?;
            self.c(')');
            return Ok(());
        }
        self.expr(n, Ctx::BExpr)
    }

    /// `deparseCExpr`.
    pub(super) fn cexpr(&mut self, n: &Node) -> R {
        let Some(e) = &n.node else {
            return Err(Unhandled);
        };
        if parens_as_c(n) {
            self.c('(');
            self.expr(n, Ctx::AExpr)?;
            self.c(')');
            return Ok(());
        }
        match e {
            E::ColumnRef(c) => self.column_ref(c),
            E::AConst(c) => self.a_const(c),
            E::ParamRef(p) => {
                self.param_ref(p);
                Ok(())
            }
            E::AIndirection(a) => self.a_indirection(a),
            E::CaseExpr(c) => self.case_expr(c),
            E::SubLink(s) => self.sublink(s),
            E::AArrayExpr(a) => {
                self.s("ARRAY[");
                self.expr_list(&a.elements)?;
                self.c(']');
                Ok(())
            }
            E::RowExpr(r) => {
                match en::<pb::CoercionForm>(r.row_format) {
                    pb::CoercionForm::Undefined | pb::CoercionForm::CoerceExplicitCall => {
                        self.s("ROW")
                    }
                    pb::CoercionForm::CoerceImplicitCast => {}
                    _ => return Err(Unhandled),
                }
                self.s("(");
                self.expr_list(&r.args)?;
                self.c(')');
                Ok(())
            }
            E::GroupingFunc(g) => {
                self.s("GROUPING(");
                self.expr_list(&g.args)?;
                self.c(')');
                Ok(())
            }
            e if is_func_expr(e) => self.func_expr(e, Ctx::None),
            _ => {
                self.c('(');
                self.expr(n, Ctx::AExpr)?;
                self.c(')');
                Ok(())
            }
        }
    }

    /// `deparseExprList`.
    pub(super) fn expr_list(&mut self, l: &[Node]) -> R {
        for (i, n) in l.iter().enumerate() {
            self.expr(n, Ctx::AExpr)?;
            if i + 1 < l.len() {
                self.s(", ");
            }
        }
        Ok(())
    }

    /// `deparseExprOperand`.
    pub(super) fn operand(&mut self, n: &Node, parent: Prec, side: Side, ctx: Ctx) -> R {
        if n.node.is_none() {
            return Err(Unhandled);
        }
        let parens = parens_for_prec(n, parent, side) || (ctx == Ctx::BExpr && parens_as_b(n));
        if parens {
            self.c('(');
        }
        self.expr(n, if parens { Ctx::AExpr } else { ctx })?;
        if parens {
            self.c(')');
        }
        Ok(())
    }

    /// `qual_Op`.
    pub(super) fn qual_op(&mut self, op: &[Node]) -> R {
        if op.len() == 1 && is_op(str_of(&op[0])?) {
            self.s(str_of(&op[0])?);
            return Ok(());
        }
        self.s("OPERATOR(");
        self.any_operator(op)?;
        self.s(")");
        Ok(())
    }

    /// `any_operator`.
    pub(super) fn any_operator(&mut self, op: &[Node]) -> R {
        match op {
            [schema, name] => {
                let name = str_of(name)?;
                self.ident(str_of(schema)?);
                self.c('.');
                self.s(name);
            }
            [name] => self.s(str_of(name)?),
            _ => return Err(Unhandled),
        }
        Ok(())
    }

    /// `subquery_Op`.
    fn subquery_op(&mut self, op: &[Node]) -> R {
        if op.len() == 1 {
            let name = str_of(&op[0])?;
            let word = match name {
                "~~" => "LIKE",
                "!~~" => "NOT LIKE",
                "~~*" => "ILIKE",
                "!~~*" => "NOT ILIKE",
                _ if is_op(name) => name,
                _ => "",
            };
            if !word.is_empty() {
                self.s(word);
                return Ok(());
            }
        }
        self.s("OPERATOR(");
        self.any_operator(op)?;
        self.s(")");
        Ok(())
    }

    fn a_expr(&mut self, a: &pb::AExpr, ctx: Ctx) -> R {
        use pb::AExprKind as K;
        let rexpr = node(&a.rexpr)?;
        let p = prec_of_a_expr(a);
        let kind = en::<K>(a.kind);
        // The fixed names of the kinds below (an operator's is checked as it prints).
        let name = || -> Result<&str, Unhandled> {
            match a.name.as_slice() {
                [n] => str_of(n),
                _ => Err(Unhandled),
            }
        };
        match kind {
            K::Undefined | K::AexprOp => {
                if let Some(l) = a.lexpr.as_deref() {
                    self.operand(l, p, Side::Left, ctx)?;
                    self.c(' ');
                }
                self.qual_op(&a.name)?;
                self.c(' ');
                self.operand(rexpr, p, Side::Right, ctx)
            }
            K::AexprOpAny | K::AexprOpAll => {
                self.operand(node(&a.lexpr)?, subquery_op_prec(&a.name), Side::Left, ctx)?;
                self.c(' ');
                self.subquery_op(&a.name)?;
                self.s(if kind == K::AexprOpAny { " ANY(" } else { " ALL(" });
                self.expr(rexpr, Ctx::AExpr)?;
                self.c(')');
                Ok(())
            }
            K::AexprDistinct | K::AexprNotDistinct => {
                self.operand(node(&a.lexpr)?, p, Side::Left, ctx)?;
                self.s(if kind == K::AexprDistinct {
                    " IS DISTINCT FROM "
                } else {
                    " IS NOT DISTINCT FROM "
                });
                self.operand(rexpr, p, Side::Right, ctx)
            }
            K::AexprNullif => {
                self.s("NULLIF(");
                self.expr(node(&a.lexpr)?, ctx)?;
                self.s(", ");
                self.expr(rexpr, ctx)?;
                self.c(')');
                Ok(())
            }
            K::AexprIn => {
                self.operand(node(&a.lexpr)?, p, Side::Left, ctx)?;
                self.c(' ');
                match name()? {
                    "=" => self.s("IN "),
                    "<>" => self.s("NOT IN "),
                    _ => return Err(Unhandled),
                }
                self.c('(');
                match &rexpr.node {
                    Some(E::SubLink(s)) => self.sublink(s)?,
                    Some(E::List(l)) => self.expr_list(&l.items)?,
                    _ => return Err(Unhandled),
                }
                self.c(')');
                Ok(())
            }
            K::AexprLike | K::AexprIlike | K::AexprSimilar => {
                let word = match (kind, name()?) {
                    (K::AexprLike, "~~") => "LIKE ",
                    (K::AexprLike, "!~~") => "NOT LIKE ",
                    (K::AexprIlike, "~~*") => "ILIKE ",
                    (K::AexprIlike, "!~~*") => "NOT ILIKE ",
                    (K::AexprSimilar, "~") => "SIMILAR TO ",
                    (K::AexprSimilar, "!~") => "NOT SIMILAR TO ",
                    _ => return Err(Unhandled),
                };
                self.operand(node(&a.lexpr)?, p, Side::Left, ctx)?;
                self.c(' ');
                self.s(word);
                if kind != K::AexprSimilar {
                    return self.operand(rexpr, p, Side::Right, ctx);
                }
                let Some(E::FuncCall(f)) = &rexpr.node else {
                    return Err(Unhandled);
                };
                match f.args.as_slice() {
                    [pat] => self.operand(pat, p, Side::Right, ctx),
                    [pat, esc] => {
                        self.operand(pat, p, Side::Right, ctx)?;
                        self.s(" ESCAPE ");
                        self.operand(esc, p, Side::Right, ctx)
                    }
                    _ => Err(Unhandled),
                }
            }
            K::AexprBetween | K::AexprNotBetween | K::AexprBetweenSym | K::AexprNotBetweenSym => {
                let Some(E::List(l)) = &rexpr.node else {
                    return Err(Unhandled);
                };
                let [lo, hi] = l.items.as_slice() else {
                    return Err(Unhandled);
                };
                let word = name()?;
                self.operand(node(&a.lexpr)?, p, Side::Left, ctx)?;
                self.c(' ');
                self.s(word);
                self.c(' ');
                // The lower bound is a b_expr in gram.y, the upper an a_expr.
                self.operand(lo, p, Side::Right, Ctx::BExpr)?;
                self.s(" AND ");
                self.operand(hi, p, Side::Right, ctx)
            }
        }
    }

    fn bool_expr(&mut self, b: &pb::BoolExpr) -> R {
        let nested = |n: &Node| {
            matches!(&n.node, Some(E::BoolExpr(b)) if matches!(
                en::<pb::BoolExprType>(b.boolop),
                pb::BoolExprType::Undefined | pb::BoolExprType::AndExpr | pb::BoolExprType::OrExpr
            ))
        };
        let op = en::<pb::BoolExprType>(b.boolop);
        if op == pb::BoolExprType::NotExpr {
            let arg = b.args.first().ok_or(Unhandled)?;
            if arg.node.is_none() {
                return Err(Unhandled);
            }
            let parens = nested(arg);
            self.s("NOT ");
            if parens {
                self.c('(');
            }
            self.expr(arg, Ctx::AExpr)?;
            if parens {
                self.c(')');
            }
            return Ok(());
        }
        for (i, n) in b.args.iter().enumerate() {
            if n.node.is_none() {
                return Err(Unhandled);
            }
            let parens = nested(n);
            if parens {
                self.c('(');
            }
            self.expr(n, Ctx::AExpr)?;
            if parens {
                self.c(')');
            }
            if i + 1 < b.args.len() {
                if op == pb::BoolExprType::OrExpr {
                    self.s(" OR ");
                } else {
                    self.part(true);
                    self.s("AND ");
                }
            }
        }
        Ok(())
    }

    pub(super) fn collate_clause(&mut self, c: &pb::CollateClause) -> R {
        if let Some(arg) = c.arg.as_deref() {
            self.operand(arg, Prec::Collate, Side::Left, Ctx::AExpr)?;
            self.c(' ');
        }
        self.s("COLLATE ");
        self.any_name(&c.collname)
    }

    pub(super) fn column_ref(&mut self, c: &pb::ColumnRef) -> R {
        match c.fields.first().and_then(|f| f.node.as_ref()) {
            Some(E::AStar(_)) => self.c('*'),
            Some(E::String(s)) => self.ident(&s.sval),
            Some(_) => {}
            None => return Err(Unhandled),
        }
        self.indirection(&c.fields[1..])
    }

    /// `opt_indirection`.
    pub(super) fn indirection(&mut self, l: &[Node]) -> R {
        for n in l {
            match &n.node {
                Some(E::String(s)) => {
                    self.c('.');
                    self.ident(&s.sval);
                }
                Some(E::AStar(_)) => self.s(".*"),
                Some(E::AIndices(i)) => {
                    self.c('[');
                    if let Some(l) = i.lidx.as_deref() {
                        self.expr(l, Ctx::AExpr)?;
                    }
                    if i.is_slice {
                        self.c(':');
                    }
                    if let Some(u) = i.uidx.as_deref() {
                        self.expr(u, Ctx::AExpr)?;
                    }
                    self.c(']');
                }
                _ => return Err(Unhandled),
            }
        }
        Ok(())
    }

    pub(super) fn param_ref(&mut self, p: &pb::ParamRef) {
        if p.number == 0 {
            self.c('?');
        } else {
            self.c('$');
            self.int(p.number.into());
        }
    }

    fn a_indirection(&mut self, a: &pb::AIndirection) -> R {
        let arg = node(&a.arg)?;
        let bare = match &arg.node {
            Some(E::ParamRef(_)) => true,
            Some(E::ColumnRef(_)) => {
                let first = a.indirection.first().ok_or(Unhandled)?;
                matches!(first.node, Some(E::AIndices(_)))
            }
            Some(E::SubLink(s)) => {
                en::<pb::SubLinkType>(s.sub_link_type) == pb::SubLinkType::ExprSublink
            }
            _ => false,
        };
        if !bare {
            self.c('(');
        }
        self.expr(arg, if bare { Ctx::None } else { Ctx::AExpr })?;
        if !bare {
            self.c(')');
        }
        self.indirection(&a.indirection)
    }

    fn case_expr(&mut self, c: &pb::CaseExpr) -> R {
        self.s("CASE ");
        if let Some(arg) = c.arg.as_deref() {
            self.expr(arg, Ctx::AExpr)?;
            self.c(' ');
        }
        self.up();
        for w in &c.args {
            let Some(E::CaseWhen(w)) = &w.node else {
                return Err(Unhandled);
            };
            self.group(Some("WHEN"), Mode::Indent);
            if let Some(e) = w.expr.as_deref() {
                self.expr(e, Ctx::AExpr)?;
            }
            self.s(" THEN ");
            if let Some(r) = w.result.as_deref() {
                self.expr(r, Ctx::AExpr)?;
            }
        }
        if let Some(d) = c.defresult.as_deref() {
            self.group(Some("ELSE"), Mode::Indent);
            self.expr(d, Ctx::AExpr)?;
        }
        self.c(' ');
        self.down();
        self.s("END");
        Ok(())
    }

    pub(super) fn sublink(&mut self, s: &pb::SubLink) -> R {
        use pb::SubLinkType as T;
        let Some(E::SelectStmt(sel)) = s.subselect.as_deref().and_then(|n| n.node.as_ref()) else {
            return Err(Unhandled);
        };
        match en::<T>(s.sub_link_type) {
            T::Undefined | T::ExistsSublink => self.s("EXISTS ("),
            T::AllSublink => {
                let t = node(&s.testexpr)?;
                self.operand(t, subquery_op_prec(&s.oper_name), Side::Left, Ctx::AExpr)?;
                self.c(' ');
                self.subquery_op(&s.oper_name)?;
                self.s(" ALL (");
            }
            T::AnySublink => {
                let t = node(&s.testexpr)?;
                if s.oper_name.is_empty() {
                    self.operand(t, Prec::Between, Side::Left, Ctx::AExpr)?;
                    self.s(" IN ");
                } else {
                    self.operand(t, subquery_op_prec(&s.oper_name), Side::Left, Ctx::AExpr)?;
                    self.c(' ');
                    self.subquery_op(&s.oper_name)?;
                    self.s(" ANY ");
                }
                self.c('(');
            }
            T::ExprSublink => self.s("("),
            T::ArraySublink => self.s("ARRAY("),
            _ => return Err(Unhandled),
        }
        self.select_stmt(sel, Ctx::None)?;
        self.c(')');
        Ok(())
    }

    /// `deparseAConst`.
    pub(super) fn a_const(&mut self, c: &pb::AConst) -> R {
        if c.isnull {
            self.s("NULL");
            return Ok(());
        }
        self.value(c.val.as_ref().ok_or(Unhandled)?, true)
    }

    /// `deparseValue`, as a constant (strings quoted) or not (printed as they are).
    pub(super) fn value(&mut self, v: &pb::a_const::Val, constant: bool) -> R {
        use pb::a_const::Val;
        match v {
            Val::Ival(i) => self.int(i.ival.into()),
            Val::Fval(f) => self.s(&f.fval),
            Val::Boolval(b) => self.s(if b.boolval { "true" } else { "false" }),
            Val::Sval(s) if constant => self.literal(&s.sval),
            Val::Sval(s) => self.s(&s.sval),
            Val::Bsval(b) => {
                let (kind, rest) = match b.bsval.as_bytes().first() {
                    Some(b'x') => ('x', &b.bsval[1..]),
                    Some(b'b') => ('b', &b.bsval[1..]),
                    _ => return Err(Unhandled),
                };
                self.c(kind);
                self.literal(rest);
            }
        }
        Ok(())
    }

    fn sql_value_function(&mut self, f: &pb::SqlValueFunction) {
        use pb::SqlValueFunctionOp as O;
        self.s(match en::<O>(f.op) {
            O::SqlvalueFunctionOpUndefined | O::SvfopCurrentDate => "current_date",
            O::SvfopCurrentTime | O::SvfopCurrentTimeN => "current_time",
            O::SvfopCurrentTimestamp | O::SvfopCurrentTimestampN => "current_timestamp",
            O::SvfopLocaltime | O::SvfopLocaltimeN => "localtime",
            O::SvfopLocaltimestamp | O::SvfopLocaltimestampN => "localtimestamp",
            O::SvfopCurrentRole => "current_role",
            O::SvfopCurrentUser => "current_user",
            O::SvfopUser => "user",
            O::SvfopSessionUser => "session_user",
            O::SvfopCurrentCatalog => "current_catalog",
            O::SvfopCurrentSchema => "current_schema",
        });
        if f.typmod != -1 {
            self.c('(');
            self.int(f.typmod.into());
            self.c(')');
        }
    }

}

/// `getExprPrecedence` of an A_Expr.
fn prec_of_a_expr(a: &pb::AExpr) -> Prec {
    use pb::AExprKind as K;
    match en::<K>(a.kind) {
        K::Undefined | K::AexprOp => {
            if a.lexpr.is_none() {
                if op_prec(&a.name) == Prec::Add {
                    Prec::UMinus
                } else {
                    Prec::Op
                }
            } else {
                op_prec(&a.name)
            }
        }
        K::AexprOpAny | K::AexprOpAll => subquery_op_prec(&a.name).min(Prec::Op),
        K::AexprDistinct | K::AexprNotDistinct => Prec::Is,
        K::AexprNullif => Prec::Atom,
        _ => Prec::Between,
    }
}


/// Whether a name is made of operator characters only (`isOp`).
fn is_op(s: &str) -> bool {
    s.bytes().all(|b| b"~!@#^&|`?+-*/%<>=".contains(&b))
}
