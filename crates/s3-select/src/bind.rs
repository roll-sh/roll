//! Binding: resolve names against the input (CSV columns, JSON attributes), inline SELECT-list
//! aliases (CSV), and pull aggregates out into slots.

use crate::ast::*;
use crate::error::{Result, SelectError};
use crate::value::Value;

/// Depth limit for the bound tree (alias inlining can deepen it).
pub const MAX_BOUND_DEPTH: usize = 250;
/// Node limit for the bound tree (alias inlining can multiply it).
pub const MAX_BOUND_NODES: usize = 100_000;

pub(crate) enum Schema<'a> {
    Csv {
        /// The header's names (FileHeaderInfo USE).
        header: Option<&'a [String]>,
        /// FileHeaderInfo NONE/IGNORE: no column has a name, so a double-quoted name that isn't
        /// `_N` or an alias is read as a string literal (Ceph's dialect).
        quoted_literals: bool,
        /// The table alias (`FROM S3Object s`).
        alias: Option<&'a Name>,
    },
    Json {
        /// Names that refer to the row itself.
        aliases: Vec<Name>,
    },
}

/// A query resolved against the input.
#[derive(Debug)]
pub(crate) struct Bound {
    /// `SELECT *`.
    pub star: bool,
    /// Output names and expressions.
    pub columns: Vec<(String, Expr)>,
    pub filter: Option<Expr>,
    /// The aggregates (an aggregate query returns one record).
    pub aggs: Vec<(AggFunc, Option<Expr>)>,
    pub aggregate: bool,
    /// CSV: how many leading columns the query reads (`usize::MAX` for all).
    pub needed_columns: usize,
}

pub(crate) fn bind(q: &Select, schema: &Schema<'_>) -> Result<Bound> {
    let mut b = Binder { q, schema, aggs: Vec::new(), alias_stack: Vec::new(), depth: 0, nodes: 0, in_agg: false, allow_agg: true, row_outside_agg: false, has_agg: false };
    let mut columns = Vec::new();
    let mut aggregate = false;
    let mut mixed = false;
    let star = matches!(q.items, SelectList::Star);
    if let SelectList::Items(items) = &q.items {
        for (pos, item) in items.iter().enumerate() {
            b.row_outside_agg = false;
            b.has_agg = false;
            let e = b.bind(&item.expr)?;
            aggregate |= b.has_agg;
            mixed |= b.row_outside_agg;
            let name = match &item.alias {
                Some(a) => a.text.clone(),
                None => match (&item.expr, &e) {
                    (_, Expr::Lit(_)) => format!("_{}", pos + 1),
                    (Expr::Ident(n), _) => n.text.clone(),
                    (Expr::Path(_, steps), _) => match steps.last() {
                        Some(Step::Name(n)) => n.text.clone(),
                        _ => format!("_{}", pos + 1),
                    },
                    _ => format!("_{}", pos + 1),
                },
            };
            columns.push((name, e));
        }
    }
    if aggregate && mixed {
        return Err(SelectError::new(
            "UnsupportedSqlStructure",
            "Aggregate and non-aggregate expressions can't be mixed in the SELECT list (S3 Select has no GROUP BY).",
        ));
    }
    b.allow_agg = false;
    let filter = q.filter.as_ref().map(|f| b.bind(f)).transpose()?;
    let mut needed = 0usize;
    let mut note = |e: &Expr| {
        if let Expr::Col(i) = e {
            needed = needed.max(i.saturating_add(1));
        }
    };
    for (_, e) in &columns {
        e.walk(&mut note);
    }
    if let Some(f) = &filter {
        f.walk(&mut note);
    }
    for (_, a) in &b.aggs {
        if let Some(a) = a {
            a.walk(&mut note);
        }
    }
    Ok(Bound { star, columns, filter, aggs: b.aggs, aggregate, needed_columns: if star { usize::MAX } else { needed } })
}

/// `_N` (1-based) as a column index.
fn positional(name: &str) -> Option<Result<usize>> {
    let digits = name.strip_prefix('_')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(match digits.parse::<usize>() {
        Ok(n) if n >= 1 => Ok(n - 1),
        _ => Err(SelectError::new("InvalidColumnIndex", format!("The column index {name} is not valid: columns are numbered from _1."))),
    })
}

struct Binder<'a> {
    q: &'a Select,
    schema: &'a Schema<'a>,
    aggs: Vec<(AggFunc, Option<Expr>)>,
    alias_stack: Vec<usize>,
    depth: usize,
    nodes: usize,
    in_agg: bool,
    allow_agg: bool,
    row_outside_agg: bool,
    has_agg: bool,
}

impl Binder<'_> {
    fn too_complex() -> SelectError {
        SelectError::new("MaxOperatorsExceeded", "Failed to parse SQL expression, try reducing complexity.")
    }

    fn row_ref(&mut self) {
        if !self.in_agg {
            self.row_outside_agg = true;
        }
    }

    /// A CSV column by name: a header name, else `_N`.
    fn column(&self, name: &Name) -> Result<Option<usize>> {
        if let Schema::Csv { header: Some(h), .. } = self.schema {
            let mut found = None;
            for (i, col) in h.iter().enumerate() {
                if name.matches(col) {
                    if found.is_some() {
                        return Err(SelectError::new(
                            "AmbiguousFieldName",
                            format!("The field name {} matches multiple fields in the file. Check the SQL expression and the file, and try again.", name.text),
                        ));
                    }
                    found = Some(i);
                }
            }
            if found.is_some() {
                return Ok(found);
            }
        }
        positional(&name.text).transpose()
    }

    /// A bare name in CSV: a column, else a SELECT-list alias, else (no header) a double-quoted
    /// string literal.
    fn csv_name(&mut self, name: &Name) -> Result<Expr> {
        if let Some(i) = self.column(name)? {
            self.row_ref();
            return Ok(Expr::Col(i));
        }
        let q = self.q;
        if let SelectList::Items(items) = &q.items {
            let hit = items.iter().position(|it| {
                it.alias.as_ref().is_some_and(|a| if name.quoted || a.quoted { a.text == name.text } else { a.text.eq_ignore_ascii_case(&name.text) })
            });
            if let Some(idx) = hit {
                if self.alias_stack.contains(&idx) {
                    return Err(SelectError::new(
                        "ParseUnsupportedAlias",
                        format!("number of calls exceed maximum size, probably a cyclic reference to alias {}.", name.text),
                    ));
                }
                self.alias_stack.push(idx);
                let e = self.bind(&items[idx].expr);
                self.alias_stack.pop();
                return e;
            }
        }
        match self.schema {
            Schema::Csv { quoted_literals: true, .. } if name.quoted => Ok(Expr::Lit(Value::Str(name.text.clone()))),
            Schema::Csv { header: Some(_), .. } => Err(SelectError::new(
                "MissingHeaders",
                format!("Some headers in the query are missing from the file ({}). Please check the file and try again.", name.text),
            )),
            _ => Err(SelectError::new(
                "EvaluatorBindingDoesNotExist",
                format!("The column {} does not exist: without a header (FileHeaderInfo USE), columns are _1, _2, ….", name.text),
            )),
        }
    }

    fn is_json_alias(&self, name: &Name) -> bool {
        match self.schema {
            Schema::Json { aliases } => aliases.iter().any(|a| a.matches(&name.text) || name.matches(&a.text)),
            _ => false,
        }
    }

    fn is_csv_alias(&self, name: &Name) -> bool {
        match self.schema {
            Schema::Csv { alias: Some(a), .. } => a.matches(&name.text) || name.matches(&a.text),
            Schema::Csv { alias: None, .. } => name.matches("s3object") || name.matches("stdin"),
            _ => false,
        }
    }

    fn ident(&mut self, name: &Name) -> Result<Expr> {
        match self.schema {
            Schema::Csv { .. } => self.csv_name(name),
            Schema::Json { .. } => {
                self.row_ref();
                Ok(if self.is_json_alias(name) {
                    Expr::Row
                } else if !name.quoted && name.text == "_1" {
                    Expr::RowOr1
                } else {
                    Expr::Path(Box::new(Expr::Row), vec![Step::Name(name.clone())])
                })
            }
        }
    }

    fn path(&mut self, base: &Expr, steps: &[Step]) -> Result<Expr> {
        let Expr::Ident(name) = base else {
            let b = self.bind(base)?;
            return Ok(Expr::Path(Box::new(b), steps.to_vec()));
        };
        match self.schema {
            Schema::Csv { .. } => {
                if self.is_csv_alias(name)
                    && let Some(Step::Name(first)) = steps.first()
                {
                    let col = self.csv_name(first)?;
                    return Ok(if steps.len() == 1 { col } else { Expr::Path(Box::new(col), steps[1..].to_vec()) });
                }
                let b = self.csv_name(name)?;
                Ok(Expr::Path(Box::new(b), steps.to_vec()))
            }
            Schema::Json { .. } => {
                self.row_ref();
                if self.is_json_alias(name) {
                    Ok(Expr::Path(Box::new(Expr::Row), steps.to_vec()))
                } else if !name.quoted && name.text == "_1" {
                    Ok(Expr::Path(Box::new(Expr::RowOr1), steps.to_vec()))
                } else {
                    let mut all = vec![Step::Name(name.clone())];
                    all.extend_from_slice(steps);
                    Ok(Expr::Path(Box::new(Expr::Row), all))
                }
            }
        }
    }

    fn bind(&mut self, e: &Expr) -> Result<Expr> {
        self.depth += 1;
        self.nodes += 1;
        if self.depth > MAX_BOUND_DEPTH || self.nodes > MAX_BOUND_NODES {
            return Err(Self::too_complex());
        }
        let r = self.bind_inner(e);
        self.depth -= 1;
        r
    }

    fn bx(&mut self, e: &Expr) -> Result<Box<Expr>> {
        Ok(Box::new(self.bind(e)?))
    }

    fn bxo(&mut self, e: Option<&Expr>) -> Result<Option<Box<Expr>>> {
        e.map(|x| self.bx(x)).transpose()
    }

    /// A small dispatcher: each node kind is bound by a small helper, so each level of the tree
    /// costs little stack (even in debug builds).
    fn bind_inner(&mut self, e: &Expr) -> Result<Expr> {
        match e {
            Expr::Lit(v) => Ok(Expr::Lit(v.clone())),
            Expr::Ident(n) => self.ident(n),
            Expr::Path(base, steps) => self.path(base, steps),
            Expr::Neg(a) => self.unary(a, &Expr::Neg),
            Expr::Not(a) => self.unary(a, &Expr::Not),
            Expr::Cast(a, t) => self.unary(a, &|x| Expr::Cast(x, *t)),
            Expr::Extract(p, a) => self.unary(a, &|x| Expr::Extract(*p, x)),
            Expr::Is { expr, what, negated } => self.unary(expr, &|x| Expr::Is { expr: x, what: *what, negated: *negated }),
            Expr::Arith(op, a, b) => self.binary(a, b, &|x, y| Expr::Arith(*op, x, y)),
            Expr::Cmp(op, a, b) => self.binary(a, b, &|x, y| Expr::Cmp(*op, x, y)),
            Expr::And(a, b) => self.binary(a, b, &Expr::And),
            Expr::Or(a, b) => self.binary(a, b, &Expr::Or),
            Expr::Concat(a, b) => self.binary(a, b, &Expr::Concat),
            Expr::DateAdd(p, a, b) => self.binary(a, b, &|x, y| Expr::DateAdd(*p, x, y)),
            Expr::DateDiff(p, a, b) => self.binary(a, b, &|x, y| Expr::DateDiff(*p, x, y)),
            Expr::Like { expr, pattern, escape, negated } => self.like(expr, pattern, escape.as_deref(), *negated),
            Expr::Between { expr, low, high, negated } => self.between(expr, low, high, *negated),
            Expr::In { expr, list, negated } => self.in_list(expr, list, *negated),
            Expr::Case { operand, whens, otherwise } => self.case(operand.as_deref(), whens, otherwise.as_deref()),
            Expr::Call(f, args) => self.call(*f, args),
            Expr::Trim { mode, chars, expr } => self.trim(*mode, chars.as_deref(), expr),
            Expr::Agg(f, arg) => self.agg(*f, arg.as_deref()),
            Expr::Col(_) | Expr::Row | Expr::RowOr1 | Expr::AggRef(_) => Ok(e.clone()),
        }
    }

    #[inline(never)]
    fn unary(&mut self, a: &Expr, mk: &dyn Fn(Box<Expr>) -> Expr) -> Result<Expr> {
        Ok(mk(self.bx(a)?))
    }

    #[inline(never)]
    fn binary(&mut self, a: &Expr, b: &Expr, mk: &dyn Fn(Box<Expr>, Box<Expr>) -> Expr) -> Result<Expr> {
        let x = self.bx(a)?;
        let y = self.bx(b)?;
        Ok(mk(x, y))
    }

    #[inline(never)]
    fn like(&mut self, expr: &Expr, pattern: &Expr, escape: Option<&Expr>, negated: bool) -> Result<Expr> {
        let expr = self.bx(expr)?;
        let pattern = self.bx(pattern)?;
        let escape = self.bxo(escape)?;
        Ok(Expr::Like { expr, pattern, escape, negated })
    }

    #[inline(never)]
    fn between(&mut self, expr: &Expr, low: &Expr, high: &Expr, negated: bool) -> Result<Expr> {
        let expr = self.bx(expr)?;
        let low = self.bx(low)?;
        let high = self.bx(high)?;
        Ok(Expr::Between { expr, low, high, negated })
    }

    #[inline(never)]
    fn in_list(&mut self, expr: &Expr, list: &[Expr], negated: bool) -> Result<Expr> {
        let expr = self.bx(expr)?;
        let mut out = Vec::with_capacity(list.len());
        for x in list {
            out.push(self.bind(x)?);
        }
        Ok(Expr::In { expr, list: out, negated })
    }

    #[inline(never)]
    fn case(&mut self, operand: Option<&Expr>, whens: &[(Expr, Expr)], otherwise: Option<&Expr>) -> Result<Expr> {
        let operand = self.bxo(operand)?;
        let mut ws = Vec::with_capacity(whens.len());
        for (w, t) in whens {
            let w = self.bind(w)?;
            let t = self.bind(t)?;
            ws.push((w, t));
        }
        let otherwise = self.bxo(otherwise)?;
        Ok(Expr::Case { operand, whens: ws, otherwise })
    }

    #[inline(never)]
    fn call(&mut self, f: Func, args: &[Expr]) -> Result<Expr> {
        let mut out = Vec::with_capacity(args.len());
        for x in args {
            out.push(self.bind(x)?);
        }
        Ok(Expr::Call(f, out))
    }

    #[inline(never)]
    fn trim(&mut self, mode: TrimMode, chars: Option<&Expr>, expr: &Expr) -> Result<Expr> {
        let chars = self.bxo(chars)?;
        let expr = self.bx(expr)?;
        Ok(Expr::Trim { mode, chars, expr })
    }

    #[inline(never)]
    fn agg(&mut self, f: AggFunc, arg: Option<&Expr>) -> Result<Expr> {
        if !self.allow_agg {
            return Err(SelectError::new("UnsupportedSqlOperation", "Aggregate functions are only allowed in the SELECT list."));
        }
        if self.in_agg {
            return Err(SelectError::new("UnsupportedSqlOperation", "Aggregate functions can't be nested."));
        }
        self.in_agg = true;
        let arg = arg.map(|a| self.bind(a));
        self.in_agg = false;
        let arg = arg.transpose()?;
        self.has_agg = true;
        self.aggs.push((f, arg));
        Ok(Expr::AggRef(self.aggs.len() - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn csv(sql: &str, header: Option<&[String]>, quoted_literals: bool) -> Result<Bound> {
        let q = parse(sql).unwrap();
        bind(&q, &Schema::Csv { header, quoted_literals, alias: q.from.alias.as_ref() })
    }

    fn json(sql: &str, aliases: &[&str]) -> Result<Bound> {
        let q = parse(sql).unwrap();
        let aliases = aliases.iter().map(|a| Name { text: a.to_string(), quoted: false }).collect();
        bind(&q, &Schema::Json { aliases })
    }

    #[test]
    fn csv_columns() {
        // `s3object` isn't the alias when one is declared: it's a column name, which doesn't exist.
        assert_eq!(csv("select s3object._2 from s3object s", None, true).unwrap_err().code, "EvaluatorBindingDoesNotExist");
        assert_eq!(csv("select s3object._2 from s3object", None, true).unwrap().columns[0].1, Expr::Col(1));
        let b = csv("select _1, s._3 from s3object s where _2 > 1", None, true).unwrap();
        assert_eq!(b.columns[0], ("_1".into(), Expr::Col(0)));
        assert_eq!(b.columns[1], ("_3".into(), Expr::Col(2)));
        assert_eq!(b.needed_columns, 3);
        let h: Vec<String> = ["Id", "name", "NAME2"].iter().map(|s| s.to_string()).collect();
        let b = csv("select id, s.\"name\", name2 from s3object s", Some(&h), false).unwrap();
        assert_eq!(b.columns.iter().map(|c| c.1.clone()).collect::<Vec<_>>(), vec![Expr::Col(0), Expr::Col(1), Expr::Col(2)]);
        assert_eq!(csv("select \"ID\" from s3object", Some(&h), false).unwrap_err().code, "MissingHeaders");
        let dup: Vec<String> = ["a", "A"].iter().map(|s| s.to_string()).collect();
        assert_eq!(csv("select a from s3object", Some(&dup), false).unwrap_err().code, "AmbiguousFieldName");
        assert_eq!(csv("select \"A\" from s3object", Some(&dup), false).unwrap().columns[0].1, Expr::Col(1));
        assert_eq!(csv("select _0 from s3object", None, true).unwrap_err().code, "InvalidColumnIndex");
        assert_eq!(csv("select foo from s3object", None, true).unwrap_err().code, "EvaluatorBindingDoesNotExist");
        assert_eq!(csv("select * from s3object", None, true).unwrap().needed_columns, usize::MAX);
    }

    #[test]
    fn ceph_quoted_literals_only_without_header() {
        let b = csv("select \"abc\" from s3object", None, true).unwrap();
        assert_eq!(b.columns[0], ("_1".into(), Expr::Lit(Value::Str("abc".into()))));
        let h: Vec<String> = vec!["x".into()];
        assert_eq!(csv("select \"abc\" from s3object", Some(&h), false).unwrap_err().code, "MissingHeaders");
        // A quoted `_N` is still a column.
        assert_eq!(csv("select \"_2\" from s3object", None, true).unwrap().columns[0].1, Expr::Col(1));
    }

    #[test]
    fn aliases_inline_and_cycles_fail() {
        let b = csv("select int(_1) as a1, int(_2) as a2, (a1+a2) as a3 from s3object where a3 > 100", None, true).unwrap();
        assert_eq!(b.columns[2].0, "a3");
        assert!(matches!(&b.columns[2].1, Expr::Arith(ArithOp::Add, a, _) if matches!(**a, Expr::Cast(..))));
        assert!(matches!(&b.filter, Some(Expr::Cmp(CmpOp::Gt, a, _)) if matches!(**a, Expr::Arith(..))));
        let e = csv("select int(_1) as a1, a1+a4 as a3, a5+a1 as a4, int(_3)+a3 as a5 from s3object", None, true).unwrap_err();
        assert_eq!(e.code, "ParseUnsupportedAlias");
        assert!(e.message.contains("cyclic reference to alias"));
        // A header column wins over an alias of the same name.
        let h: Vec<String> = vec!["c1".into(), "c4".into()];
        let b = csv("select c1 as c4, c4 from s3object", Some(&h), false).unwrap();
        assert_eq!(b.columns[1].1, Expr::Col(1));
        // Exponential alias inlining is bounded.
        let mut sql = "select _1 as a0".to_string();
        for i in 1..40 {
            sql += &format!(", a{} || a{} as a{i}", i - 1, i - 1);
        }
        sql += " from s3object";
        assert_eq!(csv(&sql, None, true).unwrap_err().code, "MaxOperatorsExceeded");
    }

    #[test]
    fn aggregates() {
        let b = csv("select count(*), sum(int(_1)) + 1 from s3object where _2 = 'x'", None, true).unwrap();
        assert!(b.aggregate);
        assert_eq!(b.aggs.len(), 2);
        assert_eq!(b.columns[1].0, "_2");
        assert_eq!(csv("select _1, count(*) from s3object", None, true).unwrap_err().code, "UnsupportedSqlStructure");
        assert_eq!(csv("select count(*) from s3object where count(*) > 1", None, true).unwrap_err().code, "UnsupportedSqlOperation");
        assert_eq!(csv("select sum(count(*)) from s3object", None, true).unwrap_err().code, "UnsupportedSqlOperation");
        assert!(!csv("select 1 + 2 from s3object", None, true).unwrap().aggregate);
    }

    #[test]
    fn json_paths() {
        let b = json("select s.a.b, x, _1.c, s from s3object[*] s", &["s"]).unwrap();
        let n = |t: &str| Step::Name(Name { text: t.into(), quoted: false });
        assert_eq!(b.columns[0], ("b".into(), Expr::Path(Box::new(Expr::Row), vec![n("a"), n("b")])));
        assert_eq!(b.columns[1], ("x".into(), Expr::Path(Box::new(Expr::Row), vec![n("x")])));
        assert_eq!(b.columns[2], ("c".into(), Expr::Path(Box::new(Expr::RowOr1), vec![n("c")])));
        assert_eq!(b.columns[3], ("s".into(), Expr::Row));
        let b = json("select id from s3object[*].Rules[*].id", &["id"]).unwrap();
        assert_eq!(b.columns[0], ("id".into(), Expr::Row));
    }
}
