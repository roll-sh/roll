//! Remote SQL (deparse.c): which expressions can run on the remote server
//! (foreign_expr_walker) and their text, the SELECT of a scan, and DML statements.

use super::option;
use crate::catalog::{Catalog, Table, quote_ident};
use crate::sql::expr::Expr;
use crate::types::{DataType, Value};

/// FirstGenbkiObjectId: objects below it are built in (shippable).
const FIRST_GENBKI: u32 = 10000;

/// What deparsing a base relation's expressions needs.
pub struct Deparser<'a> {
    pub cat: &'a Catalog,
    pub table: &'a Table,
    /// The `extensions` option's extensions (their objects are shippable too).
    pub extensions: Vec<u32>,
    /// The values of the remote query's parameters, in `$n` order (outer references and
    /// statement parameters).
    pub params: std::cell::RefCell<Vec<Expr>>,
    /// Qualify columns with this alias (joins: `r1`).
    pub alias: Option<String>,
    /// How constants print (the transmission modes: ISO dates, postgres intervals, 3 extra
    /// float digits).
    pub fmt: crate::types::text::FormatCtx,
    /// The scan row's column holding the remote row's ctid (a scan returning system
    /// columns), which conditions may send.
    pub ctid_col: Option<usize>,
}

/// Collation state of a shippable expression (FDWCollateState).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Coll {
    None,
    Safe,
    Unsafe,
}

/// The remote name of a foreign table: `schema_name` / `table_name` options, else the local
/// ones (deparseRelation).
pub fn relation_name(cat: &Catalog, t: &Table) -> String {
    let opts = t.foreign.as_ref().and_then(|f| f.options.as_ref());
    let schema = option::get(opts, "schema_name").map_or_else(|| cat.schema_name(t.schema).to_string(), str::to_string);
    let name = option::get(opts, "table_name").map_or_else(|| t.name.clone(), str::to_string);
    format!("{}.{}", quote_ident(&schema), quote_ident(&name))
}

/// The remote name of column `i`: its `column_name` option, else its own.
pub fn column_name(t: &Table, i: usize) -> String {
    let opts = t.foreign.as_ref().and_then(|f| f.column_options(i));
    option::get(opts, "column_name").map_or_else(|| t.columns[i].name.clone(), str::to_string)
}

/// The transmission modes' format (set_transmission_modes).
pub fn transmission_fmt(base: &crate::types::text::FormatCtx) -> crate::types::text::FormatCtx {
    let mut f = base.clone();
    f.date_style = crate::types::datetime::DateStyle::parse("ISO, MDY");
    f.interval_style = crate::types::text::IntervalStyle::Postgres;
    f.extra_float_digits = f.extra_float_digits.max(3);
    f
}

/// deparse_type_name: built-in types unqualified, others schema-qualified.
pub fn type_name(cat: &Catalog, ty: &DataType) -> String {
    let oid = crate::catalog::system::type_oid(cat, ty);
    crate::sql::func::system::format_type_oid(oid, ty.typmod(), cat, &[]).unwrap_or_else(|| ty.typname())
}

/// deparseStringLiteral.
pub fn string_literal(s: &str) -> String {
    let mut out = String::new();
    if s.contains('\\') {
        out.push('E');
    }
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' || ch == '\\' {
            out.push(ch);
        }
        out.push(ch);
    }
    out.push('\'');
    out
}

/// Whether a function or operator is an operator (its name is a symbol).
pub fn is_operator(name: &str) -> bool {
    name.chars().next().is_some_and(|c| !c.is_alphanumeric() && c != '_' && c != '"')
}

impl<'a> Deparser<'a> {
    pub fn new(cat: &'a Catalog, table: &'a Table, fmt: &crate::types::text::FormatCtx) -> Deparser<'a> {
        let server = table.foreign.as_ref().and_then(|f| cat.servers.get(&f.server));
        let extensions = server
            .and_then(|s| option::get(s.options.as_ref(), "extensions"))
            .and_then(|l| option::extension_list(cat, l, None).ok())
            .unwrap_or_default();
        Deparser {
            cat,
            table,
            extensions,
            params: Default::default(),
            alias: None,
            fmt: transmission_fmt(fmt),
            ctid_col: None,
        }
    }

    /// is_shippable: built in, or a member of a listed extension.
    fn object_shippable(&self, oid: u32) -> bool {
        oid < FIRST_GENBKI
            || (!self.extensions.is_empty()
                && self.extensions.iter().any(|ext| self.cat.extension_info.get(ext).is_some_and(|i| i.members.iter().any(|m| m.1 == oid))))
    }

    /// A call's function or operator is shippable: a built-in one (the registry's, by
    /// name and oid: an operator made over a built-in function isn't), or an extension's.
    fn call_shippable(&self, def: &crate::sql::func::FuncDef) -> bool {
        let reg = crate::sql::func::registry();
        let builtin = |list: Option<&Vec<std::sync::Arc<crate::sql::func::FuncDef>>>| {
            list.is_some_and(|v| v.iter().any(|d| d.oid == def.oid))
        };
        let found = if is_operator(def.name) {
            builtin(reg.binops.get(def.name)) || builtin(reg.prefix.get(def.name))
        } else {
            builtin(reg.funcs.get(def.name))
        };
        (found && def.oid < FIRST_GENBKI) || (!self.extensions.is_empty() && def.oid >= FIRST_GENBKI && self.object_shippable(def.oid))
    }

    fn type_shippable(&self, ty: &DataType) -> bool {
        match ty {
            DataType::Array(e) => self.type_shippable(e),
            t => self.object_shippable(crate::catalog::system::type_oid(self.cat, t)),
        }
    }

    /// The collation of column `i` (None: not collatable), and whether it is the default.
    fn column_collation(&self, i: usize) -> Option<String> {
        let c = self.table.columns.get(i)?;
        if !c.ty.collatable() {
            return None;
        }
        Some(c.collation.clone().unwrap_or_else(|| "default".into()))
    }

    /// is_foreign_expr: every node can run remotely, with no volatile or stable functions.
    pub fn shippable(&self, e: &Expr) -> bool {
        let mut outer = (Coll::None, None);
        self.walk(e, &mut outer) && !crate::sql::paths::expr_volatile_deep(e) && !has_stable(e)
    }

    /// foreign_expr_walker for one node: shippable children and objects, and the collation
    /// the node's value has (merged into `outer`).
    fn walk(&self, e: &Expr, outer: &mut (Coll, Option<String>)) -> bool {
        let mut inner: (Coll, Option<String>) = (Coll::None, None);
        let default = || Some("default".to_string());
        // The collation of a node computing a collatable value from collated input.
        let derived = |inner: &(Coll, Option<String>), ty: &DataType, coll: Option<String>| {
            if !ty.collatable() {
                return (Coll::None, None);
            }
            let c = coll.or_else(default);
            if inner.0 == Coll::Safe && inner.1 == c {
                (Coll::Safe, c)
            } else if c.as_deref() == Some("default") {
                (Coll::None, c)
            } else {
                (Coll::Unsafe, c)
            }
        };
        // A call's input collation must be the one its collated inputs bring.
        let input_ok = |inner: &(Coll, Option<String>), args: &[&Expr], coll: Option<String>| {
            if !args.iter().any(|a| a.ty().collatable()) {
                return true;
            }
            inner.0 == Coll::Safe && inner.1 == coll.or_else(default)
        };
        let (state, coll): (Coll, Option<String>) = match e {
            Expr::Col(i, _) if Some(*i) == self.ctid_col => (Coll::None, None),
            Expr::Col(i, _) => {
                if *i >= self.table.columns.len() {
                    return false;
                }
                let c = self.column_collation(*i);
                (if c.is_some() { Coll::Safe } else { Coll::None }, c)
            }
            Expr::Outer(..) | Expr::Param(..) => (Coll::None, None),
            Expr::Const(v, ty) => {
                if let (Value::Oid(o), DataType::Regproc | DataType::Regprocedure | DataType::Regoper | DataType::Regoperator | DataType::Regclass | DataType::Regtype | DataType::Regnamespace | DataType::Regrole | DataType::Regcollation) = (v, ty)
                    && !self.object_shippable(*o)
                {
                    return false;
                }
                (Coll::None, None)
            }
            Expr::Call(def, args, ty) => {
                if def.srf.is_some() || !self.call_shippable(def) {
                    return false;
                }
                if !args.iter().all(|a| self.walk(a, &mut inner)) {
                    return false;
                }
                let incoll = def.coll.as_ref().map(|c| c.name.clone());
                if !input_ok(&inner, &args.iter().collect::<Vec<_>>(), incoll.clone()) {
                    return false;
                }
                derived(&inner, ty, incoll)
            }
            Expr::IsDistinct(a, b, _, def) | Expr::NullIf(a, b, def, _) => {
                if matches!(e, Expr::NullIf(..)) || !self.object_shippable(def.oid) {
                    return false;
                }
                if !self.walk(a, &mut inner) || !self.walk(b, &mut inner) {
                    return false;
                }
                if !input_ok(&inner, &[a, b], def.coll.as_ref().map(|c| c.name.clone())) {
                    return false;
                }
                (Coll::None, None)
            }
            Expr::InList(lhs, items, _) => {
                if !self.walk(lhs, &mut inner) {
                    return false;
                }
                for (x, op) in items {
                    if !self.object_shippable(op.oid) || !self.walk(x, &mut inner) {
                        return false;
                    }
                }
                let coll = items.first().and_then(|(_, op)| op.coll.as_ref().map(|c| c.name.clone()));
                if !input_ok(&inner, &[lhs], coll) {
                    return false;
                }
                (Coll::None, None)
            }
            Expr::ArrayOp { left, right, op, .. } => {
                if !self.object_shippable(op.oid) || !self.walk(left, &mut inner) || !self.walk(right, &mut inner) {
                    return false;
                }
                if !input_ok(&inner, &[left], op.coll.as_ref().map(|c| c.name.clone())) {
                    return false;
                }
                (Coll::None, None)
            }
            Expr::Cast(x, _, to, _) | Expr::Relabel(x, to) => {
                if !self.walk(x, &mut inner) {
                    return false;
                }
                derived(&inner, to, None)
            }
            Expr::And(xs) | Expr::Or(xs) => {
                if !xs.iter().all(|x| self.walk(x, &mut inner)) {
                    return false;
                }
                (Coll::None, None)
            }
            Expr::Not(x) | Expr::IsNull(x, _) => {
                if !self.walk(x, &mut inner) {
                    return false;
                }
                (Coll::None, None)
            }
            Expr::Case { operand, whens, else_, ty } => {
                let mut ok = operand.as_ref().is_none_or(|o| self.walk(o, &mut (Coll::None, None)));
                for (w, t) in whens {
                    ok &= self.walk(w, &mut (Coll::None, None)) && self.walk(t, &mut inner);
                }
                ok &= else_.as_ref().is_none_or(|x| self.walk(x, &mut inner));
                if !ok {
                    return false;
                }
                derived(&inner, ty, None)
            }
            Expr::CaseTest(_) => (Coll::None, None),
            Expr::Array(xs, ty) => {
                if !xs.iter().all(|x| self.walk(x, &mut inner)) {
                    return false;
                }
                derived(&inner, ty, None)
            }
            Expr::Subscript(base, subs, ty) => {
                if !self.walk(base, &mut inner) {
                    return false;
                }
                for (lo, hi, _) in subs {
                    if !lo.as_ref().is_none_or(|x| self.walk(x, &mut (Coll::None, None)))
                        || !hi.as_ref().is_none_or(|x| self.walk(x, &mut (Coll::None, None)))
                    {
                        return false;
                    }
                }
                derived(&inner, ty, None)
            }
            _ => return false,
        };
        if !self.type_shippable(&e.ty()) {
            return false;
        }
        if state > outer.0 {
            *outer = (state, coll);
        } else if state == outer.0 && state == Coll::Safe && coll != outer.1 {
            if outer.1.as_deref() == Some("default") {
                outer.1 = coll;
            } else if coll.as_deref() != Some("default") {
                outer.0 = Coll::Unsafe;
            }
        }
        true
    }
}

/// contain_mutable_functions: a call that isn't immutable.
fn has_stable(e: &Expr) -> bool {
    let mut found = false;
    e.walk(&mut |x| {
        let v = match x {
            Expr::Call(d, ..) => d.volatility,
            Expr::IsDistinct(_, _, _, d) | Expr::NullIf(_, _, d, _) => d.volatility,
            Expr::ArrayOp { op, .. } => op.volatility,
            Expr::InList(_, items, _) => items.first().map_or('i', |(_, d)| d.volatility),
            _ => 'i',
        };
        found |= v != 'i';
    });
    found
}

impl Deparser<'_> {
    /// A column reference (deparseColumnRef), qualified in joins.
    pub fn column(&self, i: usize) -> String {
        let name = if Some(i) == self.ctid_col { "ctid".to_string() } else { quote_ident(&column_name(self.table, i)) };
        match &self.alias {
            Some(a) => format!("{a}.{name}"),
            None => name,
        }
    }

    /// printRemoteParam: `$n::type`, `n` the place of `e` among the query's parameters.
    fn param(&self, e: &Expr) -> String {
        let mut ps = self.params.borrow_mut();
        let n = match ps.iter().position(|p| p.same_as(e)) {
            Some(i) => i + 1,
            None => {
                ps.push(e.clone());
                ps.len()
            }
        };
        format!("${n}::{}", type_name(self.cat, &e.ty()))
    }

    /// deparseConst with `showtype` (-2: no label on a quoted literal, 0: label when the
    /// type isn't what the literal reads as, 1: always).
    pub fn constant(&self, v: &Value, ty: &DataType, showtype: i32) -> String {
        if v.is_null() {
            return if showtype >= 0 { format!("NULL::{}", type_name(self.cat, ty)) } else { "NULL".into() };
        }
        let text = crate::types::text::format_value(v, ty, &self.fmt);
        let (mut isfloat, mut isstring) = (false, false);
        let mut out = match ty {
            DataType::Int2 | DataType::Int4 | DataType::Int8 | DataType::Oid | DataType::Float4 | DataType::Float8 | DataType::Numeric(_) => {
                if text.chars().all(|c| "0123456789+-eE.".contains(c)) {
                    isfloat = text.contains(['e', 'E', '.']);
                    if text.starts_with(['+', '-']) { format!("({text})") } else { text.clone() }
                } else {
                    format!("'{text}'")
                }
            }
            DataType::Bit(_) | DataType::Varbit(_) => format!("B'{text}'"),
            DataType::Bool => if text == "t" { "true".into() } else { "false".into() },
            _ => {
                isstring = true;
                string_literal(&text)
            }
        };
        if showtype == -1 {
            return out;
        }
        let needlabel = match ty {
            DataType::Bool | DataType::Int4 | DataType::Unknown => false,
            DataType::Numeric(_) => !isfloat || ty.typmod() >= 0,
            _ => showtype != -2 || !isstring,
        };
        if needlabel || showtype > 0 {
            out.push_str(&format!("::{}", type_name(self.cat, ty)));
        }
        out
    }

    fn plain_var(&self, e: &Expr) -> bool {
        let e = match e {
            Expr::Relabel(x, _) => x,
            Expr::Cast(x, DataType::Varchar(_), DataType::Text, false) => x,
            e => e,
        };
        matches!(e, Expr::Col(i, _) if *i < self.table.columns.len() || Some(*i) == self.ctid_col)
    }

    /// deparseExpr.
    pub fn expr(&self, e: &Expr) -> String {
        match e {
            Expr::Col(i, _) => self.column(*i),
            Expr::Const(v, ty) => self.constant(v, ty, 0),
            Expr::Outer(..) | Expr::Param(..) => self.param(e),
            Expr::Call(def, args, _) if is_operator(def.name) => {
                if args.len() == 1 {
                    return format!("({} {})", def.name, self.expr(&args[0]));
                }
                let (l, r) = (&args[0], &args[1]);
                let same = l.ty().oid() == r.ty().oid();
                let ls = match l {
                    Expr::Const(v, t) if same && self.plain_var(r) => self.constant(v, t, -2),
                    l => self.expr(l),
                };
                let rs = match r {
                    Expr::Const(v, t) if same && !matches!(l, Expr::Const(..)) && self.plain_var(l) => self.constant(v, t, -2),
                    r => self.expr(r),
                };
                format!("({ls} {} {rs})", def.name)
            }
            Expr::Call(def, args, _) => {
                let args: Vec<String> = args.iter().map(|a| self.expr(a)).collect();
                format!("{}({})", quote_ident(def.name), args.join(", "))
            }
            Expr::Cast(x, _, to, true) => format!("{}::{}", self.expr(x), type_name(self.cat, to)),
            Expr::Cast(x, ..) | Expr::Relabel(x, _) => self.expr(x),
            Expr::And(xs) => format!("({})", xs.iter().map(|x| self.expr(x)).collect::<Vec<_>>().join(" AND ")),
            Expr::Or(xs) => format!("({})", xs.iter().map(|x| self.expr(x)).collect::<Vec<_>>().join(" OR ")),
            Expr::Not(x) => format!("(NOT {})", self.expr(x)),
            Expr::IsNull(x, neg) => format!("({} IS {}NULL)", self.expr(x), if *neg { "NOT " } else { "" }),
            Expr::IsDistinct(a, b, neg, _) => {
                let d = format!("({} IS DISTINCT FROM {})", self.expr(a), self.expr(b));
                if *neg { format!("(NOT {d})") } else { d }
            }
            Expr::InList(lhs, items, neg) => {
                let op = items.first().map_or(if *neg { "<>" } else { "=" }, |(_, d)| d.name);
                let quant = if *neg { "ALL" } else { "ANY" };
                let arr = if items.iter().all(|(x, _)| matches!(x, Expr::Const(..))) {
                    let ety = items.first().map_or(DataType::Text, |(x, _)| x.ty());
                    let vals: Vec<Value> = items.iter().map(|(x, _)| match x { Expr::Const(v, _) => v.clone(), _ => Value::Null }).collect();
                    let arr = Value::Array(std::sync::Arc::new(crate::types::array::ArrayValue::from_vec(ety.clone(), vals)));
                    self.constant(&arr, &DataType::Array(Box::new(ety)), 0)
                } else {
                    format!("ARRAY[{}]", items.iter().map(|(x, _)| self.expr(x)).collect::<Vec<_>>().join(", "))
                };
                format!("({} {op} {quant} ({arr}))", self.expr(lhs))
            }
            Expr::ArrayOp { left, right, op, all } => {
                format!("({} {} {} ({}))", self.expr(left), op.name, if *all { "ALL" } else { "ANY" }, self.expr(right))
            }
            Expr::Case { operand, whens, else_, .. } => {
                let mut s = "(CASE".to_string();
                if let Some(o) = operand {
                    s.push_str(&format!(" {}", self.expr(o)));
                }
                for (w, t) in whens {
                    let w = match (operand, w) {
                        (Some(_), Expr::Call(_, a, _)) if a.len() == 2 => self.expr(&a[1]),
                        (_, w) => self.expr(w),
                    };
                    s.push_str(&format!(" WHEN {w} THEN {}", self.expr(t)));
                }
                if let Some(x) = else_ {
                    s.push_str(&format!(" ELSE {}", self.expr(x)));
                }
                s.push_str(" END)");
                s
            }
            Expr::Array(xs, ty) => {
                let mut s = format!("ARRAY[{}]", xs.iter().map(|x| self.expr(x)).collect::<Vec<_>>().join(", "));
                if xs.is_empty() {
                    s.push_str(&format!("::{}", type_name(self.cat, ty)));
                }
                s
            }
            Expr::Subscript(base, subs, _) => {
                let mut s = match &**base {
                    Expr::Col(..) => self.expr(base),
                    b => format!("({})", self.expr(b)),
                };
                for (lo, hi, slice) in subs {
                    s.push('[');
                    if let Some(l) = lo {
                        s.push_str(&self.expr(l));
                    }
                    if *slice {
                        s.push(':');
                        if let Some(h) = hi {
                            s.push_str(&self.expr(h));
                        }
                    } else if let Some(h) = hi {
                        s.push_str(&self.expr(h));
                    }
                    s.push(']');
                }
                s
            }
            e => format!("{e:?}"),
        }
    }

    /// appendConditions: `(cond) AND (cond)` (under the transmission modes).
    pub fn conditions(&self, conds: &[Expr]) -> String {
        conds.iter().map(|c| format!("({})", self.expr(c))).collect::<Vec<_>>().join(" AND ")
    }
}
