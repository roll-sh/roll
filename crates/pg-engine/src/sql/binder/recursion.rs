//! Where a recursive WITH query may reference itself (parse_cte.c's
//! `checkWellFormedRecursion`), and the order a `WITH RECURSIVE` list is analyzed in
//! (`makeDependencyGraph` / `TopologicalSort`).

use super::ast::{inner, with_pos};
use crate::error::{Error, Result, code};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};

/// `RecursionContext`.
#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Ok,
    NonRecursiveTerm,
    Sublink,
    OuterJoin,
    Intersect,
    Except,
}

impl Ctx {
    fn message(self, name: &str) -> String {
        let place = match self {
            Ctx::Ok => "",
            Ctx::NonRecursiveTerm => "its non-recursive term",
            Ctx::Sublink => "a subquery",
            Ctx::OuterJoin => "an outer join",
            Ctx::Intersect => "INTERSECT",
            Ctx::Except => "EXCEPT",
        };
        format!("recursive reference to query \"{name}\" must not appear within {place}")
    }
}

struct Walker<'a> {
    name: &'a str,
    /// Names of WITH lists inside, which hide the query's name.
    innerwiths: Vec<Vec<String>>,
    selfrefs: usize,
    ctx: Ctx,
}

/// `checkWellFormedRecursion` for one recursive CTE whose query is `s` (a UNION).
pub(crate) fn check_well_formed(s: &pb::SelectStmt, cte: &pb::CommonTableExpr) -> Result<()> {
    let mut w = Walker {
        name: &cte.ctename,
        innerwiths: Vec::new(),
        selfrefs: 0,
        ctx: Ctx::NonRecursiveTerm,
    };
    if let Some(l) = s.larg.as_deref() {
        w.select(l)?;
    }
    w.selfrefs = 0;
    w.ctx = Ctx::Ok;
    if let Some(r) = s.rarg.as_deref() {
        w.select(r)?;
    }
    if let Some(with) = &s.with_clause {
        w.selfrefs = 0;
        w.ctx = Ctx::Sublink;
        w.list(&with.ctes)?;
    }
    Ok(())
}

impl Walker<'_> {
    fn list(&mut self, ns: &[Node]) -> Result<()> {
        ns.iter().try_for_each(|n| self.node(n))
    }

    fn opt(&mut self, n: Option<&Node>) -> Result<()> {
        n.map_or(Ok(()), |n| self.node(n))
    }

    fn node(&mut self, n: &Node) -> Result<()> {
        let Some(ne) = inner(n) else {
            return Ok(());
        };
        let save = self.ctx;
        match ne {
            NodeEnum::RangeVar(rv) => self.range_var(rv),
            NodeEnum::SelectStmt(s) => self.select(s),
            NodeEnum::WithClause(_) => Ok(()),
            NodeEnum::JoinExpr(j) => {
                use pb::JoinType as J;
                let (l, r, q) = (j.larg.as_deref(), j.rarg.as_deref(), j.quals.as_deref());
                let outer = |w: &mut Self| {
                    if save == Ctx::Ok {
                        w.ctx = Ctx::OuterJoin;
                    }
                };
                match J::try_from(j.jointype).unwrap_or(J::JoinInner) {
                    J::JoinLeft => {
                        self.opt(l)?;
                        outer(self);
                        self.opt(r)?;
                        self.ctx = save;
                        self.opt(q)
                    }
                    J::JoinFull => {
                        outer(self);
                        self.opt(l)?;
                        self.opt(r)?;
                        self.ctx = save;
                        self.opt(q)
                    }
                    J::JoinRight => {
                        outer(self);
                        self.opt(l)?;
                        self.ctx = save;
                        self.opt(r)?;
                        self.opt(q)
                    }
                    _ => {
                        self.opt(l)?;
                        self.opt(r)?;
                        self.opt(q)
                    }
                }
            }
            NodeEnum::SubLink(sl) => {
                // The subquery is independent of the outer context.
                self.ctx = Ctx::Sublink;
                self.opt(sl.subselect.as_deref())?;
                self.ctx = save;
                self.opt(sl.testexpr.as_deref())
            }
            NodeEnum::List(l) => self.list(&l.items),
            NodeEnum::ResTarget(r) => {
                self.list(&r.indirection)?;
                self.opt(r.val.as_deref())
            }
            NodeEnum::AExpr(a) => {
                self.opt(a.lexpr.as_deref())?;
                self.opt(a.rexpr.as_deref())
            }
            NodeEnum::BoolExpr(b) => self.list(&b.args),
            NodeEnum::FuncCall(f) => {
                self.list(&f.args)?;
                self.list(&f.agg_order)?;
                self.opt(f.agg_filter.as_deref())?;
                if let Some(o) = &f.over {
                    self.window(o)?;
                }
                Ok(())
            }
            NodeEnum::TypeCast(t) => self.opt(t.arg.as_deref()),
            NodeEnum::CollateClause(c) => self.opt(c.arg.as_deref()),
            NodeEnum::NullTest(t) => self.opt(t.arg.as_deref()),
            NodeEnum::BooleanTest(t) => self.opt(t.arg.as_deref()),
            NodeEnum::NamedArgExpr(a) => self.opt(a.arg.as_deref()),
            NodeEnum::CaseExpr(c) => {
                self.opt(c.arg.as_deref())?;
                self.list(&c.args)?;
                self.opt(c.defresult.as_deref())
            }
            NodeEnum::CaseWhen(c) => {
                self.opt(c.expr.as_deref())?;
                self.opt(c.result.as_deref())
            }
            NodeEnum::CoalesceExpr(c) => self.list(&c.args),
            NodeEnum::MinMaxExpr(m) => self.list(&m.args),
            NodeEnum::RowExpr(r) => self.list(&r.args),
            NodeEnum::AArrayExpr(a) => self.list(&a.elements),
            NodeEnum::AIndirection(a) => {
                self.opt(a.arg.as_deref())?;
                self.list(&a.indirection)
            }
            NodeEnum::AIndices(a) => {
                self.opt(a.lidx.as_deref())?;
                self.opt(a.uidx.as_deref())
            }
            NodeEnum::SortBy(s) => self.opt(s.node.as_deref()),
            NodeEnum::GroupingSet(g) => self.list(&g.content),
            NodeEnum::WindowDef(w) => self.window(w),
            NodeEnum::RangeSubselect(r) => self.opt(r.subquery.as_deref()),
            NodeEnum::RangeFunction(r) => self.list(&r.functions),
            NodeEnum::RangeTableSample(r) => {
                self.opt(r.relation.as_deref())?;
                self.list(&r.args)?;
                self.opt(r.repeatable.as_deref())
            }
            NodeEnum::CommonTableExpr(c) => self.opt(c.ctequery.as_deref()),
            NodeEnum::ColumnRef(_)
            | NodeEnum::AConst(_)
            | NodeEnum::ParamRef(_)
            | NodeEnum::AStar(_)
            | NodeEnum::String(_)
            | NodeEnum::Integer(_)
            | NodeEnum::Float(_)
            | NodeEnum::Boolean(_)
            | NodeEnum::TypeName(_)
            | NodeEnum::SetToDefault(_)
            | NodeEnum::LockingClause(_) => Ok(()),
            // Anything else: its relation references count where it stands.
            other => {
                for (d, _, _, _) in other.nodes() {
                    if let pg_query::NodeRef::RangeVar(rv) = d {
                        self.range_var(rv)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn window(&mut self, w: &pb::WindowDef) -> Result<()> {
        self.list(&w.partition_clause)?;
        self.list(&w.order_clause)?;
        self.opt(w.start_offset.as_deref())?;
        self.opt(w.end_offset.as_deref())
    }

    fn range_var(&mut self, rv: &pb::RangeVar) -> Result<()> {
        if !rv.schemaname.is_empty() || self.innerwiths.iter().flatten().any(|n| *n == rv.relname)
        {
            return Ok(());
        }
        if rv.relname != self.name {
            return Ok(());
        }
        if self.ctx != Ctx::Ok {
            return Err(with_pos(
                Error::new(code::INVALID_RECURSION, self.ctx.message(self.name)),
                rv.location,
            ));
        }
        self.selfrefs += 1;
        if self.selfrefs > 1 {
            return Err(with_pos(
                Error::new(
                    code::INVALID_RECURSION,
                    format!(
                        "recursive reference to query \"{}\" must not appear more than once",
                        self.name
                    ),
                ),
                rv.location,
            ));
        }
        Ok(())
    }

    fn select(&mut self, s: &pb::SelectStmt) -> Result<()> {
        let Some(with) = &s.with_clause else {
            return self.select_body(s);
        };
        let ctes = || {
            with.ctes.iter().filter_map(|c| match inner(c) {
                Some(NodeEnum::CommonTableExpr(c)) => Some(c),
                _ => None,
            })
        };
        if with.recursive {
            // All of a recursive WITH's names are visible to its items and the main query.
            self.innerwiths
                .push(ctes().map(|c| c.ctename.clone()).collect());
            for c in ctes() {
                self.opt(c.ctequery.as_deref())?;
            }
        } else {
            // Otherwise each name is visible to the items after it.
            self.innerwiths.push(Vec::new());
            for c in ctes() {
                self.opt(c.ctequery.as_deref())?;
                self.innerwiths.last_mut().unwrap().push(c.ctename.clone());
            }
        }
        let r = self.select_body(s);
        self.innerwiths.pop();
        r
    }

    /// `checkWellFormedSelectStmt`.
    fn select_body(&mut self, s: &pb::SelectStmt) -> Result<()> {
        use pb::SetOperation as Op;
        let save = self.ctx;
        if save != Ctx::Ok {
            return self.select_children(s);
        }
        let op = Op::try_from(s.op).unwrap_or(Op::SetopNone);
        let (l, r) = (s.larg.as_deref(), s.rarg.as_deref());
        match op {
            Op::SetopIntersect | Op::SetopExcept => {
                if s.all {
                    self.ctx = if op == Op::SetopIntersect {
                        Ctx::Intersect
                    } else {
                        Ctx::Except
                    };
                }
                if let Some(l) = l {
                    self.select(l)?;
                }
                if op == Op::SetopExcept {
                    self.ctx = Ctx::Except;
                }
                if let Some(r) = r {
                    self.select(r)?;
                }
                self.ctx = save;
                self.list(&s.sort_clause)?;
                self.opt(s.limit_offset.as_deref())?;
                self.opt(s.limit_count.as_deref())
            }
            _ => self.select_children(s),
        }
    }

    /// A SELECT's parts in `raw_expression_tree_walker`'s order.
    fn select_children(&mut self, s: &pb::SelectStmt) -> Result<()> {
        self.list(&s.distinct_clause)?;
        self.list(&s.target_list)?;
        self.list(&s.from_clause)?;
        self.opt(s.where_clause.as_deref())?;
        self.list(&s.group_clause)?;
        self.opt(s.having_clause.as_deref())?;
        self.list(&s.window_clause)?;
        self.list(&s.values_lists)?;
        self.list(&s.sort_clause)?;
        self.opt(s.limit_offset.as_deref())?;
        self.opt(s.limit_count.as_deref())?;
        if let Some(l) = s.larg.as_deref() {
            self.select(l)?;
        }
        if let Some(r) = s.rarg.as_deref() {
            self.select(r)?;
        }
        Ok(())
    }
}

/// The order a `WITH RECURSIVE` list's items are analyzed in: each after those it reads
/// (`TopologicalSort`), or "mutual recursion" when some read each other.
pub(crate) fn analysis_order(ctes: &[&pb::CommonTableExpr]) -> Result<Vec<usize>> {
    let names: Vec<&str> = ctes.iter().map(|c| c.ctename.as_str()).collect();
    // depends[i]: the other items item i reads.
    let mut depends: Vec<Vec<usize>> = ctes
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut refs = Vec::new();
            if let Some(q) = c.ctequery.as_deref().and_then(inner) {
                for (d, _, _, _) in q.nodes() {
                    if let pg_query::NodeRef::RangeVar(rv) = d
                        && rv.schemaname.is_empty()
                        && let Some(j) = names.iter().position(|n| *n == rv.relname)
                        && j != i
                        && !refs.contains(&j)
                    {
                        refs.push(j);
                    }
                }
            }
            refs
        })
        .collect();
    let mut items: Vec<usize> = (0..ctes.len()).collect();
    for i in 0..items.len() {
        let Some(j) = (i..items.len()).find(|&j| depends[items[j]].is_empty()) else {
            return Err(with_pos(
                Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "mutual recursion between WITH items is not implemented",
                ),
                ctes[items[i]].location,
            ));
        };
        items.swap(i, j);
        let done = items[i];
        for &k in &items[i + 1..] {
            depends[k].retain(|&d| d != done);
        }
    }
    Ok(items)
}
