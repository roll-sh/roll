//! Outputs of a FROM sub-select or view that the query above never reads. Postgres never
//! plans them: a sub-select it pulls up leaves them behind with the rest of its target list,
//! one it keeps has them replaced by NULLs first (remove_unused_subquery_outputs), so the
//! immutable PL/pgSQL calls in them aren't run while planning (evaluate_function). The
//! engine binds such an output with its calls left (`Binder::skip_procs`); the projection
//! nothing reads is dropped before anything runs.

use super::Binder;
use super::ast::inner;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::collections::HashSet;
use std::ptr::NonNull;

/// What a query level reads of one FROM sub-select or view (`Binder::outer_reads`).
pub(crate) struct OuterReads {
    /// The sub-select's or view's query level.
    pub(crate) level: usize,
    /// Every column name the level above names anywhere (but in its own unread outputs).
    names: HashSet<String>,
    /// The output names the level above sees: its alias's column names, a view's columns.
    rename: Vec<String>,
}

impl OuterReads {
    /// Whether output `i`, named `name` by its own select, is read by nothing above.
    pub(crate) fn unread(&self, i: usize, name: &str) -> bool {
        !self.names.contains(self.rename.get(i).map_or(name, String::as_str))
    }
}

/// Whether the outputs of select `s` nothing above reads can go (remove_unused_subquery_outputs
/// keeps everything under DISTINCT and every sort or group key; a set operation's leaves
/// aren't bound by `bind_simple_select` with reads).
pub(crate) fn prunable(s: &pb::SelectStmt) -> bool {
    s.distinct_clause.is_empty() && s.group_clause.is_empty() && s.sort_clause.is_empty() && s.having_clause.is_none()
}

impl Binder<'_, '_> {
    /// Sets the FROM list being bound (`bind_from` over select `s`), whose names decide what
    /// its sub-selects and views must produce; returns the one it replaces.
    pub(crate) fn set_from_stmt(&mut self, s: Option<&pb::SelectStmt>) -> Option<(usize, NonNull<pb::SelectStmt>)> {
        let lvl = self.levels.len();
        std::mem::replace(&mut self.from_stmt, s.map(|s| (lvl, NonNull::from(s))))
    }

    /// Before binding a sub-select or view of the FROM list being bound, known to the level as
    /// `alias`, whose outputs it sees as `rename` (where given): what the level reads of it.
    /// None when it may read every output (a `*`, a whole-row reference, a NATURAL join) or
    /// the statement isn't planned.
    pub(crate) fn outer_reads(&self, alias: &str, rename: Vec<String>) -> Option<OuterReads> {
        if self.deparse || self.unplanned || self.deps.is_some() || self.refs.is_some() {
            return None;
        }
        let level = self.levels.len();
        let (at, ptr) = self.from_stmt?;
        if at != level {
            return None;
        }
        // SAFETY: `from_stmt` is set only while `bind_from` runs over the statement it points
        // to (`bind_simple_select` holds the borrow and restores the field after).
        let s = unsafe { ptr.as_ref() };
        // This level's own outputs nothing above reads don't read anything either.
        let own = self.outer_reads.as_ref().filter(|r| r.level == level && prunable(s));
        let has_star = |n: &pb::Node| {
            matches!(inner(n), Some(NodeEnum::ResTarget(rt))
                if matches!(rt.val.as_deref().and_then(inner), Some(NodeEnum::ColumnRef(c))
                    if matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))))
        };
        let own = own.filter(|_| !s.target_list.iter().any(has_star));
        let mut names = HashSet::new();
        let mut visit = |n: pg_query::NodeRef| -> bool {
            match n {
                pg_query::NodeRef::ColumnRef(c) => {
                    for f in &c.fields {
                        match inner(f) {
                            Some(NodeEnum::AStar(_)) => return false,
                            Some(NodeEnum::String(s)) => {
                                names.insert(s.sval.clone());
                            }
                            _ => {}
                        }
                    }
                }
                pg_query::NodeRef::JoinExpr(j) => {
                    if j.is_natural {
                        return false;
                    }
                    for u in &j.using_clause {
                        if let Some(NodeEnum::String(s)) = inner(u) {
                            names.insert(s.sval.clone());
                        }
                    }
                }
                _ => {}
            }
            true
        };
        let mut walk = |n: &pb::Node| n.node.as_ref().is_none_or(|n| n.nodes().into_iter().all(|(n, ..)| visit(n)));
        for (i, t) in s.target_list.iter().enumerate() {
            if let (Some(own), Some(NodeEnum::ResTarget(rt))) = (own, inner(t)) {
                let name = match rt.name.as_str() {
                    "" => super::select::figure_colname(rt.val.as_deref()?),
                    n => n.to_string(),
                };
                if own.unread(i, &name) {
                    continue;
                }
            }
            if !walk(t) {
                return None;
            }
        }
        let clauses = s.from_clause.iter().chain(&s.group_clause).chain(&s.window_clause).chain(&s.sort_clause).chain(&s.distinct_clause);
        let opt = [&s.where_clause, &s.having_clause, &s.limit_offset, &s.limit_count];
        if !clauses.chain(opt.into_iter().filter_map(|n| n.as_deref())).all(&mut walk) {
            return None;
        }
        if names.contains(alias) {
            return None;
        }
        Some(OuterReads { level: level + 1, names, rename })
    }
}
