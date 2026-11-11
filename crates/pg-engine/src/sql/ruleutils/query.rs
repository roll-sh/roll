//! `pg_get_viewdef`: a view's query printed like Postgres's `get_query_def`. Each query level
//! (sublinks, FROM subqueries, WITH queries, set-operation branches) is bound again, in deparse
//! mode, as it prints, so its names resolve the way they did when the view was made.

use super::{Deparser, JOIN, STD};
use crate::catalog::quote_ident;
use crate::exec::ExecCtx;
use crate::sql::binder::{Binder, Clause, Level, ast, figure_colname};
use crate::sql::expr::{Expr, Subquery, SubqueryKind};
use crate::sql::plan::{AggCall, FrameBound, Plan, SortKey, SrfCall, WinFunc, WindowCall};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

mod body;
pub(super) use body::function_body;
mod rule;
pub(super) use rule::rule_def;

/// `PRETTYINDENT_LIMIT`: past it, deeper levels indent less (and wrap around).
const LIMIT: i32 = 40;

/// `appendContextKeyword`: a new line at the indentation level (plus `plus`), then `s`.
pub(super) fn keyword(
    buf: &mut String,
    indent: &mut i32,
    s: &str,
    before: i32,
    after: i32,
    plus: i32,
) {
    if super::NO_INDENT.get() {
        buf.push_str(s);
        return;
    }
    *indent += before;
    while buf.ends_with(' ') {
        buf.pop();
    }
    buf.push('\n');
    let amount = if *indent < LIMIT {
        (*indent).max(0) + plus
    } else {
        (LIMIT + (*indent - LIMIT) / (STD / 2)) % LIMIT + plus
    };
    buf.extend(std::iter::repeat_n(' ', amount.max(0) as usize));
    buf.push_str(s);
    *indent = (*indent + after).max(0);
}

/// A window specification as written, its expressions bound.
pub(super) struct Spec {
    refname: String,
    partition: Vec<Expr>,
    order: Vec<SortKey>,
    frame: i32,
    start: Option<Expr>,
    end: Option<Expr>,
}

/// A window function call's OVER: a named window, or a specification.
pub(super) enum Over {
    Name(String),
    Spec(Box<Spec>),
}

/// A view query level being printed.
#[derive(Default)]
pub(super) struct ViewLevel {
    /// The level's FROM columns by row position, as the level shows them.
    pub cols: Vec<String>,
    /// The same, always qualified (as queries nested in it show them).
    pub qual: Vec<String>,
    /// The enclosing level's values it reads (`Outer(1, i)`).
    pub outer: Vec<Vec<String>>,
    /// Range-table names of the scope's relations ("" for a USING join's merged columns).
    refs: Vec<String>,
    /// Window function calls (`Expr::Window(i)`): the call, its name and OVER.
    windows: Vec<(WindowCall, String, Over)>,
    /// The select list: (output name, expression).
    targets: Vec<(String, Expr)>,
    /// FULL JOIN USING's merged columns: (value, name), shown by name.
    merged: Vec<(Expr, String)>,
    /// GROUP BY expressions (what GROUPING() arguments stand for).
    groups: Vec<Expr>,
    /// Set-returning calls in the select list (`Expr::Srf(i)`).
    srfs: Vec<SrfCall>,
    /// Aliased joins' insides, by `JoinExpr` address.
    joins: Vec<(usize, ViewLevel)>,
    /// An aliased join's inside: the scope its ON conditions bind in.
    scope: Option<crate::sql::binder::Scope>,
    /// Each relation's range-table name and the row positions of its columns: a ROW of
    /// exactly those is the relation's whole-row reference (`t.*`).
    pub rows: Vec<(String, Vec<usize>)>,
}

/// The whole-row references a level can make: each relation's columns' row positions.
pub(super) fn whole_rows(
    scope: &crate::sql::binder::Scope,
    refs: &[String],
) -> Vec<(String, Vec<usize>)> {
    let mut out = Vec::new();
    for (r, refname) in scope.rels.iter().zip(refs) {
        if refname.is_empty() || r.anonymous {
            continue;
        }
        let pos: Option<Vec<usize>> = (r.cols.iter())
            .filter(|c| !c.hidden && !c.merged)
            .map(|c| match c.expr {
                Expr::Col(i, _) => Some(i),
                _ => None,
            })
            .collect();
        if let Some(pos) = pos.filter(|p| !p.is_empty()) {
            out.push((refname.clone(), pos));
        }
    }
    out
}

/// Where a query is printed (what `get_query_def` is called for).
#[derive(Clone)]
pub(super) enum Nest {
    /// The view's query; the view's column names.
    Top(Vec<String>),
    Sublink,
    From,
    Cte,
    /// A set-operation leaf: the output names (the view's), whether its own names show, and
    /// the output types (what its unknown-typed literals become).
    Branch(Option<Vec<String>>, bool, Vec<DataType>),
    /// A statement of a SQL function body (column names not visible, references
    /// qualified).
    Body,
    /// An INSERT's SELECT: as `Body`, its unknown-typed literals left unresolved (the
    /// target columns' types take them).
    Insert,
    /// A rule's SELECT action (column names visible, references qualified).
    Action,
}

impl Nest {
    fn result(&self) -> Option<&[String]> {
        match self {
            Nest::Top(r) => Some(r),
            Nest::Branch(r, ..) => r.as_deref(),
            _ => None,
        }
    }

    /// `colNamesVisible`.
    fn names_visible(&self) -> bool {
        !matches!(
            self,
            Nest::Sublink | Nest::Branch(_, false, _) | Nest::Body | Nest::Insert
        )
    }

    /// A query nested in another (its column references are always qualified).
    fn nested(&self) -> bool {
        !matches!(self, Nest::Top(_))
    }

    /// Bound at a new level (its outer references are parameters).
    fn new_level(&self) -> bool {
        matches!(self, Nest::Sublink | Nest::From)
    }

    fn types(&self) -> &[DataType] {
        match self {
            Nest::Branch(_, _, t) => t,
            _ => &[],
        }
    }
}

/// What printing a view's expressions needs from the query printer.
pub(super) trait Nested {
    /// Run `f` with the binder at the level being printed.
    fn binder(&mut self, f: &mut dyn FnMut(&mut Binder<'_, '_>));
    /// Print a nested query at indentation `indent`; `qual`: the qualified column names of
    /// the level it is nested in.
    fn query(
        &mut self,
        n: &pb::Node,
        buf: &mut String,
        indent: i32,
        qual: &[String],
        how: Nest,
    ) -> Option<()>;
    /// The query a sublink (subquery `id`) runs, as written.
    fn sublink_node(&self, id: usize) -> Option<pb::Node>;
    /// A WITH query visible by this name.
    fn is_cte(&self, name: &str) -> bool;
    /// The columns a NATURAL join joins on.
    fn natural_using(&self, j: &pb::JoinExpr) -> Option<Vec<String>>;
}

/// Prints a view's query.
struct Views<'v, 'b, 'a> {
    b: &'v mut Binder<'b, 'a>,
    ctx: &'v ExecCtx<'a>,
    pretty: bool,
    wrap: usize,
    /// Range-table names taken by the levels being printed (`set_rtable_names`).
    used: Vec<String>,
    /// For each binder level above the first: the qualified column names of the level below.
    enclosing: Vec<Vec<String>>,
}

/// A select-list item: its expression, its name as written (or figured), and the column's own
/// name when it is a plain column reference.
struct Target {
    expr: Expr,
    name: String,
    own: Option<String>,
}

/// A basic SELECT's parts, bound.
#[derive(Default)]
struct Select {
    distinct: Option<Vec<Expr>>,
    /// (expression, output name, the name it shows without AS)
    targets: Vec<(Expr, String, Option<String>)>,
    /// FROM printed as is (a VALUES RTE).
    from_text: Option<String>,
    where_: Option<Expr>,
    group: Vec<Group>,
    group_distinct: bool,
    having: Option<Expr>,
    windows: Vec<(String, Spec)>,
    order: Vec<SortKey>,
    offset: Option<Expr>,
    limit: Option<Expr>,
    with_ties: bool,
    /// FOR UPDATE / SHARE: (keyword, range-table name, wait policy)
    locks: Vec<(&'static str, String, &'static str)>,
}

/// A GROUP BY item.
enum Group {
    Expr(Expr),
    Empty,
    Simple(Vec<Expr>),
    Rollup(Vec<Group>),
    Cube(Vec<Group>),
    Sets(Vec<Group>),
}

/// A scope relation a FROM item binds: a USING join's merged columns, or a range-table entry
/// (and whether FOR UPDATE without OF locks it).
#[derive(Clone, Copy, PartialEq)]
enum Slot<'n> {
    Merged,
    Rel {
        lockable: bool,
    },
    /// An aliased join (one range-table entry hiding those inside it).
    Join(&'n pb::JoinExpr),
}

/// A view's definition as `pg_get_viewdef` prints it. `cols`: the view's column names;
/// `wrap`: the wrap column (0: one item per line); `rowtypes`: the function result columns
/// the view reads, as recorded (`ViewDef::rowtype_cols`: one dropped since still prints).
/// `None` for queries it can't print.
pub(super) fn view_query(
    ctx: &ExecCtx,
    sql: &str,
    cols: Vec<String>,
    pretty: bool,
    wrap: usize,
    rowtypes: Option<std::sync::Arc<Vec<(u32, i16, String, DataType)>>>,
) -> Option<String> {
    let node = ast::parse_stmt(sql).ok()?;
    let mut b = Binder::new(ctx, vec![]);
    b.deparse = true;
    b.view_rowtypes = rowtypes;
    b.level().barrier = true;
    let mut v = Views {
        b: &mut b,
        ctx,
        pretty,
        wrap,
        used: Vec::new(),
        enclosing: Vec::new(),
    };
    let mut buf = String::new();
    v.query(&node, &mut buf, 0, &[], Nest::Top(cols))?;
    buf.push(';');
    Some(buf)
}

/// An expression stored over `table` that holds subqueries (a policy's), printed with the
/// view printer for them: the table's columns unqualified at its level, qualified by its
/// name inside the subqueries.
pub(super) fn stored_expr_query(
    ctx: &ExecCtx,
    table: &std::sync::Arc<crate::catalog::Table>,
    node: &pb::Node,
    pretty: bool,
) -> Option<String> {
    let mut b = Binder::new(ctx, vec![]);
    b.deparse = true;
    let mut lvl = Level::new();
    lvl.barrier = true;
    lvl.scope = crate::sql::binder::Scope {
        rels: vec![crate::sql::binder::table_rel(table, &table.name, None)],
    };
    b.levels.push(lvl);
    let e = b
        .in_clause(Clause::Where, |b| b.bind_expr(node))
        .ok()?;
    let cols: Vec<String> = table.columns.iter().map(|c| quote_ident(&c.name)).collect();
    let qual: Vec<String> = table
        .columns
        .iter()
        .map(|c| format!("{}.{}", quote_ident(&table.name), quote_ident(&c.name)))
        .collect();
    let mut v = Views {
        b: &mut b,
        ctx,
        pretty,
        wrap: 0,
        used: vec![table.name.clone()],
        enclosing: vec![Vec::new()],
    };
    let level = ViewLevel {
        cols,
        qual,
        refs: vec![table.name.clone()],
        ..ViewLevel::default()
    };
    let mut buf = String::new();
    {
        let _g = crate::scoped::set(&super::RTABLES, super::RTABLES.get() + 1, "ruleutils::RTABLES.expr");
        v.print(&level, 0, &mut buf, &[], |d| d.expr(&e, false))?;
    }
    Some(buf)
}

impl Nested for Views<'_, '_, '_> {
    fn binder(&mut self, f: &mut dyn FnMut(&mut Binder<'_, '_>)) {
        f(self.b)
    }

    fn query(
        &mut self,
        n: &pb::Node,
        buf: &mut String,
        indent: i32,
        qual: &[String],
        how: Nest,
    ) -> Option<()> {
        let Some(NodeEnum::SelectStmt(s)) = n.node.as_ref() else {
            return None;
        };
        crate::stack::check().ok()?;
        let saved = if how.new_level() {
            self.enclosing.push(qual.to_vec());
            self.b.levels.push(Level::new());
            None
        } else {
            let lvl = self.b.level();
            Some((std::mem::take(&mut lvl.scope), std::mem::take(&mut lvl.st)))
        };
        let used = self.used.len();
        let setop = !matches!(
            pb::SetOperation::try_from(s.op),
            Ok(pb::SetOperation::SetopNone | pb::SetOperation::Undefined) | Err(_)
        );
        let rtable = usize::from(!s.from_clause.is_empty() || !s.values_lists.is_empty() || setop);
        let r = {
            let _g = crate::scoped::set(&super::RTABLES, super::RTABLES.get() + rtable, "ruleutils::RTABLES");
            self.select(s, buf, indent, &how)
        };
        self.used.truncate(used);
        match saved {
            None => {
                self.b.levels.pop();
                self.enclosing.pop();
            }
            Some((scope, st)) => {
                let lvl = self.b.level();
                lvl.scope = scope;
                lvl.st = st;
            }
        }
        r
    }

    fn sublink_node(&self, id: usize) -> Option<pb::Node> {
        self.b.sublink_nodes.get(&id).cloned()
    }

    fn is_cte(&self, name: &str) -> bool {
        self.b.ctes.iter().flatten().any(|c| c.name == name)
    }

    fn natural_using(&self, j: &pb::JoinExpr) -> Option<Vec<String>> {
        self.b
            .natural_using
            .get(&(j as *const pb::JoinExpr as usize))
            .cloned()
    }
}

impl Views<'_, '_, '_> {
    /// Print with an expression printer for `level`, writing at `indent` onto `buf`.
    fn print<R>(
        &mut self,
        level: &ViewLevel,
        indent: i32,
        buf: &mut String,
        aggs: &[AggCall],
        f: impl FnOnce(&mut Deparser) -> Option<R>,
    ) -> Option<R> {
        let ctx = self.ctx;
        let cat = ctx.catalog();
        let pretty = self.pretty;
        let mut d = Deparser {
            ctx,
            cat: &cat,
            cols: &level.cols,
            aggs,
            pretty,
            indent,
            buf: std::mem::take(buf),
            outer: &level.outer,
            subplans: None,
            view: Some(level),
            nested: Some(self),
        };
        let r = f(&mut d);
        *buf = d.buf;
        r
    }

    /// How the params of binder level `l` print: the expressions they stand for, over the
    /// level below (whose columns show qualified).
    fn param_names(&self, l: usize) -> Vec<String> {
        let (Some(below), Some(lvl)) = (
            l.checked_sub(1).and_then(|i| self.enclosing.get(i)),
            self.b.levels.get(l),
        ) else {
            return Vec::new();
        };
        let outer = vec![self.param_names(l - 1)];
        let cat = self.ctx.catalog();
        lvl.params
            .iter()
            .map(|p| {
                let mut d = Deparser {
                    ctx: self.ctx,
                    cat: &cat,
                    cols: below,
                    aggs: &[],
                    pretty: self.pretty,
                    indent: 0,
                    buf: String::new(),
                    outer: &outer,
                    subplans: None,
                    view: None,
                    nested: None,
                };
                match d.expr(p, false) {
                    Some(()) => d.buf,
                    None => "?".into(),
                }
            })
            .collect()
    }

    fn outer_names(&self) -> Vec<Vec<String>> {
        vec![self.param_names(self.b.levels.len() - 1)]
    }

    fn bind_opt(&mut self, n: Option<&pb::Node>, clause: Clause) -> Option<Option<Expr>> {
        match n {
            None => Some(None),
            Some(n) => {
                let e = self.b.in_clause(clause, |b| b.bind_expr(n)).ok()?;
                // OFFSET / LIMIT are bigint (transformLimitClause): the coercion shows.
                if matches!(clause, Clause::Limit | Clause::Offset)
                    && !matches!(e, Expr::Const(Value::Null, _))
                    && !matches!(e.ty(), DataType::Int8 | DataType::Unknown)
                {
                    return Some(Some(self.b.coerce_implicit(e, &DataType::Int8).ok()?));
                }
                Some(Some(e))
            }
        }
    }

    /// `get_select_query_def`, with the query's WITH queries visible.
    fn select(
        &mut self,
        s: &pb::SelectStmt,
        buf: &mut String,
        indent: i32,
        how: &Nest,
    ) -> Option<()> {
        if s.into_clause.is_some() {
            return None;
        }
        let Some(w) = &s.with_clause else {
            return self.select_body(s, buf, indent, how);
        };
        self.b.ctes.push(Vec::new());
        let r = if self.b.define_ctes(w).is_ok() {
            self.select_body(s, buf, indent, how)
        } else {
            None
        };
        self.b.ctes.pop();
        r
    }

    fn select_body(
        &mut self,
        s: &pb::SelectStmt,
        buf: &mut String,
        indent: i32,
        how: &Nest,
    ) -> Option<()> {
        let setop = !matches!(
            pb::SetOperation::try_from(s.op).ok()?,
            pb::SetOperation::SetopNone | pb::SetOperation::Undefined
        );
        if !setop && s.values_lists.is_empty() {
            return self.basic(s, buf, indent, how);
        }
        if !s.locking_clause.is_empty() || s.limit_option == pb::LimitOption::WithTies as i32 {
            return None;
        }
        // The output columns: ORDER BY names or numbers them.
        let out = {
            let mut copy = s.clone();
            copy.with_clause = None;
            self.b.in_sibling(|b| b.bind_select_stmt(&copy)).ok()?.1
        };
        let mut indent = indent;
        if !setop {
            let names: Vec<String> = (1..=out.len()).map(|i| format!("column{i}")).collect();
            let simple = how
                .result()
                .is_none_or(|r| r.iter().zip(&names).all(|(a, b)| a == b));
            if !simple {
                return self.values_select(s, buf, indent, how, &names);
            }
        }
        if let Some(w) = &s.with_clause {
            self.with_clause(w, buf, &mut indent)?;
        }
        if setop {
            let types: Vec<DataType> = out.iter().map(|c| c.ty.clone()).collect();
            let result = how.result().map(<[String]>::to_vec);
            self.setop(
                s,
                buf,
                &mut indent,
                &result,
                how.names_visible(),
                &types,
                true,
            )?;
        } else {
            buf.push(' ');
            indent += STD;
            let rows = self.values_text(&s.values_lists)?;
            buf.push_str(&rows);
        }
        let level = ViewLevel {
            outer: self.outer_names(),
            ..ViewLevel::default()
        };
        let mut order = Vec::new();
        for o in &s.sort_clause {
            let Some(NodeEnum::SortBy(sb)) = o.node.as_ref() else {
                return None;
            };
            let n = match sb.node.as_deref().and_then(|n| n.node.as_ref())? {
                NodeEnum::AConst(c) => match &c.val {
                    Some(pb::a_const::Val::Ival(v)) => v.ival as usize,
                    _ => return None,
                },
                NodeEnum::ColumnRef(cr) => {
                    let [f] = cr.fields.as_slice() else {
                        return None;
                    };
                    let name = ast::sval(f)?;
                    out.iter().position(|c| c.name == name)? + 1
                }
                _ => return None,
            };
            // A set operation's ORDER BY prints column numbers; VALUES', its columns.
            let text = if setop {
                n.to_string()
            } else if how.nested() {
                format!("\"*VALUES*\".column{n}")
            } else {
                format!("column{n}")
            };
            order.push((
                text,
                sort_key(Expr::Const(Value::Null, DataType::Unknown), sb)?,
            ));
        }
        let offset = self.bind_opt(s.limit_offset.as_deref(), Clause::Offset)?;
        let limit = self.bind_opt(s.limit_count.as_deref(), Clause::Limit)?;
        self.print(&level, indent, buf, &[], |d| {
            if !order.is_empty() {
                d.keyword(" ORDER BY ", -STD, STD, 1);
                for (i, (text, k)) in order.iter().enumerate() {
                    if i > 0 {
                        d.buf.push_str(", ");
                    }
                    d.buf.push_str(text);
                    d.sort_suffix(k);
                }
            }
            d.offset_limit(offset.as_ref(), limit.as_ref(), false)
        })
    }

    /// VALUES rows (`get_values_def`), bound in the current level.
    fn values_text(&mut self, lists: &[pb::Node]) -> Option<String> {
        let (plan, _) = self.b.bind_values(lists).ok()?;
        let Plan::Values { rows, .. } = plan else {
            return None;
        };
        let level = ViewLevel {
            outer: self.outer_names(),
            ..ViewLevel::default()
        };
        let mut buf = String::new();
        self.print(&level, 0, &mut buf, &[], |d| {
            d.buf.push_str("VALUES ");
            for (i, row) in rows.iter().enumerate() {
                if i > 0 {
                    d.buf.push_str(", ");
                }
                d.buf.push('(');
                for (j, e) in row.iter().enumerate() {
                    if j > 0 {
                        d.buf.push(',');
                    }
                    if d.whole_row(e).is_some() || matches!(e, Expr::Outer(..)) {
                        d.expr_toplevel(e)?;
                    } else {
                        d.expr(e, false)?;
                    }
                }
                d.buf.push(')');
            }
            Some(())
        })?;
        Some(buf)
    }

    /// A VALUES query whose output names differ from the VALUES columns': a SELECT over the
    /// VALUES range-table entry.
    fn values_select(
        &mut self,
        s: &pb::SelectStmt,
        buf: &mut String,
        indent: i32,
        how: &Nest,
        names: &[String],
    ) -> Option<()> {
        if !s.sort_clause.is_empty() || s.limit_count.is_some() || s.limit_offset.is_some() {
            return None;
        }
        let mut indent = indent;
        if let Some(w) = &s.with_clause {
            self.with_clause(w, buf, &mut indent)?;
        }
        let rows = self.values_text(&s.values_lists)?;
        let result = how.result()?;
        let prefix = how.nested();
        let cols: Vec<String> = names
            .iter()
            .map(|n| {
                if prefix {
                    format!("\"*VALUES*\".{n}")
                } else {
                    n.clone()
                }
            })
            .collect();
        let sel = Select {
            targets: cols
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    (
                        Expr::Col(i, DataType::Unknown),
                        result.get(i).unwrap_or(&names[i]).clone(),
                        Some(names[i].clone()),
                    )
                })
                .collect(),
            from_text: Some(format!("({rows}) \"*VALUES*\"")),
            ..Select::default()
        };
        let level = ViewLevel {
            cols,
            outer: self.outer_names(),
            ..ViewLevel::default()
        };
        let wrap = self.wrap;
        self.print(&level, indent, buf, &[], |d| {
            d.basic_select(&[], &sel, wrap)
        })
    }

    /// `get_with_clause`.
    fn with_clause(
        &mut self,
        w: &pb::WithClause,
        buf: &mut String,
        indent: &mut i32,
    ) -> Option<()> {
        *indent += STD;
        buf.push(' ');
        let mut sep = if w.recursive {
            "WITH RECURSIVE "
        } else {
            "WITH "
        };
        for c in &w.ctes {
            let Some(NodeEnum::CommonTableExpr(cte)) = c.node.as_ref() else {
                return None;
            };
            buf.push_str(sep);
            buf.push_str(&quote_ident(&cte.ctename));
            let aliases = ast::names(&cte.aliascolnames);
            if !aliases.is_empty() {
                buf.push('(');
                buf.push_str(
                    &aliases
                        .iter()
                        .map(|a| quote_ident(a))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                buf.push(')');
            }
            buf.push_str(" AS ");
            match pb::CteMaterialize::try_from(cte.ctematerialized).ok()? {
                pb::CteMaterialize::Always => buf.push_str("MATERIALIZED "),
                pb::CteMaterialize::Never => buf.push_str("NOT MATERIALIZED "),
                _ => {}
            }
            buf.push('(');
            keyword(buf, indent, "", 0, 0, 0);
            let q = cte.ctequery.as_deref()?;
            match q.node.as_ref()? {
                NodeEnum::InsertStmt(_) | NodeEnum::UpdateStmt(_) | NodeEnum::DeleteStmt(_) => {
                    self.dml(q, buf, *indent)?
                }
                _ => self.query(q, buf, *indent, &[], Nest::Cte)?,
            }
            keyword(buf, indent, "", 0, 0, 0);
            buf.push(')');
            if let Some(sc) = &cte.search_clause {
                let cols: Option<Vec<String>> = sc
                    .search_col_list
                    .iter()
                    .map(|c| ast::sval(c).map(quote_ident))
                    .collect();
                buf.push_str(&format!(
                    " SEARCH {} FIRST BY {} SET {}",
                    if sc.search_breadth_first {
                        "BREADTH"
                    } else {
                        "DEPTH"
                    },
                    cols?.join(", "),
                    quote_ident(&sc.search_seq_column)
                ));
            }
            if let Some(cc) = &cte.cycle_clause {
                let cols: Option<Vec<String>> = cc
                    .cycle_col_list
                    .iter()
                    .map(|c| ast::sval(c).map(quote_ident))
                    .collect();
                buf.push_str(&format!(
                    " CYCLE {} SET {}",
                    cols?.join(", "),
                    quote_ident(&cc.cycle_mark_column)
                ));
                let mut bind = |n: Option<&pb::Node>, dflt: bool| -> Option<Expr> {
                    match n {
                        None => Some(Expr::Const(Value::Bool(dflt), DataType::Bool)),
                        Some(n) => {
                            let e = self.b.bind_expr(n).ok()?;
                            if matches!(e.ty(), DataType::Unknown) {
                                self.b.coerce_implicit(e, &DataType::Text).ok()
                            } else {
                                Some(e)
                            }
                        }
                    }
                };
                let value = bind(cc.cycle_mark_value.as_deref(), true)?;
                let default = bind(cc.cycle_mark_default.as_deref(), false)?;
                let plain = matches!(
                    (&value, &default),
                    (
                        Expr::Const(Value::Bool(true), DataType::Bool),
                        Expr::Const(Value::Bool(false), DataType::Bool)
                    )
                );
                if !plain {
                    let level = ViewLevel::default();
                    let mut text = String::new();
                    self.print(&level, *indent, &mut text, &[], |d| {
                        d.buf.push_str(" TO ");
                        d.expr(&value, false)?;
                        d.buf.push_str(" DEFAULT ");
                        d.expr(&default, false)
                    })?;
                    buf.push_str(&text);
                }
                buf.push_str(&format!(" USING {}", quote_ident(&cc.cycle_path_column)));
            }
            sep = ", ";
        }
        *indent -= STD;
        keyword(buf, indent, "", 0, 0, 0);
        Some(())
    }

    /// `get_setop_query`. `top`: the set operation of the query level itself.
    #[allow(clippy::too_many_arguments)]
    fn setop(
        &mut self,
        s: &pb::SelectStmt,
        buf: &mut String,
        indent: &mut i32,
        result: &Option<Vec<String>>,
        visible: bool,
        types: &[DataType],
        top: bool,
    ) -> Option<()> {
        if !top && is_leaf(s) {
            let paren = s.with_clause.is_some()
                || !s.sort_clause.is_empty()
                || !s.locking_clause.is_empty()
                || s.limit_count.is_some()
                || s.limit_offset.is_some();
            if paren {
                buf.push('(');
            }
            let node = pb::Node {
                node: Some(NodeEnum::SelectStmt(Box::new(s.clone()))),
            };
            self.query(
                &node,
                buf,
                *indent,
                &[],
                Nest::Branch(result.clone(), visible, types.to_vec()),
            )?;
            if paren {
                buf.push(')');
            }
            return Some(());
        }
        let op = pb::SetOperation::try_from(s.op).ok()?;
        let (larg, rarg) = (s.larg.as_deref()?, s.rarg.as_deref()?);
        let paren = !is_leaf(larg) && !(larg.op == s.op && larg.all == s.all);
        let sub = if paren {
            buf.push('(');
            keyword(buf, indent, "", STD, 0, 0);
            STD
        } else {
            0
        };
        self.setop(larg, buf, indent, result, visible, types, false)?;
        keyword(buf, indent, if paren { ") " } else { "" }, -sub, 0, 0);
        buf.push_str(match op {
            pb::SetOperation::SetopUnion => "UNION ",
            pb::SetOperation::SetopIntersect => "INTERSECT ",
            _ => "EXCEPT ",
        });
        if s.all {
            buf.push_str("ALL ");
        }
        let paren = !is_leaf(rarg);
        let sub = if paren {
            buf.push('(');
            STD
        } else {
            0
        };
        keyword(buf, indent, "", sub, 0, 0);
        self.setop(rarg, buf, indent, result, false, types, false)?;
        *indent -= sub;
        if paren {
            keyword(buf, indent, ")", 0, 0, 0);
        }
        Some(())
    }

    /// `get_basic_select_query` and the clauses after it.
    fn basic(
        &mut self,
        s: &pb::SelectStmt,
        buf: &mut String,
        indent: i32,
        how: &Nest,
    ) -> Option<()> {
        for w in &s.window_clause {
            if let Some(NodeEnum::WindowDef(wd)) = w.node.as_ref() {
                self.b
                    .level()
                    .st
                    .named_windows
                    .push((wd.name.clone(), (**wd).clone()));
            }
        }
        self.b.bind_from(&s.from_clause, None).ok()?;
        let scope = self.b.scope().clone();
        // Range-table names, unique among the levels being printed.
        let mut slots = Vec::new();
        for item in &s.from_clause {
            from_slots(item, &mut slots, &|n| self.is_cte(n), &|j| {
                self.natural_using(j)
            })?;
        }
        let (refs, joins) = self.rel_names(&scope, &slots)?;
        let prefix = how.nested() || s.from_clause.iter().map(rte_count).sum::<usize>() != 1;
        let (cols, qual) = column_names(&scope, &refs, prefix);
        let mut sel = self.bind_select(s, &scope, how)?;
        // FOR UPDATE: the locked relations' range-table names, strongest lock kept.
        sel.locks = Vec::new();
        for l in &s.locking_clause {
            let Some(NodeEnum::LockingClause(lc)) = l.node.as_ref() else {
                return None;
            };
            let strength = match pb::LockClauseStrength::try_from(lc.strength).ok()? {
                pb::LockClauseStrength::LcsForkeyshare => " FOR KEY SHARE",
                pb::LockClauseStrength::LcsForshare => " FOR SHARE",
                pb::LockClauseStrength::LcsFornokeyupdate => " FOR NO KEY UPDATE",
                _ => " FOR UPDATE",
            };
            let wait = match pb::LockWaitPolicy::try_from(lc.wait_policy).ok()? {
                pb::LockWaitPolicy::LockWaitError => " NOWAIT",
                pb::LockWaitPolicy::LockWaitSkip => " SKIP LOCKED",
                _ => "",
            };
            let rels: Vec<usize> = if lc.locked_rels.is_empty() {
                slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| **s == Slot::Rel { lockable: true })
                    .map(|(i, _)| i)
                    .collect()
            } else {
                let mut v = Vec::new();
                for r in &lc.locked_rels {
                    let Some(NodeEnum::RangeVar(rv)) = r.node.as_ref() else {
                        return None;
                    };
                    v.push(scope.rels.iter().zip(&refs).position(|(r, n)| {
                        !n.is_empty() && !r.anonymous && r.alias == rv.relname
                    })?);
                }
                v
            };
            let rank = |k: &str| {
                [
                    " FOR KEY SHARE",
                    " FOR SHARE",
                    " FOR NO KEY UPDATE",
                    " FOR UPDATE",
                ]
                .iter()
                .position(|x| *x == k)
            };
            for i in rels {
                let name = refs[i].clone();
                match sel.locks.iter_mut().find(|(_, n, _)| *n == name) {
                    Some(l) => {
                        if rank(strength) > rank(l.0) {
                            l.0 = strength;
                        }
                        if wait == " NOWAIT" || (wait == " SKIP LOCKED" && l.2.is_empty()) {
                            l.2 = wait;
                        }
                    }
                    None => sel.locks.push((strength, name, wait)),
                }
            }
        }
        let windows = self.window_calls()?;
        let st = &self.b.levels.last()?.st;
        let (aggs, groups, srfs) = (st.aggs.clone(), st.group_exprs.clone(), st.srfs.clone());
        let targets = sel
            .targets
            .iter()
            .map(|t| (t.1.clone(), t.0.clone()))
            .collect();
        let merged = merged_columns(&scope, &slots, &refs);
        let rows = whole_rows(&scope, &refs);
        let level = ViewLevel {
            cols,
            qual,
            outer: self.outer_names(),
            refs,
            windows,
            targets,
            merged,
            groups,
            srfs,
            joins,
            scope: None,
            rows,
        };
        let mut indent = indent;
        if let Some(w) = &s.with_clause {
            self.with_clause(w, buf, &mut indent)?;
        }
        let wrap = self.wrap;
        self.print(&level, indent, buf, &aggs, |d| {
            d.basic_select(&s.from_clause, &sel, wrap)
        })
    }

    /// Range-table names for `scope`'s relations ("" for merged columns), unique among the
    /// levels being printed, and the insides of its aliased joins (named before the join).
    fn rel_names(
        &mut self,
        scope: &crate::sql::binder::Scope,
        slots: &[Slot],
    ) -> Option<(Vec<String>, Vec<(usize, ViewLevel)>)> {
        if slots.len() != scope.rels.len() {
            return None;
        }
        let mut refs = Vec::with_capacity(slots.len());
        let mut joins = Vec::new();
        for (r, slot) in scope.rels.iter().zip(slots) {
            match slot {
                Slot::Merged => {
                    refs.push(String::new());
                    continue;
                }
                Slot::Join(j) => {
                    let inner = self.join_level(j, r)?;
                    joins.push((*j as *const pb::JoinExpr as usize, inner));
                }
                Slot::Rel { .. } => {}
            }
            let base = if r.anonymous {
                "unnamed_subquery".to_string()
            } else {
                r.shown.clone().unwrap_or_else(|| r.alias.clone())
            };
            let mut name = base.clone();
            let mut i = 0;
            while self.used.contains(&name) {
                i += 1;
                name = crate::catalog::suffixed_name(&base, &format!("_{i}"));
            }
            self.used.push(name.clone());
            refs.push(name);
        }
        Some((refs, joins))
    }

    /// The inside of aliased join `j` (scope relation `alias` outside it).
    fn join_level(
        &mut self,
        j: &pb::JoinExpr,
        alias: &crate::sql::binder::ScopeRel,
    ) -> Option<ViewLevel> {
        let inner = self
            .b
            .join_inner
            .get(&(j as *const pb::JoinExpr as usize))?
            .clone();
        // The alias's columns are the inside's visible ones, moved to where the join sits.
        let visible = inner
            .rels
            .iter()
            .flat_map(|r| r.cols.iter().filter(|c| !c.hidden && !c.merged));
        let delta = alias
            .cols
            .iter()
            .zip(visible)
            .find_map(|(a, b)| match (&a.expr, &b.expr) {
                (Expr::Col(x, _), Expr::Col(y, _)) => x.checked_sub(*y),
                _ => None,
            })?;
        let scope = inner.shifted(delta);
        let mut slots = Vec::new();
        join_slots(j, &mut slots, &|n| self.is_cte(n), &|j| {
            self.natural_using(j)
        })?;
        let (refs, joins) = self.rel_names(&scope, &slots)?;
        let (_, qual) = column_names(&scope, &refs, true);
        let merged = merged_columns(&scope, &slots, &refs);
        Some(ViewLevel {
            cols: qual.clone(),
            qual,
            outer: self.outer_names(),
            refs,
            merged,
            joins,
            scope: Some(scope),
            ..ViewLevel::default()
        })
    }

    /// Bind a basic SELECT's parts (its FROM is bound).
    fn bind_select(
        &mut self,
        s: &pb::SelectStmt,
        scope: &crate::sql::binder::Scope,
        how: &Nest,
    ) -> Option<Select> {
        let b = &mut *self.b;
        // GROUP BY expressions first: GROUPING() in the select list refers to them.
        let mut group_exprs = Vec::new();
        for g in &s.group_clause {
            pre_group(b, g, &mut group_exprs);
        }
        b.level().st.group_exprs = group_exprs;
        let mut targets: Vec<Target> = Vec::new();
        for t in &s.target_list {
            let Some(NodeEnum::ResTarget(rt)) = &t.node else {
                return None;
            };
            let val = rt.val.as_deref()?;
            if let Some(NodeEnum::ColumnRef(cr)) = &val.node
                && matches!(
                    cr.fields.last().and_then(|f| f.node.as_ref()),
                    Some(NodeEnum::AStar(_))
                )
            {
                let rel = match cr.fields.len() {
                    1 => None,
                    2 => Some(ast::sval(&cr.fields[0])?.to_string()),
                    _ => return None,
                };
                let mut found = false;
                for r in scope.rels.iter().filter(|r| match &rel {
                    Some(n) => *n == r.alias && !r.anonymous,
                    None => !r.qualified_only,
                }) {
                    found = true;
                    for c in r
                        .cols
                        .iter()
                        .filter(|c| !c.hidden && (rel.is_some() || !c.merged))
                    {
                        targets.push(Target {
                            expr: c.expr.clone(),
                            name: c.name.clone(),
                            own: Some(c.name.clone()),
                        });
                    }
                }
                // `rel.*` of an enclosing level (a rule action's OLD / NEW): its columns, as
                // outer references.
                if let (false, Some(n)) = (found, &rel) {
                    let names: Vec<String> = (b.levels.iter().rev())
                        .flat_map(|l| l.scope.rels.iter())
                        .find(|r| r.alias == *n && !r.anonymous)
                        .map(|r| {
                            (r.cols.iter())
                                .filter(|c| !c.hidden)
                                .map(|c| c.name.clone())
                                .collect()
                        })?;
                    for c in names {
                        let node = ast::mk(NodeEnum::ColumnRef(pb::ColumnRef {
                            fields: vec![
                                ast::mk(NodeEnum::String(pb::String { sval: n.clone() })),
                                ast::mk(NodeEnum::String(pb::String { sval: c.clone() })),
                            ],
                            location: -1,
                        }));
                        let e = b.in_clause(Clause::Select, |b| b.bind_expr(&node)).ok()?;
                        targets.push(Target {
                            expr: e,
                            name: c.clone(),
                            own: Some(c),
                        });
                    }
                }
                continue;
            }
            let e = b.in_clause(Clause::Select, |b| b.bind_expr(val)).ok()?;
            let own = match (&e, &val.node) {
                (
                    Expr::Col(..) | Expr::Outer(..) | Expr::Coalesce(..),
                    Some(NodeEnum::ColumnRef(cr)),
                ) if !fn_arg(b, &e) => cr.fields.last().and_then(ast::sval).map(str::to_string),
                // A column however written (`(t).a`, a cast to its own type): it shows as
                // itself.
                (Expr::Col(i, _), _) if !fn_arg(b, &e) => (b.scope().rels.iter())
                    .flat_map(|r| r.cols.iter())
                    .find(|c| !c.hidden && matches!(c.expr, Expr::Col(j, _) if j == *i))
                    .map(|c| c.name.clone()),
                _ => None,
            };
            let name = if rt.name.is_empty() {
                figure_colname(val)
            } else {
                rt.name.clone()
            };
            // Unknown-typed literals take the set operation's type, else text (in an
            // INSERT's SELECT, the target column's).
            let e = if matches!(e.ty(), DataType::Unknown) && !matches!(how, Nest::Insert) {
                let to = how
                    .types()
                    .get(targets.len())
                    .filter(|t| !matches!(t, DataType::Unknown))
                    .cloned()
                    .unwrap_or(DataType::Text);
                b.coerce_implicit(e, &to).ok()?
            } else {
                e
            };
            targets.push(Target { expr: e, name, own });
        }
        let bind_cond = |b: &mut Binder, n: &pb::Node, clause: Clause| -> Option<Expr> {
            b.in_clause(clause, |b| {
                let e = b.bind_expr(n)?;
                b.coerce_bool(e, clause.name())
            })
            .ok()
        };
        let where_ = match s.where_clause.as_deref() {
            Some(w) => Some(bind_cond(b, w, Clause::Where)?),
            None => None,
        };
        let mut group = Vec::new();
        let sets = s
            .group_clause
            .iter()
            .any(|g| matches!(g.node, Some(NodeEnum::GroupingSet(_))));
        for g in &s.group_clause {
            let item = group_item(b, g, &targets)?;
            // A repeated plain GROUP BY item is grouped once.
            if !sets
                && let Group::Expr(e) = &item
                && group
                    .iter()
                    .any(|x| matches!(x, Group::Expr(y) if y.same_as(e)))
            {
                continue;
            }
            group.push(item);
        }
        let having = match s.having_clause.as_deref() {
            Some(h) => Some(bind_cond(b, h, Clause::Having)?),
            None => None,
        };
        let distinct = if s.distinct_clause.is_empty() {
            None
        } else if s.distinct_clause.iter().all(|n| n.node.is_none()) {
            Some(Vec::new())
        } else {
            let mut on = Vec::new();
            for n in &s.distinct_clause {
                on.push(output_item(b, n, &targets, Clause::DistinctOn)?);
            }
            Some(on)
        };
        let mut order = Vec::new();
        for o in &s.sort_clause {
            let Some(NodeEnum::SortBy(sb)) = &o.node else {
                return None;
            };
            let e = output_item(b, sb.node.as_deref()?, &targets, Clause::OrderBy)?;
            order.push(sort_key(e, sb)?);
        }
        let mut windows = Vec::new();
        for w in &s.window_clause {
            let Some(NodeEnum::WindowDef(wd)) = w.node.as_ref() else {
                return None;
            };
            windows.push((wd.name.clone(), bind_spec(b, wd)?));
        }
        let offset = self.bind_opt(s.limit_offset.as_deref(), Clause::Offset)?;
        let limit = self.bind_opt(s.limit_count.as_deref(), Clause::Limit)?;
        // Output names: the view's (or the set operation's), else the targets'. A column
        // reference shows its own name; anything else shows none when column names are
        // visible, else FigureColname's fallback.
        let result = how.result();
        let visible = how.names_visible();
        let targets = targets
            .into_iter()
            .enumerate()
            .map(|(i, t)| {
                let name = result.and_then(|r| r.get(i)).cloned().unwrap_or(t.name);
                let shown = t.own.or_else(|| (!visible).then(|| "?column?".to_string()));
                (t.expr, name, shown)
            })
            .collect();
        Some(Select {
            distinct,
            targets,
            from_text: None,
            where_,
            group,
            group_distinct: s.group_distinct,
            having,
            windows,
            order,
            offset,
            limit,
            with_ties: s.limit_option == pb::LimitOption::WithTies as i32,
            locks: Vec::new(),
        })
    }

    /// The level's window function calls with their OVER clauses bound.
    fn window_calls(&mut self) -> Option<Vec<(WindowCall, String, Over)>> {
        let st = &self.b.levels.last()?.st;
        let calls = st.windows.clone();
        let defs = st.window_defs.clone();
        let mut out = Vec::new();
        for (i, call) in calls.into_iter().enumerate() {
            let (name, over) = defs.get(i)?;
            let over = if !over.name.is_empty()
                && over.refname.is_empty()
                && over.partition_clause.is_empty()
                && over.order_clause.is_empty()
                && over.frame_options & FRAME_NONDEFAULT == 0
            {
                Over::Name(over.name.clone())
            } else {
                Over::Spec(Box::new(bind_spec(self.b, over)?))
            };
            out.push((call, name.clone(), over));
        }
        Some(out)
    }
}

/// Whether `e`, bound at the binder's current level, is a SQL function's argument (a parameter
/// to Postgres, which names no column).
fn fn_arg(b: &Binder, e: &Expr) -> bool {
    let mut lvl = b.levels.len() - 1;
    let mut e = e;
    while let Expr::Outer(1, j, _) = e {
        if b.levels[lvl].fn_args.is_some() {
            return true;
        }
        let Some(p) = b.levels[lvl].params.get(*j) else {
            return false;
        };
        let Some(up) = lvl.checked_sub(1) else {
            return false;
        };
        (e, lvl) = (p, up);
    }
    false
}

/// The length of `buf`'s last line.
fn line_len(buf: &str) -> usize {
    buf.rsplit('\n').next().map_or(0, str::len)
}

/// A set-operation input that prints as a query of its own.
fn is_leaf(s: &pb::SelectStmt) -> bool {
    matches!(
        pb::SetOperation::try_from(s.op),
        Ok(pb::SetOperation::SetopNone | pb::SetOperation::Undefined) | Err(_)
    ) || s.with_clause.is_some()
        || !s.sort_clause.is_empty()
        || !s.locking_clause.is_empty()
        || s.limit_count.is_some()
        || s.limit_offset.is_some()
}

/// Range-table entries a FROM item makes (a join is one too).
fn rte_count(n: &pb::Node) -> usize {
    match n.node.as_ref() {
        Some(NodeEnum::JoinExpr(j)) => {
            1 + j.larg.as_deref().map_or(0, rte_count) + j.rarg.as_deref().map_or(0, rte_count)
        }
        _ => 1,
    }
}

/// Each scope relation's columns as the level shows them, and always qualified.
fn column_names(
    scope: &crate::sql::binder::Scope,
    refs: &[String],
    prefix: bool,
) -> (Vec<String>, Vec<String>) {
    let mut cols: Vec<String> = Vec::new();
    let mut qual: Vec<String> = Vec::new();
    for (r, refname) in scope.rels.iter().zip(refs) {
        if refname.is_empty() {
            continue;
        }
        for c in r.cols.iter().filter(|c| !c.hidden) {
            if let Expr::Col(i, _) = c.expr {
                if cols.len() <= i {
                    cols.resize(i + 1, String::new());
                    qual.resize(i + 1, String::new());
                }
                qual[i] = format!("{}.{}", quote_ident(refname), quote_ident(&c.name));
                cols[i] = if prefix {
                    qual[i].clone()
                } else {
                    quote_ident(&c.name)
                };
            }
        }
    }
    (cols, qual)
}

/// Columns that are no plain column (FULL JOIN USING's merged ones), with how they show:
/// by name, or qualified through a join alias.
fn merged_columns(
    scope: &crate::sql::binder::Scope,
    slots: &[Slot],
    refs: &[String],
) -> Vec<(Expr, String)> {
    let mut merged = Vec::new();
    for ((r, slot), refname) in scope.rels.iter().zip(slots).zip(refs) {
        let computed = r.cols.iter().filter(|c| !matches!(c.expr, Expr::Col(..)));
        match slot {
            Slot::Merged if r.anonymous => {
                merged.extend(computed.map(|c| (c.expr.clone(), quote_ident(&c.name))))
            }
            Slot::Join(_) => merged.extend(computed.filter(|c| !c.hidden).map(|c| {
                (
                    c.expr.clone(),
                    format!("{}.{}", quote_ident(refname), quote_ident(&c.name)),
                )
            })),
            _ => {}
        }
    }
    merged
}

/// The scope relations a join binds inside its alias (if any), in the binder's order.
fn join_slots<'n>(
    j: &'n pb::JoinExpr,
    out: &mut Vec<Slot<'n>>,
    is_cte: &dyn Fn(&str) -> bool,
    natural: &dyn Fn(&pb::JoinExpr) -> Option<Vec<String>>,
) -> Option<()> {
    let using = if j.is_natural {
        natural(j)?
    } else {
        ast::names(&j.using_clause)
    };
    if !using.is_empty() {
        // The USING alias's columns, then the merged columns.
        if j.join_using_alias.is_some() {
            if j.jointype == pb::JoinType::JoinFull as i32 {
                return None;
            }
            out.push(Slot::Merged);
        }
        out.push(Slot::Merged);
    }
    from_slots(j.larg.as_deref()?, out, is_cte, natural)?;
    from_slots(j.rarg.as_deref()?, out, is_cte, natural)
}

/// The scope relations a FROM item binds, in the binder's order.
fn from_slots<'n>(
    n: &'n pb::Node,
    out: &mut Vec<Slot<'n>>,
    is_cte: &dyn Fn(&str) -> bool,
    natural: &dyn Fn(&pb::JoinExpr) -> Option<Vec<String>>,
) -> Option<()> {
    match n.node.as_ref()? {
        // A join alias hides the names inside it.
        NodeEnum::JoinExpr(j) if j.alias.is_some() => {
            out.push(Slot::Join(j));
            Some(())
        }
        NodeEnum::JoinExpr(j) => join_slots(j, out, is_cte, natural),
        NodeEnum::RangeVar(rv) => {
            out.push(Slot::Rel {
                lockable: !(rv.schemaname.is_empty() && is_cte(&rv.relname)),
            });
            Some(())
        }
        NodeEnum::RangeSubselect(_) => {
            out.push(Slot::Rel { lockable: true });
            Some(())
        }
        NodeEnum::RangeTableSample(_) => {
            out.push(Slot::Rel { lockable: true });
            Some(())
        }
        _ => {
            out.push(Slot::Rel { lockable: false });
            Some(())
        }
    }
}

fn sort_key(e: Expr, sb: &pb::SortBy) -> Option<SortKey> {
    if sb.sortby_dir == pb::SortByDir::SortbyUsing as i32 {
        return None;
    }
    let desc = sb.sortby_dir == pb::SortByDir::SortbyDesc as i32;
    let nulls_first = match pb::SortByNulls::try_from(sb.sortby_nulls).ok()? {
        pb::SortByNulls::SortbyNullsFirst => true,
        pb::SortByNulls::SortbyNullsLast => false,
        _ => desc,
    };
    Some(SortKey {
        expr: e,
        desc,
        nulls_first,
        using: None,
    })
}

/// An ORDER BY / DISTINCT ON item: an output column number or name (a bare name means the
/// output column first), or an expression.
fn output_item(b: &mut Binder, n: &pb::Node, targets: &[Target], clause: Clause) -> Option<Expr> {
    if let Some(NodeEnum::AConst(c)) = &n.node
        && let Some(pb::a_const::Val::Ival(i)) = &c.val
    {
        return targets
            .get((i.ival as usize).checked_sub(1)?)
            .map(|t| t.expr.clone());
    }
    if let Some(NodeEnum::ColumnRef(cr)) = &n.node
        && let [f] = cr.fields.as_slice()
        && let Some(name) = ast::sval(f)
    {
        let mut found = targets.iter().filter(|t| t.name == name);
        if let (Some(t), None) = (found.next(), found.next()) {
            return Some(t.expr.clone());
        }
    }
    b.in_clause(clause, |b| b.bind_expr(n)).ok()
}

/// A GROUP BY item: an input column or expression first, then an output column.
fn group_expr(b: &mut Binder, n: &pb::Node, targets: &[Target]) -> Option<Expr> {
    if let Some(NodeEnum::AConst(c)) = &n.node
        && let Some(pb::a_const::Val::Ival(i)) = &c.val
    {
        return targets
            .get((i.ival as usize).checked_sub(1)?)
            .map(|t| t.expr.clone());
    }
    if let Ok(e) = b.in_clause(Clause::GroupBy, |b| b.bind_expr(n)) {
        return Some(e);
    }
    let Some(NodeEnum::ColumnRef(cr)) = &n.node else {
        return None;
    };
    let [f] = cr.fields.as_slice() else {
        return None;
    };
    let name = ast::sval(f)?;
    targets
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.expr.clone())
}

/// The expressions in a GROUP BY item that bind as input expressions.
fn pre_group(b: &mut Binder, n: &pb::Node, out: &mut Vec<Expr>) {
    match n.node.as_ref() {
        Some(NodeEnum::GroupingSet(gs)) => gs.content.iter().for_each(|c| pre_group(b, c, out)),
        Some(NodeEnum::RowExpr(r)) => r.args.iter().for_each(|c| pre_group(b, c, out)),
        Some(NodeEnum::AConst(_)) | None => {}
        Some(_) => {
            if let Ok(e) = b.in_clause(Clause::GroupBy, |b| b.bind_expr(n))
                && !out.iter().any(|x| x.same_as(&e))
            {
                out.push(e);
            }
        }
    }
}

/// A GROUP BY item, bound.
fn group_item(b: &mut Binder, n: &pb::Node, targets: &[Target]) -> Option<Group> {
    use pb::GroupingSetKind as K;
    let Some(NodeEnum::GroupingSet(gs)) = n.node.as_ref() else {
        return group_expr(b, n, targets).map(Group::Expr);
    };
    let mut children = Vec::new();
    for c in &gs.content {
        children.push(match c.node.as_ref() {
            Some(NodeEnum::RowExpr(r)) => Group::Simple(
                r.args
                    .iter()
                    .map(|a| group_expr(b, a, targets))
                    .collect::<Option<Vec<_>>>()?,
            ),
            _ => group_item(b, c, targets)?,
        });
    }
    Some(match K::try_from(gs.kind).ok()? {
        K::GroupingSetEmpty => Group::Empty,
        K::GroupingSetSimple => Group::Simple(
            gs.content
                .iter()
                .map(|c| group_expr(b, c, targets))
                .collect::<Option<Vec<_>>>()?,
        ),
        K::GroupingSetRollup => Group::Rollup(children),
        K::GroupingSetCube => Group::Cube(children),
        K::GroupingSetSets => Group::Sets(children),
        _ => return None,
    })
}

/// A window specification's parts as written, bound in the current level (frame offsets as
/// the frame binds them).
fn bind_spec(b: &mut Binder, w: &pb::WindowDef) -> Option<Spec> {
    let (_, _, frame) = b.bind_window_def(w).ok()?;
    let partition = b
        .in_clause(Clause::Window, |b| {
            w.partition_clause
                .iter()
                .map(|p| b.bind_expr(p))
                .collect::<crate::error::Result<Vec<_>>>()
        })
        .ok()?;
    let mut order = Vec::new();
    for o in &w.order_clause {
        let Some(NodeEnum::SortBy(sb)) = o.node.as_ref() else {
            return None;
        };
        let e = b
            .in_clause(Clause::Window, |b| b.bind_expr(sb.node.as_deref().unwrap()))
            .ok()?;
        order.push(sort_key(e, sb)?);
    }
    let offset = |f: &FrameBound| match f {
        FrameBound::Preceding(e) | FrameBound::Following(e) => Some(e.clone()),
        _ => None,
    };
    Some(Spec {
        refname: w.refname.clone(),
        partition,
        order,
        frame: w.frame_options,
        start: offset(&frame.start),
        end: offset(&frame.end),
    })
}

impl Deparser<'_, '_> {
    /// A view's sublink (`get_sublink_expr`).
    pub(super) fn sublink(&mut self, sq: &Subquery) -> Option<()> {
        let level = self.view?;
        self.buf.push_str(if sq.kind == SubqueryKind::Array {
            "ARRAY("
        } else {
            "("
        });
        let mut op = None;
        match sq.left.as_slice() {
            [] => {}
            [l] => {
                self.expr(l, true)?;
                op = Some(sq.ops.first()?.name);
            }
            ls => {
                self.buf.push('(');
                self.list(ls)?;
                self.buf.push(')');
                op = Some(sq.ops.first()?.name);
            }
        }
        let paren = match sq.kind {
            SubqueryKind::Exists => {
                self.buf.push_str("EXISTS ");
                true
            }
            SubqueryKind::Any if op == Some("=") => {
                self.buf.push_str(" IN ");
                true
            }
            SubqueryKind::Any => {
                self.buf.push_str(&format!(" {} ANY ", op?));
                true
            }
            SubqueryKind::RowCompare => {
                self.buf.push_str(&format!(" {} ", op?));
                true
            }
            SubqueryKind::All => {
                self.buf.push_str(&format!(" {} ALL ", op?));
                true
            }
            SubqueryKind::Scalar | SubqueryKind::Array => false,
        };
        if paren {
            self.buf.push('(');
        }
        let nested = self.nested.as_deref_mut()?;
        let node = nested.sublink_node(sq.id)?;
        nested.query(
            &node,
            &mut self.buf,
            self.indent,
            &level.qual,
            Nest::Sublink,
        )?;
        self.buf.push_str(if paren { "))" } else { ")" });
        Some(())
    }

    /// A FULL JOIN USING column, shown by name.
    pub(super) fn merged_column(&mut self, e: &Expr) -> bool {
        let Some(level) = self.view else { return false };
        match level.merged.iter().find(|(x, _)| x.same_as(e)) {
            Some((_, n)) => {
                self.buf.push_str(n);
                true
            }
            None => false,
        }
    }

    /// `GROUPING(args)`: the GROUP BY expressions it tests.
    pub(super) fn grouping(&mut self, pos: &[usize]) -> Option<()> {
        let level = self.view?;
        let args: Vec<Expr> = pos
            .iter()
            .map(|p| level.groups.get(*p).cloned())
            .collect::<Option<_>>()?;
        self.buf.push_str("GROUPING(");
        self.list(&args)?;
        self.buf.push(')');
        Some(())
    }

    /// A set-returning call in the select list.
    pub(super) fn srf(&mut self, i: usize) -> Option<()> {
        let c = self.view?.srfs.get(i)?;
        if c.body.is_some() {
            return None;
        }
        self.expr(
            &Expr::Call(c.func.clone(), c.args.clone(), c.ty.clone()),
            false,
        )
    }

    /// A window function call (`get_windowfunc_expr`).
    pub(super) fn window_func(&mut self, i: usize) -> Option<()> {
        let level = self.view?;
        let (call, name, over) = level.windows.get(i)?;
        match &call.func {
            WinFunc::Agg(a) => self.aggregate(a)?,
            WinFunc::Builtin(_) => {
                self.buf.push_str(&quote_ident(name));
                self.buf.push('(');
                self.list(&call.args)?;
                self.buf.push(')');
            }
        }
        self.buf.push_str(" OVER ");
        match over {
            Over::Name(n) => self.buf.push_str(&quote_ident(n)),
            Over::Spec(s) => self.window_spec(s)?,
        }
        Some(())
    }

    /// `get_rule_windowspec`.
    fn window_spec(&mut self, s: &Spec) -> Option<()> {
        self.buf.push('(');
        let mut space = false;
        if !s.refname.is_empty() {
            self.buf.push_str(&quote_ident(&s.refname));
            space = true;
        }
        if !s.partition.is_empty() && s.refname.is_empty() {
            if space {
                self.buf.push(' ');
            }
            self.buf.push_str("PARTITION BY ");
            for (i, p) in s.partition.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.order_item(p)?;
            }
            space = true;
        }
        if !s.order.is_empty() {
            if space {
                self.buf.push(' ');
            }
            self.buf.push_str("ORDER BY ");
            for (i, k) in s.order.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.order_item(&k.expr)?;
                self.sort_suffix(k);
            }
            space = true;
        }
        if s.frame & FRAME_NONDEFAULT != 0 {
            if space {
                self.buf.push(' ');
            }
            self.frame(s)?;
        }
        self.buf.push(')');
        Some(())
    }

    /// `get_window_frame_options`.
    fn frame(&mut self, s: &Spec) -> Option<()> {
        let f = s.frame;
        self.buf.push_str(if f & FRAME_RANGE != 0 {
            "RANGE "
        } else if f & FRAME_ROWS != 0 {
            "ROWS "
        } else {
            "GROUPS "
        });
        if f & FRAME_BETWEEN != 0 {
            self.buf.push_str("BETWEEN ");
        }
        if f & FRAME_START_UNBOUNDED_PRECEDING != 0 {
            self.buf.push_str("UNBOUNDED PRECEDING ");
        } else if f & FRAME_START_CURRENT_ROW != 0 {
            self.buf.push_str("CURRENT ROW ");
        } else {
            self.expr(s.start.as_ref()?, false)?;
            self.buf.push_str(if f & FRAME_START_OFFSET_PRECEDING != 0 {
                " PRECEDING "
            } else {
                " FOLLOWING "
            });
        }
        if f & FRAME_BETWEEN != 0 {
            self.buf.push_str("AND ");
            if f & FRAME_END_UNBOUNDED_FOLLOWING != 0 {
                self.buf.push_str("UNBOUNDED FOLLOWING ");
            } else if f & FRAME_END_CURRENT_ROW != 0 {
                self.buf.push_str("CURRENT ROW ");
            } else {
                self.expr(s.end.as_ref()?, false)?;
                self.buf.push_str(if f & FRAME_END_OFFSET_PRECEDING != 0 {
                    " PRECEDING "
                } else {
                    " FOLLOWING "
                });
            }
        }
        if f & FRAME_EXCLUDE_CURRENT_ROW != 0 {
            self.buf.push_str("EXCLUDE CURRENT ROW ");
        } else if f & FRAME_EXCLUDE_GROUP != 0 {
            self.buf.push_str("EXCLUDE GROUP ");
        } else if f & FRAME_EXCLUDE_TIES != 0 {
            self.buf.push_str("EXCLUDE TIES ");
        }
        self.buf.pop();
        Some(())
    }

    /// A sort item outside GROUP BY: a column whose name is also another select-list item's
    /// output name shows qualified, so it can't be read as that output column.
    fn order_item(&mut self, e: &Expr) -> Option<()> {
        if let (Expr::Col(i, _), Some(level)) = (e, self.view) {
            let (shown, qual) = (level.cols.get(*i)?, level.qual.get(*i)?);
            if shown != qual
                && level
                    .targets
                    .iter()
                    .any(|(n, x)| quote_ident(n) == *shown && !x.same_as(e))
            {
                self.buf.push_str(qual);
                return Some(());
            }
        }
        self.sort_group_item(e)
    }

    /// `get_rule_groupingset`.
    fn grouping_set(&mut self, g: &Group, omit_parens: bool) -> Option<()> {
        let (kw, children, omit_child) = match g {
            Group::Empty => {
                self.buf.push_str("()");
                return Some(());
            }
            Group::Expr(e) => return self.group_simple(std::slice::from_ref(e), omit_parens),
            Group::Simple(items) => return self.group_simple(items, omit_parens),
            Group::Rollup(c) => ("ROLLUP(", c, true),
            Group::Cube(c) => ("CUBE(", c, true),
            Group::Sets(c) => ("GROUPING SETS (", c, false),
        };
        self.buf.push_str(kw);
        for (i, c) in children.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.grouping_set(c, omit_child)?;
        }
        self.buf.push(')');
        Some(())
    }

    fn group_simple(&mut self, items: &[Expr], omit_parens: bool) -> Option<()> {
        let paren = !omit_parens || items.len() != 1;
        if paren {
            self.buf.push('(');
        }
        for (i, e) in items.iter().enumerate() {
            if i > 0 {
                self.buf.push_str(", ");
            }
            self.sort_group_item(e)?;
        }
        if paren {
            self.buf.push(')');
        }
        Some(())
    }

    /// OFFSET / LIMIT (or FETCH FIRST … WITH TIES).
    fn offset_limit(
        &mut self,
        offset: Option<&Expr>,
        limit: Option<&Expr>,
        with_ties: bool,
    ) -> Option<()> {
        if let Some(o) = offset {
            self.keyword(" OFFSET ", -STD, STD, 0);
            self.expr(o, false)?;
        }
        if let Some(l) = limit {
            if with_ties {
                self.keyword(" FETCH FIRST ", -STD, STD, 0);
                self.buf.push('(');
                self.expr(l, false)?;
                self.buf.push_str(") ROWS WITH TIES");
            } else {
                self.keyword(" LIMIT ", -STD, STD, 0);
                match l {
                    Expr::Const(Value::Null, _) => self.buf.push_str("ALL"),
                    l => self.expr(l, false)?,
                }
            }
        }
        Some(())
    }

    /// `get_basic_select_query` and the clauses after it.
    fn basic_select(&mut self, from: &[pb::Node], sel: &Select, wrap: usize) -> Option<()> {
        self.indent += STD;
        self.buf.push_str(if super::NO_INDENT.get() {
            "SELECT"
        } else {
            " SELECT"
        });
        match &sel.distinct {
            Some(on) if on.is_empty() => self.buf.push_str(" DISTINCT"),
            Some(on) => {
                self.buf.push_str(" DISTINCT ON (");
                for (i, e) in on.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    self.order_item(e)?;
                }
                self.buf.push(')');
            }
            None => {}
        }
        self.target_list(&sel.targets, wrap)?;
        // FROM.
        if let Some(text) = &sel.from_text {
            self.keyword(" FROM ", -STD, STD, 2);
            self.buf.push_str(text);
        }
        self.print_from_list(" FROM ", from, 0, wrap)?;
        if let Some(w) = &sel.where_ {
            self.keyword(" WHERE ", -STD, STD, 1);
            self.expr(w, false)?;
        }
        if !sel.group.is_empty() {
            self.keyword(" GROUP BY ", -STD, STD, 1);
            if sel.group_distinct {
                self.buf.push_str("DISTINCT ");
            }
            for (i, g) in sel.group.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                match g {
                    Group::Expr(e) => self.sort_group_item(e)?,
                    g => self.grouping_set(g, true)?,
                }
            }
        }
        if let Some(h) = &sel.having {
            self.keyword(" HAVING ", -STD, STD, 0);
            self.expr(h, false)?;
        }
        for (i, (name, spec)) in sel.windows.iter().enumerate() {
            if i == 0 {
                self.keyword(" WINDOW ", -STD, STD, 1);
            } else {
                self.buf.push_str(", ");
            }
            self.buf.push_str(&quote_ident(name));
            self.buf.push_str(" AS ");
            self.window_spec(spec)?;
        }
        if !sel.order.is_empty() {
            self.keyword(" ORDER BY ", -STD, STD, 1);
            for (i, k) in sel.order.iter().enumerate() {
                if i > 0 {
                    self.buf.push_str(", ");
                }
                self.order_item(&k.expr)?;
                self.sort_suffix(k);
            }
        }
        self.offset_limit(sel.offset.as_ref(), sel.limit.as_ref(), sel.with_ties)?;
        for (kw, rel, wait) in &sel.locks {
            self.keyword(kw, -STD, STD, 0);
            self.buf.push_str(" OF ");
            self.buf.push_str(&quote_ident(rel));
            self.buf.push_str(wait);
        }
        Some(())
    }

    /// `get_target_list`: an item goes on a new line when the current one would pass the wrap
    /// column, or the previous item spanned lines.
    fn target_list(
        &mut self,
        targets: &[(Expr, String, Option<String>)],
        wrap: usize,
    ) -> Option<()> {
        let mut last_multiline = false;
        for (n, (e, name, shown)) in targets.iter().enumerate() {
            self.buf.push_str(if n == 0 { " " } else { ", " });
            let outer = std::mem::take(&mut self.buf);
            // Postgres shows a target's implicit coercions; ours mostly have no counterpart
            // there, except XMLSERIALIZE's to its target type.
            let showimplicit =
                matches!(e, Expr::Cast(inner, ..) if matches!(**inner, Expr::Xml(_)));
            // A whole-row reference as a target is cast to its type (else `t.*` would
            // expand).
            let r = if self.whole_row(e).is_some() {
                self.expr_toplevel(e)
            } else {
                self.expr(e, showimplicit)
            };
            if shown.as_deref() != Some(name.as_str()) {
                self.buf.push_str(" AS ");
                self.buf.push_str(&quote_ident(name));
            }
            let item = std::mem::replace(&mut self.buf, outer);
            r?;
            if item.starts_with('\n') {
                while self.buf.ends_with(' ') {
                    self.buf.pop();
                }
            } else if n > 0 && (line_len(&self.buf) + item.len() > wrap || last_multiline) {
                self.keyword("", -STD, STD, JOIN);
            }
            last_multiline = item.trim_start_matches('\n').contains('\n');
            self.buf.push_str(&item);
        }
        Some(())
    }

    /// `get_from_clause`: `from`'s items after keyword `kw`, binding scope relations from
    /// `rel` on.
    fn print_from_list(
        &mut self,
        kw: &str,
        from: &[pb::Node],
        mut rel: usize,
        wrap: usize,
    ) -> Option<()> {
        for (n, item) in from.iter().enumerate() {
            if n == 0 {
                self.keyword(kw, -STD, STD, 2);
                self.print_from_item(item, &mut rel)?;
            } else {
                self.buf.push_str(", ");
                let outer = std::mem::take(&mut self.buf);
                let r = self.print_from_item(item, &mut rel);
                let text = std::mem::replace(&mut self.buf, outer);
                r?;
                if text.starts_with('\n') {
                    while self.buf.ends_with(' ') {
                        self.buf.pop();
                    }
                } else if line_len(&self.buf) + text.len() > wrap {
                    self.keyword("", -STD, STD, JOIN);
                }
                self.buf.push_str(&text);
            }
        }
        Some(())
    }

    /// A relation's name and alias (`get_rte_alias`): the alias shows when one was written,
    /// the range-table name differs from the relation's, or column aliases follow (`aliased`).
    fn relation(&mut self, rv: &pb::RangeVar, refname: &str, aliased: bool) -> Option<u32> {
        let full = if rv.schemaname.is_empty() {
            quote_ident(&rv.relname)
        } else {
            format!(
                "{}.{}",
                quote_ident(&rv.schemaname),
                quote_ident(&rv.relname)
            )
        };
        let names = self.ctx.fmt().names.clone()?;
        let oid = names.rel_oid(&full).ok()?;
        if !rv.inh {
            self.buf.push_str("ONLY ");
        }
        let (schema, real) = self
            .cat
            .tables
            .get(&oid)
            .map(|t| (t.schema, t.name.clone()))
            .or_else(|| self.cat.views.get(&oid).map(|v| (v.schema, v.name.clone())))
            .or_else(|| {
                crate::catalog::system::SysTable::by_oid(oid).map(|t| (t.schema(), t.name().to_string()))
            })?;
        let mut shown = names.rel_name(oid)?;
        // A name a WITH query hides is qualified (`generate_relation_name`).
        if shown == quote_ident(&real) && self.nested.as_deref().is_some_and(|n| n.is_cte(&real)) {
            shown = format!("{}.{shown}", quote_ident(self.cat.schema_name(schema)));
        }
        self.buf.push_str(&shown);
        if ast::written_alias(rv).is_some() || refname != real || aliased {
            self.buf.push(' ');
            self.buf.push_str(&quote_ident(refname));
        }
        Some(oid)
    }

    /// The live columns' names of the relation `rv` names.
    fn rel_column_names(&self, rv: &pb::RangeVar) -> Option<Vec<String>> {
        let full = if rv.schemaname.is_empty() {
            quote_ident(&rv.relname)
        } else {
            format!(
                "{}.{}",
                quote_ident(&rv.schemaname),
                quote_ident(&rv.relname)
            )
        };
        let oid = self.ctx.fmt().names.clone()?.rel_oid(&full).ok()?;
        self.cat
            .tables
            .get(&oid)
            .map(|t| {
                t.columns
                    .iter()
                    .filter(|c| !c.dropped)
                    .map(|c| c.name.clone())
                    .collect()
            })
            .or_else(|| {
                self.cat
                    .views
                    .get(&oid)
                    .map(|v| v.columns.iter().map(|c| c.0.clone()).collect())
            })
            .or_else(|| {
                crate::catalog::system::SysTable::by_oid(oid)
                    .map(|t| t.columns().iter().map(|c| c.0.clone()).collect())
            })
    }

    /// `get_from_clause_item`. `rel`: the next scope relation (FROM items bind in order).
    fn print_from_item(&mut self, n: &pb::Node, rel: &mut usize) -> Option<()> {
        let level = self.view?;
        match n.node.as_ref()? {
            NodeEnum::RangeVar(rv) => {
                let refname = level.refs.get(*rel)?.clone();
                *rel += 1;
                if rv.schemaname.is_empty() && self.nested.as_deref()?.is_cte(&rv.relname) {
                    self.buf.push_str(&quote_ident(&rv.relname));
                    if refname != rv.relname {
                        self.buf.push(' ');
                        self.buf.push_str(&quote_ident(&refname));
                    }
                    return Some(());
                }
                // Column aliases show when they differ from the column names.
                let colnames = rv
                    .alias
                    .as_ref()
                    .map(|a| ast::names(&a.colnames))
                    .unwrap_or_default();
                let list = if colnames.is_empty() {
                    None
                } else {
                    let real = self.rel_column_names(rv)?;
                    colnames
                        .iter()
                        .zip(&real)
                        .any(|(a, r)| a != r)
                        .then(|| {
                            real.iter()
                                .enumerate()
                                .map(|(i, r)| quote_ident(colnames.get(i).unwrap_or(r)))
                                .collect::<Vec<String>>()
                        })
                };
                self.relation(rv, &refname, list.is_some())?;
                if let Some(all) = list {
                    self.buf.push_str(&format!("({})", all.join(", ")));
                }
                Some(())
            }
            NodeEnum::RangeSubselect(rs) => {
                let refname = level.refs.get(*rel)?.clone();
                let idx = *rel;
                *rel += 1;
                if rs.lateral {
                    self.buf.push_str("LATERAL ");
                }
                self.buf.push('(');
                let nested = self.nested.as_deref_mut()?;
                // Only a LATERAL subquery sees the FROM items beside it.
                let mut beside = crate::sql::binder::Scope::default();
                let lateral = rs.lateral;
                if !lateral {
                    nested.binder(&mut |b| std::mem::swap(&mut b.level().scope, &mut beside));
                }
                let r = nested.query(
                    rs.subquery.as_deref()?,
                    &mut self.buf,
                    self.indent,
                    &level.qual,
                    Nest::From,
                );
                if !lateral {
                    nested.binder(&mut |b| std::mem::swap(&mut b.level().scope, &mut beside));
                }
                r?;
                self.buf.push(')');
                self.buf.push(' ');
                self.buf.push_str(&quote_ident(&refname));
                if rs.alias.as_ref().is_some_and(|a| !a.colnames.is_empty()) {
                    let all = self.rel_columns(idx)?;
                    self.buf.push_str(&format!(
                        "({})",
                        all.iter()
                            .map(|c| quote_ident(c))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                Some(())
            }
            // A FROM item that is an expression other than a function call (`COALESCE(…)`,
            // `CURRENT_DATE`, a cast, SQL-syntax calls): `get_rule_expr_funccall`.
            NodeEnum::RangeFunction(rf)
                if rf.functions.len() == 1
                    && rf.coldeflist.is_empty()
                    && !rf.ordinality
                    && matches!(rf.functions[0].node.as_ref(), Some(NodeEnum::List(l))
                        if l.items.len() == 1 || l.items.get(1).is_some_and(|c| c.node.is_none()))
                    && match rf.functions[0].node.as_ref() {
                        Some(NodeEnum::List(l)) => match l.items.first().and_then(|n| n.node.as_ref()) {
                            Some(NodeEnum::FuncCall(fc)) => {
                                fc.funcformat == pb::CoercionForm::CoerceSqlSyntax as i32
                            }
                            Some(_) => true,
                            None => false,
                        },
                        _ => false,
                    } =>
            {
                let refname = level.refs.get(*rel)?.clone();
                let idx = *rel;
                *rel += 1;
                let Some(NodeEnum::List(l)) = rf.functions[0].node.as_ref() else {
                    return None;
                };
                let node = l.items.first()?;
                let mut bound = None;
                let mut cols = Vec::new();
                self.nested.as_deref_mut()?.binder(&mut |b| {
                    bound = b.in_clause(Clause::FromFunction, |b| b.bind_expr(node)).ok();
                    if let Some(r) = b.scope().rels.get(idx) {
                        cols = r
                            .cols
                            .iter()
                            .filter(|c| !c.hidden)
                            .map(|c| quote_ident(&c.name))
                            .collect();
                    }
                });
                let e = bound?;
                if matches!(e, Expr::Srf(..)) {
                    return None;
                }
                if rf.lateral {
                    self.buf.push_str("LATERAL ");
                }
                if super::looks_like_function(&e) {
                    self.expr(&e, true)?;
                } else {
                    self.buf.push_str("CAST(");
                    self.expr(&e, true)?;
                    self.buf.push_str(&format!(" AS {})", self.type_name(&e.ty())));
                }
                self.buf
                    .push_str(&format!(" {}({})", quote_ident(&refname), cols.join(", ")));
                Some(())
            }
            NodeEnum::RangeFunction(rf) => {
                let refname = level.refs.get(*rel)?.clone();
                let idx = *rel;
                *rel += 1;
                // The functions (each with its column definitions, if any).
                let mut items = Vec::new();
                for f in &rf.functions {
                    let NodeEnum::List(l) = f.node.as_ref()? else {
                        return None;
                    };
                    let NodeEnum::FuncCall(fc) = l.items.first()?.node.as_ref()? else {
                        return None;
                    };
                    let defs: Vec<&pb::ColumnDef> =
                        match l.items.get(1).and_then(|c| c.node.as_ref()) {
                            Some(NodeEnum::List(cd)) => cd
                                .items
                                .iter()
                                .map(|c| {
                                    if let Some(NodeEnum::ColumnDef(d)) = &c.node {
                                        Some(&**d)
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Option<_>>()?,
                            _ => Vec::new(),
                        };
                    let fname = ast::names(&fc.funcname);
                    let (schema, name) = match fname.as_slice() {
                        [n] => (None, n.clone()),
                        [s, n] => (Some(s.clone()), n.clone()),
                        _ => return None,
                    };
                    items.push((fc, schema, name, defs));
                }
                let coldefs =
                    !rf.coldeflist.is_empty() || (items.len() == 1 && !items[0].3.is_empty());
                if items.is_empty() || (coldefs && rf.ordinality) {
                    return None;
                }
                // UNNEST(a, b) runs one unnest per argument.
                let multi_unnest =
                    items.len() == 1 && items[0].2 == "unnest" && items[0].0.args.len() > 1;
                let mut calls = Vec::new();
                // The item each call comes from (a many-argument unnest in ROWS FROM is one
                // unnest per argument).
                let mut call_items: Vec<usize> = Vec::new();
                let mut unnest_args = Vec::new();
                let mut cols = Vec::new();
                self.nested.as_deref_mut()?.binder(&mut |b| {
                    b.in_clause(Clause::FromFunction, |b| {
                        if multi_unnest {
                            for a in &items[0].0.args {
                                unnest_args.push(b.bind_expr(a)?);
                            }
                        } else {
                            for (k, (fc, schema, name, _)) in items.iter().enumerate() {
                                if name == "unnest"
                                    && fc.args.len() > 1
                                    && schema.as_deref().is_none_or(|s| s == "pg_catalog")
                                {
                                    for a in &fc.args {
                                        if let Ok(c) = b.resolve_call(
                                            name,
                                            std::slice::from_ref(a),
                                            false,
                                            fc.location,
                                        ) {
                                            calls.push(c);
                                            call_items.push(k);
                                        }
                                    }
                                    continue;
                                }
                                let before = calls.len();
                                let builtin = if schema.as_deref().is_none_or(|s| s == "pg_catalog")
                                {
                                    b.resolve_call(name, &fc.args, fc.func_variadic, fc.location)
                                        .ok()
                                } else {
                                    None
                                };
                                match builtin {
                                    Some(c) => calls.push(c),
                                    None => {
                                        let Some((fdef, args)) =
                                            b.resolve_user_function(schema.as_deref(), name, fc)?
                                        else {
                                            return Ok(());
                                        };
                                        calls.push((
                                            crate::sql::func::display_def(&fdef),
                                            args,
                                            fdef.ret.clone(),
                                        ));
                                    }
                                }
                                if calls.len() > before {
                                    call_items.push(k);
                                }
                            }
                        }
                        Ok(())
                    })
                    .ok();
                    if let Some(r) = b.scope().rels.get(idx) {
                        cols = r
                            .cols
                            .iter()
                            .filter(|c| !c.hidden)
                            .map(|c| (c.name.clone(), c.expr.ty()))
                            .collect();
                    }
                });
                let covered: std::collections::BTreeSet<usize> = call_items.iter().copied().collect();
                if covered.len() + usize::from(multi_unnest) != items.len() {
                    return None;
                }
                // A function reading earlier FROM items is LATERAL.
                let mut lateral = rf.lateral;
                for e in calls.iter().flat_map(|c| c.1.iter()).chain(&unnest_args) {
                    e.walk(&mut |x| lateral |= matches!(x, Expr::Col(..)));
                }
                if lateral {
                    self.buf.push_str("LATERAL ");
                }
                let all_unnest =
                    calls.len() > 1 && calls.iter().all(|c| c.0.name == "unnest" && c.1.len() == 1);
                if multi_unnest || all_unnest {
                    let args: Vec<Expr> = if multi_unnest {
                        unnest_args
                    } else {
                        calls.into_iter().flat_map(|c| c.1).collect()
                    };
                    self.buf.push_str("UNNEST(");
                    self.list(&args)?;
                    self.buf.push(')');
                } else if let [(def, args, ret)] = calls.as_slice() {
                    self.expr(&Expr::Call(def.clone(), args.clone(), ret.clone()), false)?;
                } else {
                    self.buf.push_str("ROWS FROM(");
                    for (i, (def, args, ret)) in calls.into_iter().enumerate() {
                        if i > 0 {
                            self.buf.push_str(", ");
                        }
                        self.expr(&Expr::Call(def, args, ret), false)?;
                        let defs = &items[*call_items.get(i)?].3;
                        if !defs.is_empty() {
                            let mut shown = Vec::new();
                            for d in defs {
                                let (t, _) = crate::sql::binder::resolve_type_name(
                                    self.cat,
                                    &self.ctx.search_path,
                                    d.type_name.as_ref()?,
                                )
                                .ok()?;
                                shown.push(format!(
                                    "{} {}",
                                    quote_ident(&d.colname),
                                    self.type_name(&t)
                                ));
                            }
                            self.buf.push_str(&format!(" AS ({})", shown.join(", ")));
                        }
                    }
                    self.buf.push(')');
                }
                if rf.ordinality {
                    self.buf.push_str(" WITH ORDINALITY");
                }
                let cols: Vec<String> = cols
                    .iter()
                    .map(|(n, t)| {
                        if coldefs {
                            format!("{} {}", quote_ident(n), self.type_name(t))
                        } else {
                            quote_ident(n)
                        }
                    })
                    .collect();
                self.buf
                    .push_str(&format!(" {}({})", quote_ident(&refname), cols.join(", ")));
                Some(())
            }
            NodeEnum::JoinExpr(j) if j.alias.is_some() => {
                let refname = level.refs.get(*rel)?.clone();
                let idx = *rel;
                *rel += 1;
                // Inside the alias: the names it hides, and the scope its conditions bind in.
                let inner = &level
                    .joins
                    .iter()
                    .find(|(p, _)| *p == &**j as *const pb::JoinExpr as usize)?
                    .1;
                let (view, cols) = (self.view, self.cols);
                self.view = Some(inner);
                self.cols = &inner.cols;
                let mut scope = inner.scope.clone()?;
                self.nested
                    .as_deref_mut()?
                    .binder(&mut |b| std::mem::swap(&mut b.level().scope, &mut scope));
                let r = self.join(j, &mut 0, true);
                self.nested
                    .as_deref_mut()?
                    .binder(&mut |b| std::mem::swap(&mut b.level().scope, &mut scope));
                (self.view, self.cols) = (view, cols);
                r?;
                self.buf.push(' ');
                self.buf.push_str(&quote_ident(&refname));
                let all = self.rel_columns(idx)?;
                // Without column aliases, the join's columns show only when two would share
                // a name (columns added below it since): they're made unique
                // (set_join_column_names).
                let mut unique: Vec<String> = Vec::with_capacity(all.len());
                for c in &all {
                    let mut n = c.clone();
                    let mut i = 0;
                    while unique.contains(&n) {
                        i += 1;
                        n = crate::catalog::suffixed_name(c, &format!("_{i}"));
                    }
                    unique.push(n);
                }
                let written = j.alias.as_ref().is_some_and(|a| !a.colnames.is_empty());
                if written || unique != all {
                    self.buf.push_str(&format!(
                        "({})",
                        unique
                            .iter()
                            .map(|c| quote_ident(c))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                Some(())
            }
            NodeEnum::JoinExpr(j) => self.join(j, rel, !self.pretty),
            NodeEnum::RangeTableSample(ts) => {
                let refname = level.refs.get(*rel)?.clone();
                *rel += 1;
                let Some(NodeEnum::RangeVar(rv)) =
                    ts.relation.as_deref().and_then(|n| n.node.as_ref())
                else {
                    return None;
                };
                self.relation(rv, &refname, false)?;
                let method = ast::names(&ts.method).pop()?;
                self.buf
                    .push_str(&format!(" TABLESAMPLE {} (", quote_ident(&method)));
                let mut args = Vec::new();
                let mut seed = None;
                self.nested.as_deref_mut()?.binder(&mut |b| {
                    args = ts
                        .args
                        .iter()
                        .filter_map(|a| {
                            b.bind_expr(a)
                                .ok()
                                .and_then(|e| b.coerce_implicit(e, &DataType::Float4).ok())
                        })
                        .collect();
                    seed = ts.repeatable.as_deref().and_then(|r| {
                        b.bind_expr(r)
                            .ok()
                            .and_then(|e| b.coerce_implicit(e, &DataType::Float8).ok())
                    });
                });
                if args.len() != ts.args.len() || seed.is_some() != ts.repeatable.is_some() {
                    return None;
                }
                for (i, e) in args.iter().enumerate() {
                    if i > 0 {
                        self.buf.push_str(", ");
                    }
                    self.expr(e, false)?;
                }
                self.buf.push(')');
                if let Some(e) = seed {
                    self.buf.push_str(" REPEATABLE (");
                    self.expr(&e, false)?;
                    self.buf.push(')');
                }
                Some(())
            }
            NodeEnum::RangeTableFunc(rtf) => {
                *rel += 1;
                let mut bound = None;
                self.nested
                    .as_deref_mut()?
                    .binder(&mut |b| bound = b.bind_xml_table(rtf).ok());
                let (plan, _, params) = bound?;
                let Plan::FunctionScan {
                    mut calls, alias, ..
                } = plan
                else {
                    return None;
                };
                if !params.is_empty() || rtf.lateral {
                    self.buf.push_str("LATERAL ");
                }
                // Lateral references come back as the enclosing level's columns.
                if !params.is_empty() {
                    let args = std::mem::take(&mut calls[0].args);
                    calls[0].args = args
                        .into_iter()
                        .map(|a| {
                            a.transform(&mut |e| match e {
                                Expr::Outer(1, i, _) => Ok(params[i].clone()),
                                e => Ok(e),
                            })
                        })
                        .collect::<crate::error::Result<_>>()
                        .ok()?;
                }
                let text = {
                    let mut show = |e: &Expr| {
                        let saved = std::mem::take(&mut self.buf);
                        // get_tablefunc shows implicit casts.
                        let r = self.expr(e, true);
                        let out = std::mem::replace(&mut self.buf, saved);
                        r.map(|_| out)
                    };
                    crate::sql::func::xmltable::xml_table_text(&calls[0].args, &mut show)?
                };
                self.buf.push_str(&text);
                if rtf.alias.is_some() {
                    self.buf.push(' ');
                    self.buf.push_str(&quote_ident(&alias));
                }
                Some(())
            }
            NodeEnum::JsonTable(jt) => {
                *rel += 1;
                let mut bound = None;
                self.nested
                    .as_deref_mut()?
                    .binder(&mut |b| bound = b.bind_json_table(jt).ok());
                let (plan, _, params) = bound?;
                let Plan::FunctionScan {
                    mut calls, alias, ..
                } = plan
                else {
                    return None;
                };
                // Lateral references come back as the enclosing level's columns.
                if !params.is_empty() {
                    self.buf.push_str("LATERAL ");
                    let args = std::mem::take(&mut calls[0].args);
                    calls[0].args = args
                        .into_iter()
                        .map(|a| {
                            a.transform(&mut |e| match e {
                                Expr::Outer(1, i, _) => Ok(params[i].clone()),
                                e => Ok(e),
                            })
                        })
                        .collect::<crate::error::Result<_>>()
                        .ok()?;
                }
                let pretty = Some(self.indent);
                let text = {
                    let mut show = |e: &Expr| {
                        let saved = std::mem::take(&mut self.buf);
                        let r = self.expr(e, false);
                        let out = std::mem::replace(&mut self.buf, saved);
                        r.map(|_| out)
                    };
                    super::json_table_text(
                        &calls[0],
                        &mut show,
                        &|t: &DataType| t.display(),
                        pretty,
                        false,
                    )?
                };
                self.buf.push_str(&text);
                if jt.alias.is_some() {
                    self.buf.push(' ');
                    self.buf.push_str(&quote_ident(&alias));
                }
                Some(())
            }
            _ => None,
        }
    }

    /// A join's `larg JOIN rarg ...`, parenthesized if `paren`.
    fn join(&mut self, j: &pb::JoinExpr, rel: &mut usize, paren: bool) -> Option<()> {
        if paren {
            self.buf.push('(');
        }
        // USING's merged columns (and its alias's) come first in the join's scope.
        let using = if j.is_natural {
            self.nested.as_deref()?.natural_using(j)?
        } else {
            ast::names(&j.using_clause)
        };
        if !using.is_empty() {
            *rel += 1 + usize::from(j.join_using_alias.is_some());
        }
        self.print_from_item(j.larg.as_deref()?, rel)?;
        let quals = j.quals.as_deref();
        let kw = match pb::JoinType::try_from(j.jointype).ok()? {
            pb::JoinType::JoinInner if quals.is_none() && using.is_empty() => " CROSS JOIN ",
            pb::JoinType::JoinInner => " JOIN ",
            pb::JoinType::JoinLeft => " LEFT JOIN ",
            pb::JoinType::JoinRight => " RIGHT JOIN ",
            pb::JoinType::JoinFull => " FULL JOIN ",
            _ => return None,
        };
        self.keyword(kw, -STD, STD, JOIN);
        let paren_right =
            self.pretty && matches!(j.rarg.as_deref()?.node, Some(NodeEnum::JoinExpr(_)));
        if paren_right {
            self.buf.push('(');
        }
        self.print_from_item(j.rarg.as_deref()?, rel)?;
        if paren_right {
            self.buf.push(')');
        }
        if !using.is_empty() {
            self.buf.push_str(&format!(
                " USING ({})",
                using
                    .iter()
                    .map(|c| quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            if let Some(a) = &j.join_using_alias {
                self.buf.push_str(" AS ");
                self.buf.push_str(&quote_ident(&a.aliasname));
            }
        } else if let Some(q) = quals {
            let mut e = None;
            self.nested.as_deref_mut()?.binder(&mut |b| {
                e = b
                    .in_clause(Clause::JoinOn, |b| {
                        let e = b.bind_expr(q)?;
                        b.coerce_bool(e, "JOIN/ON")
                    })
                    .ok();
            });
            self.buf.push_str(" ON ");
            self.open();
            self.expr(&e?, false)?;
            self.close();
        } else if kw != " CROSS JOIN " {
            self.buf.push_str(" ON TRUE");
        }
        if paren {
            self.buf.push(')');
        }
        Some(())
    }

    /// The column names of scope relation `rel` (a FROM subquery's, after its aliases).
    fn rel_columns(&mut self, rel: usize) -> Option<Vec<String>> {
        let mut out = None;
        self.nested.as_deref_mut()?.binder(&mut |b| {
            out = b.scope().rels.get(rel).map(|r| {
                r.cols
                    .iter()
                    .filter(|c| !c.hidden)
                    .map(|c| c.name.clone())
                    .collect()
            });
        });
        out
    }
}

/// `frameOptions` bits (nodes/parsenodes.h).
const FRAME_NONDEFAULT: i32 = 0x00001;
const FRAME_RANGE: i32 = 0x00002;
const FRAME_ROWS: i32 = 0x00004;
const FRAME_BETWEEN: i32 = 0x00010;
const FRAME_START_UNBOUNDED_PRECEDING: i32 = 0x00020;
const FRAME_END_UNBOUNDED_FOLLOWING: i32 = 0x00100;
const FRAME_START_CURRENT_ROW: i32 = 0x00200;
const FRAME_END_CURRENT_ROW: i32 = 0x00400;
const FRAME_START_OFFSET_PRECEDING: i32 = 0x00800;
const FRAME_END_OFFSET_PRECEDING: i32 = 0x01000;
const FRAME_EXCLUDE_CURRENT_ROW: i32 = 0x08000;
const FRAME_EXCLUDE_GROUP: i32 = 0x10000;
const FRAME_EXCLUDE_TIES: i32 = 0x20000;
