//! Function calls (with their SQL-syntax forms), casts and type names, sort clauses and
//! window definitions.

use super::expr::{Prec, Side, parens_as_c};
use super::{Ctx, D, Mode, R, Unhandled, en, node, str_of};
use pg_query::NodeEnum as E;
use pg_query::protobuf::{self as pb, Node};

impl D {
    pub(super) fn type_cast(&mut self, t: &pb::TypeCast, ctx: Ctx) -> R {
        let arg = node(&t.arg)?;
        let tn = t.type_name.as_ref().ok_or(Unhandled)?;
        let mut parens = parens_as_c(arg);
        if ctx == Ctx::FuncExpr {
            self.s("CAST(");
            self.expr(arg, Ctx::AExpr)?;
            self.s(" AS ");
            self.type_name(tn)?;
            self.c(')');
            return Ok(());
        }
        if let Some(E::AConst(c)) = &arg.node {
            use pb::a_const::Val;
            let val = if c.isnull { None } else { c.val.as_ref() };
            if tn.names.len() == 2 && str_of(&tn.names[0])? == "pg_catalog" {
                let name = str_of(&tn.names[1])?;
                if name == "bpchar" && tn.typmods.is_empty() {
                    self.s("char ");
                    return self.a_const(c);
                } else if name == "bool"
                    && let Some(Val::Sval(s)) = val
                {
                    match s.sval.as_str() {
                        "t" => {
                            self.s("true");
                            return Ok(());
                        }
                        "f" => {
                            self.s("false");
                            return Ok(());
                        }
                        _ => {}
                    }
                }
            }
            if matches!(val, Some(Val::Fval(_)))
                || matches!(val, Some(Val::Ival(i)) if i.ival < 0)
            {
                parens = true;
            }
            if tn.names.len() == 1 && str_of(&tn.names[0])? == "point" && c.location > tn.location {
                self.s(" point ");
                return self.a_const(c);
            }
        }
        if parens {
            self.c('(');
        }
        self.expr(arg, if parens { Ctx::AExpr } else { Ctx::None })?;
        if parens {
            self.c(')');
        }
        self.s("::");
        self.type_name(tn)
    }

    pub(super) fn type_name(&mut self, t: &pb::TypeName) -> R {
        let mut skip_typmods = false;
        if t.setof {
            self.s("SETOF ");
        }
        let catalog = if t.names.len() == 2 && str_of(&t.names[0])? == "pg_catalog" {
            Some(str_of(&t.names[1])?)
        } else {
            None
        };
        match catalog {
            Some(name) => match name {
                "bpchar" => self.s("char"),
                "varchar" | "numeric" | "time" | "timestamp" => self.s(name),
                "bool" => self.s("boolean"),
                "int2" => self.s("smallint"),
                "int4" => self.s("int"),
                "int8" => self.s("bigint"),
                "real" | "float4" => self.s("real"),
                "float8" => self.s("double precision"),
                "timetz" | "timestamptz" => {
                    self.s(if name == "timetz" { "time " } else { "timestamp " });
                    if !t.typmods.is_empty() {
                        self.c('(');
                        for (i, m) in t.typmods.iter().enumerate() {
                            self.int(int_const(m)?.into());
                            if i + 1 < t.typmods.len() {
                                self.s(", ");
                            }
                        }
                        self.s(") ");
                    }
                    self.s("with time zone");
                    skip_typmods = true;
                }
                "interval" => {
                    self.s("interval");
                    if !t.typmods.is_empty() {
                        self.interval_typmods(t)?;
                        skip_typmods = true;
                    }
                }
                _ => {
                    self.s("pg_catalog.");
                    self.s(name);
                }
            },
            None => self.any_name(&t.names)?,
        }
        if !t.typmods.is_empty() && !skip_typmods {
            self.c('(');
            for (i, m) in t.typmods.iter().enumerate() {
                match &m.node {
                    Some(E::AConst(c)) => self.a_const(c)?,
                    Some(E::ParamRef(p)) => self.param_ref(p),
                    Some(E::ColumnRef(c)) => self.column_ref(c)?,
                    _ => return Err(Unhandled),
                }
                if i + 1 < t.typmods.len() {
                    self.s(", ");
                }
            }
            self.c(')');
        }
        for b in &t.array_bounds {
            self.c('[');
            if let Some(E::Integer(i)) = &b.node
                && i.ival != -1
            {
                self.int(i.ival.into());
            }
            self.c(']');
        }
        if t.pct_type {
            self.s("%type");
        }
        Ok(())
    }

    fn interval_typmods(&mut self, t: &pb::TypeName) -> R {
        const fn m(b: u32) -> i32 {
            1 << b
        }
        const MONTH: u32 = 1;
        const YEAR: u32 = 2;
        const DAY: u32 = 3;
        const HOUR: u32 = 10;
        const MINUTE: u32 = 11;
        const SECOND: u32 = 12;
        let fields = int_const(&t.typmods[0])?;
        let text = match fields {
            f if f == m(YEAR) => " year",
            f if f == m(MONTH) => " month",
            f if f == m(DAY) => " day",
            f if f == m(HOUR) => " hour",
            f if f == m(MINUTE) => " minute",
            f if f == m(SECOND) => " second",
            f if f == m(YEAR) | m(MONTH) => " year to month",
            f if f == m(DAY) | m(HOUR) => " day to hour",
            f if f == m(DAY) | m(HOUR) | m(MINUTE) => " day to minute",
            f if f == m(DAY) | m(HOUR) | m(MINUTE) | m(SECOND) => " day to second",
            f if f == m(HOUR) | m(MINUTE) => " hour to minute",
            f if f == m(HOUR) | m(MINUTE) | m(SECOND) => " hour to second",
            f if f == m(MINUTE) | m(SECOND) => " minute to second",
            0x7FFF => "",
            _ => return Err(Unhandled),
        };
        self.s(text);
        if t.typmods.len() == 2 {
            let precision = int_const(&t.typmods[1])?;
            if precision != 0xFFFF {
                self.c('(');
                self.int(precision.into());
                self.c(')');
            }
        }
        Ok(())
    }

    /// `func_name`.
    pub(super) fn func_name(&mut self, names: &[Node]) -> R {
        self.any_name(names)
    }

    pub(super) fn func_call(&mut self, f: &pb::FuncCall) -> R {
        if f.funcname.is_empty() {
            return Err(Unhandled);
        }
        let args = &f.args;
        let a = |i: usize| -> Result<&Node, Unhandled> { args.get(i).ok_or(Unhandled) };
        let catalog = if f.funcname.len() == 2 && str_of(&f.funcname[0])? == "pg_catalog" {
            Some(str_of(&f.funcname[1])?)
        } else {
            None
        };
        let sql = en::<pb::CoercionForm>(f.funcformat) == pb::CoercionForm::CoerceSqlSyntax;
        match (catalog, sql) {
            (Some("overlay"), _) if args.len() == 4 => {
                self.s("OVERLAY(");
                self.expr(a(0)?, Ctx::AExpr)?;
                self.s(" PLACING ");
                self.expr(a(1)?, Ctx::AExpr)?;
                self.s(" FROM ");
                self.expr(a(2)?, Ctx::AExpr)?;
                self.s(" FOR ");
                self.expr(a(3)?, Ctx::AExpr)?;
                self.c(')');
                return Ok(());
            }
            (Some("substring"), true) => {
                if !(args.len() == 2 || args.len() == 3) {
                    return Err(Unhandled);
                }
                self.s("SUBSTRING(");
                self.expr(a(0)?, Ctx::AExpr)?;
                self.s(" FROM ");
                self.expr(a(1)?, Ctx::AExpr)?;
                if args.len() == 3 {
                    self.s(" FOR ");
                    self.expr(a(2)?, Ctx::AExpr)?;
                }
                self.c(')');
                return Ok(());
            }
            (Some("position"), true) if args.len() == 2 => {
                self.s("POSITION(");
                self.bexpr(a(1)?)?;
                self.s(" IN ");
                self.bexpr(a(0)?)?;
                self.c(')');
                return Ok(());
            }
            (Some("overlay"), true) if args.len() == 3 => {
                self.s("overlay(");
                self.expr(a(0)?, Ctx::AExpr)?;
                self.s(" placing ");
                self.expr(a(1)?, Ctx::AExpr)?;
                self.s(" from ");
                self.expr(a(2)?, Ctx::AExpr)?;
                self.c(')');
                return Ok(());
            }
            (Some("pg_collation_for"), true) if args.len() == 1 => {
                self.s("collation for (");
                self.expr(a(0)?, Ctx::AExpr)?;
                self.c(')');
                return Ok(());
            }
            (Some("extract"), true) if args.len() == 2 => {
                self.s("extract (");
                self.expr(a(0)?, Ctx::AExpr)?;
                self.s(" FROM ");
                self.expr(a(1)?, Ctx::AExpr)?;
                self.c(')');
                return Ok(());
            }
            (Some("overlaps"), true) if args.len() == 4 => {
                self.c('(');
                self.expr(a(0)?, Ctx::AExpr)?;
                self.s(", ");
                self.expr(a(1)?, Ctx::AExpr)?;
                self.s(") OVERLAPS (");
                self.expr(a(2)?, Ctx::AExpr)?;
                self.s(", ");
                self.expr(a(3)?, Ctx::AExpr)?;
                self.c(')');
                return Ok(());
            }
            (Some(name @ ("ltrim" | "btrim" | "rtrim")), true) => {
                if !(args.len() == 1 || args.len() == 2) {
                    return Err(Unhandled);
                }
                self.s("TRIM (");
                self.s(match name {
                    "ltrim" => "LEADING ",
                    "btrim" => "BOTH ",
                    _ => "TRAILING ",
                });
                if args.len() == 2 {
                    self.expr(a(1)?, Ctx::AExpr)?;
                }
                self.s(" FROM ");
                self.expr(a(0)?, Ctx::AExpr)?;
                self.c(')');
                return Ok(());
            }
            (Some("timezone"), true) if !args.is_empty() && args.len() <= 2 => {
                let local = args.len() == 1;
                let e = if local { a(0)? } else { a(1)? };
                self.operand(e, Prec::At, Side::Left, Ctx::AExpr)?;
                if local {
                    self.s(" AT LOCAL");
                } else {
                    self.s(" AT TIME ZONE ");
                    self.operand(a(0)?, Prec::At, Side::Right, Ctx::AExpr)?;
                }
                return Ok(());
            }
            (Some("normalize"), true) => {
                if !(args.len() == 1 || args.len() == 2) {
                    return Err(Unhandled);
                }
                self.s("normalize (");
                self.expr(a(0)?, Ctx::AExpr)?;
                if args.len() == 2 {
                    self.s(", ");
                    self.form_const(a(1)?)?;
                }
                self.c(')');
                return Ok(());
            }
            (Some("is_normalized"), true) => {
                if !(args.len() == 1 || args.len() == 2) {
                    return Err(Unhandled);
                }
                self.operand(a(0)?, Prec::Is, Side::Left, Ctx::AExpr)?;
                self.s(" IS ");
                if args.len() == 2 {
                    self.form_const(a(1)?)?;
                }
                self.s(" NORMALIZED ");
                return Ok(());
            }
            (Some("xmlexists"), true) if args.len() == 2 => {
                self.s("xmlexists (");
                self.cexpr(a(0)?)?;
                self.s(" PASSING ");
                self.cexpr(a(1)?)?;
                self.c(')');
                return Ok(());
            }
            (Some("system_user"), true) => {
                self.s("SYSTEM_USER");
                return Ok(());
            }
            _ => {}
        }
        self.func_name(&f.funcname)?;
        self.c('(');
        if f.agg_distinct {
            self.s("DISTINCT ");
        }
        if f.agg_star {
            self.c('*');
        } else {
            for (i, n) in args.iter().enumerate() {
                if f.func_variadic && i + 1 == args.len() {
                    self.s("VARIADIC ");
                }
                self.func_arg(n)?;
                if i + 1 < args.len() {
                    self.s(", ");
                }
            }
        }
        self.c(' ');
        if !f.agg_order.is_empty() && !f.agg_within_group {
            self.sort_clause(&f.agg_order, Ctx::None)?;
        }
        self.strip();
        self.s(") ");
        if !f.agg_order.is_empty() && f.agg_within_group {
            self.s("WITHIN GROUP (");
            self.sort_clause(&f.agg_order, Ctx::None)?;
            self.strip();
            self.s(") ");
        }
        self.filter_over(f.agg_filter.as_deref(), f.over.as_deref())
    }

    /// A function's `FILTER (WHERE ...)` and `OVER ...`, then the trailing space dropped.
    pub(super) fn filter_over(&mut self, filter: Option<&Node>, over: Option<&pb::WindowDef>) -> R {
        if let Some(fl) = filter {
            self.s("FILTER (WHERE ");
            self.expr(fl, Ctx::AExpr)?;
            self.s(") ");
        }
        if let Some(w) = over {
            self.s("OVER ");
            if w.name.is_empty() {
                self.window_def(w)?;
            } else {
                self.s(&w.name);
            }
        }
        self.strip();
        Ok(())
    }

    /// normalize()'s and IS NORMALIZED's form: an A_Const printed as it is.
    fn form_const(&mut self, n: &Node) -> R {
        let Some(E::AConst(c)) = &n.node else {
            return Err(Unhandled);
        };
        if c.isnull {
            return Err(Unhandled);
        }
        self.value(c.val.as_ref().ok_or(Unhandled)?, false)
    }

    /// `func_arg_expr`: a named argument's name isn't quoted.
    fn func_arg(&mut self, n: &Node) -> R {
        if let Some(E::NamedArgExpr(na)) = &n.node {
            if na.name.is_empty() {
                return Err(Unhandled);
            }
            self.s(&na.name);
            self.s(" := ");
            if let Some(arg) = na.arg.as_deref() {
                self.expr(arg, Ctx::AExpr)?;
            }
            return Ok(());
        }
        self.expr(n, Ctx::AExpr)
    }

    /// `opt_sort_clause`: a SELECT's starts a group, a function's is inline.
    pub(super) fn sort_clause(&mut self, l: &[Node], ctx: Ctx) -> R {
        if l.is_empty() {
            return Ok(());
        }
        if ctx == Ctx::SortClause {
            self.group(Some("ORDER BY"), Mode::Merge);
        } else {
            self.s("ORDER BY ");
        }
        for (i, n) in l.iter().enumerate() {
            let Some(E::SortBy(s)) = &n.node else {
                return Err(Unhandled);
            };
            self.sort_by(s)?;
            if i + 1 < l.len() {
                if ctx == Ctx::SortClause {
                    self.comma_part();
                } else {
                    self.s(", ");
                }
            }
        }
        self.c(' ');
        Ok(())
    }

    fn sort_by(&mut self, s: &pb::SortBy) -> R {
        use pb::SortByDir as Dir;
        use pb::SortByNulls as N;
        if let Some(n) = s.node.as_deref() {
            self.expr(n, Ctx::AExpr)?;
        }
        self.c(' ');
        match en::<Dir>(s.sortby_dir) {
            Dir::Undefined | Dir::SortbyDefault => {}
            Dir::SortbyAsc => self.s("ASC "),
            Dir::SortbyDesc => self.s("DESC "),
            Dir::SortbyUsing => {
                self.s("USING ");
                self.qual_op(&s.use_op)?;
            }
        }
        match en::<N>(s.sortby_nulls) {
            N::Undefined | N::SortbyNullsDefault => {}
            N::SortbyNullsFirst => self.s("NULLS FIRST "),
            N::SortbyNullsLast => self.s("NULLS LAST "),
        }
        self.strip();
        Ok(())
    }

    pub(super) fn window_def(&mut self, w: &pb::WindowDef) -> R {
        const NONDEFAULT: i32 = 0x1;
        const RANGE: i32 = 0x2;
        const ROWS: i32 = 0x4;
        const GROUPS: i32 = 0x8;
        const BETWEEN: i32 = 0x10;
        const START_UNBOUNDED_PRECEDING: i32 = 0x20;
        const START_UNBOUNDED_FOLLOWING: i32 = 0x80;
        const END_UNBOUNDED_PRECEDING: i32 = 0x40;
        const END_UNBOUNDED_FOLLOWING: i32 = 0x100;
        const START_CURRENT_ROW: i32 = 0x200;
        const END_CURRENT_ROW: i32 = 0x400;
        const START_OFFSET_PRECEDING: i32 = 0x800;
        const END_OFFSET_PRECEDING: i32 = 0x1000;
        const START_OFFSET_FOLLOWING: i32 = 0x2000;
        const END_OFFSET_FOLLOWING: i32 = 0x4000;
        const EXCLUDE_CURRENT_ROW: i32 = 0x8000;
        const EXCLUDE_GROUP: i32 = 0x10000;
        const EXCLUDE_TIES: i32 = 0x20000;
        let o = w.frame_options;
        if w.refname.is_empty()
            && w.partition_clause.is_empty()
            && w.order_clause.is_empty()
            && o & NONDEFAULT == 0
        {
            self.s("()");
            return Ok(());
        }
        self.c('(');
        self.up();
        if !w.refname.is_empty() {
            self.ident(&w.refname);
            self.c(' ');
        }
        if !w.partition_clause.is_empty() {
            self.s("PARTITION BY ");
            self.expr_list(&w.partition_clause)?;
            self.c(' ');
        }
        self.strip();
        self.part(true);
        self.sort_clause(&w.order_clause, Ctx::None)?;
        if o & NONDEFAULT != 0 {
            self.group(None, Mode::NoIndent);
            if o & RANGE != 0 {
                self.s("RANGE ");
            } else if o & ROWS != 0 {
                self.s("ROWS ");
            } else if o & GROUPS != 0 {
                self.s("GROUPS ");
            }
            if o & BETWEEN != 0 {
                self.s("BETWEEN ");
            }
            let offset = |d: &mut D, n: &Option<Box<Node>>, word: &str| -> R {
                d.expr(node(n)?, Ctx::AExpr)?;
                d.s(word);
                Ok(())
            };
            if o & START_UNBOUNDED_PRECEDING != 0 {
                self.s("UNBOUNDED PRECEDING ");
            } else if o & START_UNBOUNDED_FOLLOWING != 0 {
                return Err(Unhandled);
            } else if o & START_CURRENT_ROW != 0 {
                self.s("CURRENT ROW ");
            } else if o & START_OFFSET_PRECEDING != 0 {
                offset(self, &w.start_offset, " PRECEDING ")?;
            } else if o & START_OFFSET_FOLLOWING != 0 {
                offset(self, &w.start_offset, " FOLLOWING ")?;
            }
            if o & BETWEEN != 0 {
                self.s("AND ");
                if o & END_UNBOUNDED_PRECEDING != 0 {
                    return Err(Unhandled);
                } else if o & END_UNBOUNDED_FOLLOWING != 0 {
                    self.s("UNBOUNDED FOLLOWING ");
                } else if o & END_CURRENT_ROW != 0 {
                    self.s("CURRENT ROW ");
                } else if o & END_OFFSET_PRECEDING != 0 {
                    offset(self, &w.end_offset, " PRECEDING ")?;
                } else if o & END_OFFSET_FOLLOWING != 0 {
                    offset(self, &w.end_offset, " FOLLOWING ")?;
                }
            }
            if o & EXCLUDE_CURRENT_ROW != 0 {
                self.s("EXCLUDE CURRENT ROW ");
            } else if o & EXCLUDE_GROUP != 0 {
                self.s("EXCLUDE GROUP ");
            } else if o & EXCLUDE_TIES != 0 {
                self.s("EXCLUDE TIES ");
            }
        }
        self.strip();
        self.down();
        self.c(')');
        Ok(())
    }
}

/// An A_Const holding an Integer (`intVal(&castNode(A_Const, n)->val)`).
fn int_const(n: &Node) -> Result<i32, Unhandled> {
    match &n.node {
        Some(E::AConst(c)) if !c.isnull => match &c.val {
            Some(pb::a_const::Val::Ival(i)) => Ok(i.ival),
            _ => Err(Unhandled),
        },
        _ => Err(Unhandled),
    }
}
