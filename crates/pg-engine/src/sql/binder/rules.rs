//! The rule system for INSERT / UPDATE / DELETE (rewriteHandler.c's RewriteQuery, fireRules,
//! rewriteRuleAction, CopyAndAddInvertedQual): a statement on a relation with rules becomes
//! the rules' actions, run over the statement's rows, and the statement itself unless an
//! unconditional INSTEAD rule replaces it, in Postgres's order (an INSERT before its rules'
//! actions, an UPDATE or DELETE after them).
//!
//! An action reads the statement's rows through a `*RULE*` FROM item put first in its FROM
//! list: the rows the INSERT inserts (NEW), or the rows the UPDATE / DELETE touches (OLD, and
//! NEW: the updated values). OLD and NEW are reachable only qualified, as in Postgres, where
//! they are range-table entries in the relation namespace only.

use super::ast::{self, inner, with_pos};
use super::{Binder, Clause, Level, Scope, ScopeRel, derived_rel, table_rel};
use crate::catalog::{Relation, Rule, Table};
use crate::error::{Error, Result, code};
use crate::sql::expr::{BoolTest, Expr};
use crate::sql::plan::{JoinKind, OutCol, Plan};
use crate::types::DataType;
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};
use std::sync::Arc;

/// The FROM item an action reads the statement's rows through.
pub(crate) const RULE_RTE: &str = "*RULE*";

/// What a rule action reads OLD / NEW from: the triggering statement's rows.
#[derive(Clone, Debug)]
pub(crate) struct RuleSource {
    pub plan: Arc<Plan>,
    /// The rule's relation (a view as a table).
    pub table: Arc<Table>,
    /// Where OLD's and NEW's columns start in its rows (one per table column).
    pub old: Option<usize>,
    pub new: Option<usize>,
}

impl RuleSource {
    /// OLD and NEW as scope entries (`qualified_only`: as an action sees them; otherwise as
    /// the rule's WHERE condition does).
    pub fn rels(&self, qualified_only: bool) -> Vec<ScopeRel> {
        let mut out = Vec::new();
        for (name, at) in [("old", self.old), ("new", self.new)] {
            let Some(at) = at else { continue };
            let mut r = table_rel(&self.table, name, None);
            for c in &mut r.cols {
                c.expr = c.expr.clone().shift_cols(at as isize);
            }
            r.table = None;
            r.rel = Some(self.table.oid);
            r.qualified_only = qualified_only;
            out.push(r);
        }
        out
    }

    /// The types of its rows' columns.
    fn types(&self) -> Vec<DataType> {
        let n = self.table.columns.len();
        let w = self.old.map_or(0, |o| o + n).max(self.new.map_or(0, |o| o + n));
        let mut out = vec![DataType::Bool; w];
        for at in [self.old, self.new].into_iter().flatten() {
            for (i, c) in self.table.columns.iter().enumerate() {
                out[at + i] = c.ty.clone();
            }
        }
        out
    }
}

/// Where a rewritten statement came from (`QuerySource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QSrc {
    Original,
    Instead,
    QualInstead,
    Also,
}

/// One statement the rule system produced.
pub(crate) struct Piece {
    pub plan: Plan,
    pub cols: Vec<OutCol>,
    pub src: QSrc,
    /// 'i', 'u', 'd', 's' (a SELECT) or 'n' (NOTIFY).
    pub cmd: char,
}

/// The FROM item standing for the statement's rows.
fn marker() -> Node {
    ast::mk(NodeEnum::RangeVar(pb::RangeVar {
        relname: RULE_RTE.into(),
        inh: true,
        relpersistence: "p".into(),
        location: -1,
        ..Default::default()
    }))
}

/// Whether a FROM list starts with the `*RULE*` item.
pub(crate) fn is_marked(from: &[Node]) -> bool {
    from.first().is_some_and(|n| {
        matches!(inner(n), Some(NodeEnum::RangeVar(rv)) if rv.relname == RULE_RTE && rv.schemaname.is_empty())
    })
}

/// Subqueries in FROM that read OLD / NEW are LATERAL references to `*RULE*`, as Postgres
/// takes them.
fn lateral_old_new(from: &mut [Node]) {
    for item in from {
        if let Some(NodeEnum::RangeSubselect(rs)) = item.node.as_mut()
            && !rs.lateral
            && let Some(q) = rs.subquery.as_deref()
            && let Ok(sql) = ast::deparse_stmt(q)
            && old_new_refs(&sql) != (false, false)
        {
            rs.lateral = true;
        }
    }
}

/// Put the `*RULE*` item in an action's FROM list. False when the action has no place for
/// it (a set operation, a utility statement). `last`: it stands for OLD alone (an UPDATE or
/// DELETE without FROM items of its own), which transformRuleStmt appends to the action's
/// FROM list; otherwise it is the statement's own FROM list too, which rewriteRuleAction puts
/// first (adjustJoinTreeList). (First anyway when an item of the action reads OLD / NEW: the
/// items to its left are all it may refer to.)
fn inject(n: &mut Node, last: bool) -> bool {
    fn put(from: &mut Vec<Node>, last: bool) {
        lateral_old_new(from);
        if last && !from.iter().any(item_reads_old_new) {
            from.push(marker());
        } else {
            from.insert(0, marker());
        }
    }
    match n.node.as_mut() {
        Some(NodeEnum::InsertStmt(s)) => match s.select_stmt.as_deref_mut() {
            None => {
                // DEFAULT VALUES: a row of no columns per source row.
                s.select_stmt = Some(Box::new(ast::mk(NodeEnum::SelectStmt(Box::new(
                    pb::SelectStmt {
                        from_clause: vec![marker()],
                        op: pb::SetOperation::SetopNone as i32,
                        limit_option: pb::LimitOption::Default as i32,
                        ..Default::default()
                    },
                )))));
                true
            }
            Some(sel) => inject(sel, last),
        },
        Some(NodeEnum::UpdateStmt(s)) => {
            put(&mut s.from_clause, last);
            true
        }
        Some(NodeEnum::DeleteStmt(s)) => {
            put(&mut s.using_clause, last);
            true
        }
        Some(NodeEnum::SelectStmt(s)) => {
            let op = pb::SetOperation::try_from(s.op).unwrap_or(pb::SetOperation::SetopNone);
            if !matches!(op, pb::SetOperation::SetopNone | pb::SetOperation::Undefined) {
                return false;
            }
            put(&mut s.from_clause, last);
            true
        }
        _ => false,
    }
}

/// Whether a FROM item refers to OLD / NEW (a LATERAL subquery, a function's arguments).
fn item_reads_old_new(item: &Node) -> bool {
    let sel = ast::mk(NodeEnum::SelectStmt(Box::new(pb::SelectStmt {
        target_list: vec![],
        from_clause: vec![item.clone()],
        op: pb::SetOperation::SetopNone as i32,
        limit_option: pb::LimitOption::Default as i32,
        ..Default::default()
    })));
    ast::deparse_stmt(&sel).map_or(true, |sql| old_new_refs(&sql) != (false, false))
}

/// `OLD.*` / `NEW.*` in an action's VALUES lists stand for the relation's columns
/// (`transformExpressionList`'s expansion).
pub(crate) fn expand_old_new_star(n: &mut Node, t: &Table) {
    match n.node.as_mut() {
        Some(NodeEnum::InsertStmt(s)) => {
            if let Some(sel) = s.select_stmt.as_deref_mut() {
                expand_old_new_star(sel, t);
            }
        }
        Some(NodeEnum::SelectStmt(s)) => {
            for l in &mut s.values_lists {
                let Some(NodeEnum::List(items)) = l.node.as_mut() else {
                    continue;
                };
                let mut out = Vec::new();
                for it in std::mem::take(&mut items.items) {
                    if let Some(NodeEnum::ColumnRef(c)) = inner(&it)
                        && let [q, star] = c.fields.as_slice()
                        && matches!(inner(star), Some(NodeEnum::AStar(_)))
                        && let Some(q) = ast::sval(q)
                        && (q == "old" || q == "new")
                    {
                        for col in t.columns.iter().filter(|c| !c.dropped) {
                            out.push(ast::mk(NodeEnum::ColumnRef(pb::ColumnRef {
                                fields: vec![
                                    ast::mk(NodeEnum::String(pb::String { sval: q.to_string() })),
                                    ast::mk(NodeEnum::String(pb::String {
                                        sval: col.name.clone(),
                                    })),
                                ],
                                location: c.location,
                            })));
                        }
                    } else {
                        out.push(it);
                    }
                }
                items.items = out;
            }
        }
        _ => {}
    }
}

/// A top-level VALUES action (`DO ALSO VALUES (...)`) as `SELECT * FROM *RULE*, LATERAL
/// (VALUES ...)`.
fn values_as_select(n: &Node) -> Option<Node> {
    let Some(NodeEnum::SelectStmt(s)) = inner(n) else {
        return None;
    };
    if s.values_lists.is_empty() {
        return None;
    }
    let star = ast::mk(NodeEnum::ResTarget(Box::new(pb::ResTarget {
        val: Some(Box::new(ast::mk(NodeEnum::ColumnRef(pb::ColumnRef {
            fields: vec![ast::mk(NodeEnum::AStar(pb::AStar {}))],
            location: -1,
        })))),
        location: -1,
        ..Default::default()
    })));
    let sub = ast::mk(NodeEnum::RangeSubselect(Box::new(pb::RangeSubselect {
        lateral: true,
        subquery: Some(Box::new(n.clone())),
        alias: Some(pb::Alias {
            aliasname: "*VALUES*".into(),
            colnames: vec![],
        }),
    })));
    Some(ast::mk(NodeEnum::SelectStmt(Box::new(pb::SelectStmt {
        target_list: vec![star],
        from_clause: vec![sub],
        op: pb::SetOperation::SetopNone as i32,
        limit_option: pb::LimitOption::Default as i32,
        ..Default::default()
    }))))
}

/// The tokens of SQL text that matter for name references: identifiers (folded, or as
/// quoted), and single punctuation characters; string literals and comments are skipped.
fn tokens(sql: &str) -> Vec<String> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'\'' {
            i += 1;
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                if b[i] == b'\\' && i > 0 {
                    i += 1;
                }
                i += 1;
            }
            i += 1;
            out.push("'".into());
        } else if c == b'"' {
            let mut s = String::new();
            i += 1;
            while i < b.len() {
                if b[i] == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        s.push('"');
                        i += 2;
                        continue;
                    }
                    break;
                }
                s.push(b[i] as char);
                i += 1;
            }
            i += 1;
            out.push(format!("\"{s}"));
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'$'
            && let Some(end) = sql[i + 1..].find('$')
            && sql[i + 1..i + 1 + end]
                .chars()
                .all(|ch| ch.is_alphanumeric() || ch == '_')
            && !sql[i + 1..i + 1 + end].starts_with(|ch: char| ch.is_ascii_digit())
        {
            let tag = &sql[i..i + end + 2];
            match sql[i + tag.len()..].find(tag) {
                Some(close) => i += tag.len() + close + tag.len(),
                None => i = b.len(),
            }
            out.push("'".into());
        } else if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            out.push(sql[start..i].to_lowercase());
        } else if c.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                i += 1;
            }
            out.push("0".into());
        } else {
            out.push((c as char).to_string());
            i += 1;
        }
    }
    out
}

/// The name a token stands for (a quoted identifier's text, a folded one).
fn ident(t: &str) -> Option<&str> {
    if let Some(q) = t.strip_prefix('"') {
        return Some(q);
    }
    t.starts_with(|c: char| c.is_alphabetic() || c == '_').then_some(t)
}

/// Whether SQL text refers to OLD / NEW (`old.x`, `old.*`).
pub(crate) fn old_new_refs(sql: &str) -> (bool, bool) {
    let t = tokens(sql);
    let (mut old, mut new) = (false, false);
    for (i, w) in t.iter().enumerate() {
        if i > 0 && t[i - 1] == "." {
            continue;
        }
        if t.get(i + 1).map(String::as_str) != Some(".") {
            continue;
        }
        match ident(w) {
            Some("old") => old = true,
            Some("new") => new = true,
            _ => {}
        }
    }
    (old, new)
}

/// Whether a WHERE condition (as SQL) refers to the target relation: by its reference name,
/// or by an unqualified name of one of its columns.
fn refers_to(sql: &str, refname: &str, t: &Table) -> bool {
    let tk = tokens(sql);
    for (i, w) in tk.iter().enumerate() {
        let Some(name) = ident(w) else { continue };
        let after_dot = i > 0 && tk[i - 1] == ".";
        let before_dot = tk.get(i + 1).map(String::as_str) == Some(".");
        if before_dot && !after_dot && name == refname {
            return true;
        }
        if !after_dot
            && !before_dot
            && tk.get(i + 1).map(String::as_str) != Some("(")
            && t.column_index(name).is_some()
        {
            return true;
        }
    }
    false
}

/// A statement's target, event and pieces the rewrite needs.
struct Target<'n> {
    rv: &'n pb::RangeVar,
    /// pg_rewrite.ev_type of its command.
    event: char,
    cmd: char,
    returning: &'n [Node],
    with: Option<&'n pb::WithClause>,
}

fn target(n: &Node) -> Option<Target<'_>> {
    Some(match inner(n)? {
        NodeEnum::InsertStmt(s) => Target {
            rv: s.relation.as_ref()?,
            event: '3',
            cmd: 'i',
            returning: ast::returning(&s.returning_clause),
            with: s.with_clause.as_ref(),
        },
        NodeEnum::UpdateStmt(s) => Target {
            rv: s.relation.as_ref()?,
            event: '2',
            cmd: 'u',
            returning: ast::returning(&s.returning_clause),
            with: s.with_clause.as_ref(),
        },
        NodeEnum::DeleteStmt(s) => Target {
            rv: s.relation.as_ref()?,
            event: '4',
            cmd: 'd',
            returning: ast::returning(&s.returning_clause),
            with: s.with_clause.as_ref(),
        },
        _ => return None,
    })
}

/// A statement's command letter (as `Piece::cmd`).
fn command(n: &Node) -> char {
    match inner(n) {
        Some(NodeEnum::InsertStmt(_)) => 'i',
        Some(NodeEnum::UpdateStmt(_)) => 'u',
        Some(NodeEnum::DeleteStmt(_)) => 'd',
        Some(NodeEnum::NotifyStmt(_)) => 'n',
        _ => 's',
    }
}

/// Remove a DML statement's RETURNING (the triggering statement doesn't ask for one).
fn strip_returning(n: &mut Node) {
    match n.node.as_mut() {
        Some(NodeEnum::InsertStmt(s)) => s.returning_clause = None,
        Some(NodeEnum::UpdateStmt(s)) => s.returning_clause = None,
        Some(NodeEnum::DeleteStmt(s)) => s.returning_clause = None,
        _ => {}
    }
}

fn has_returning(n: &Node) -> bool {
    target(n).is_some_and(|t| !t.returning.is_empty())
}

/// Whether a RETURNING list reads OLD or NEW (as `old.x`, `new.*` or a WITH alias).
fn returning_reads_old_new(n: &Node) -> bool {
    let rc = match inner(n) {
        Some(NodeEnum::InsertStmt(s)) => s.returning_clause.as_ref(),
        Some(NodeEnum::UpdateStmt(s)) => s.returning_clause.as_ref(),
        Some(NodeEnum::DeleteStmt(s)) => s.returning_clause.as_ref(),
        _ => None,
    };
    let Some(rc) = rc else { return false };
    if !rc.options.is_empty() {
        return true;
    }
    let mut found = false;
    for e in &rc.exprs {
        let mut e = e.clone();
        let hit = std::cell::Cell::new(false);
        map_colrefs(&mut e, &|fields| {
            if fields.len() == 2
                && let Some(NodeEnum::String(s)) = fields[0].node.as_ref()
                && (s.sval == "old" || s.sval == "new")
            {
                hit.set(true);
            }
        });
        found |= hit.get();
    }
    found
}

/// A rule action's RETURNING list followed by itself over the action's OLD row and then
/// its NEW row (references to the written relation requalified), so the statement's
/// RETURNING old / new can read them. False when the action has no relation we know.
fn returning_with_old_new(
    cat: &crate::catalog::Catalog,
    path: &[String],
    n: &mut Node,
) -> bool {
    let (rv, rc) = match n.node.as_mut() {
        Some(NodeEnum::InsertStmt(s)) => (s.relation.as_ref(), s.returning_clause.as_mut()),
        Some(NodeEnum::UpdateStmt(s)) => (s.relation.as_ref(), s.returning_clause.as_mut()),
        Some(NodeEnum::DeleteStmt(s)) => (s.relation.as_ref(), s.returning_clause.as_mut()),
        _ => (None, None),
    };
    let (Some(rv), Some(rc)) = (rv, rc) else {
        return false;
    };
    let Some((_, table, ..)) = rule_relation(cat, rv, path) else {
        return false;
    };
    if !rc.options.is_empty() {
        return false;
    }
    let refname = rv
        .alias
        .as_ref()
        .map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone());
    let names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    let base = rc.exprs.clone();
    for (q, kind) in [
        (RULE_OLD, pb::ReturningOptionKind::ReturningOptionOld),
        (RULE_NEW, pb::ReturningOptionKind::ReturningOptionNew),
    ] {
        rc.options.push(ast::mk(NodeEnum::ReturningOption(pb::ReturningOption {
            option: kind as i32,
            value: q.to_string(),
            location: -1,
        })));
        for e in &base {
            let mut e = e.clone();
            if let Some(NodeEnum::ResTarget(rt)) = e.node.as_mut()
                && let Some(val) = rt.val.as_deref_mut()
            {
                map_colrefs(val, &|fields| {
                    let qual = |f: &mut Vec<Node>| {
                        f.insert(0, ast::mk(NodeEnum::String(pb::String { sval: q.to_string() })));
                    };
                    match fields.len() {
                        1 => match fields[0].node.as_ref() {
                            Some(NodeEnum::AStar(_)) => qual(fields),
                            Some(NodeEnum::String(s)) if names.contains(&s.sval) => qual(fields),
                            _ => {}
                        },
                        2 => {
                            if let Some(NodeEnum::String(s)) = fields[0].node.as_mut()
                                && s.sval == refname
                            {
                                s.sval = q.to_string();
                            }
                        }
                        _ => {}
                    }
                });
            }
            rc.exprs.push(e);
        }
    }
    true
}

/// The names a rule action's RETURNING reads its own OLD and NEW rows by.
const RULE_OLD: &str = "*OLD ROW*";
const RULE_NEW: &str = "*NEW ROW*";

/// The relation a statement writes, for its rules: (oid, its row shape, owner, is a view).
pub(crate) fn rule_relation(
    cat: &crate::catalog::Catalog,
    rv: &pb::RangeVar,
    path: &[String],
) -> Option<(u32, Arc<Table>, u32, bool)> {
    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
    match cat.find(schema, &rv.relname, path)? {
        Relation::Table(t) => Some((t.oid, t.clone(), t.owner, false)),
        Relation::View(v) if !v.materialized => Some((v.oid, v.as_table(), v.owner, true)),
        _ => None,
    }
}

/// The rules of relation `oid` (for `event`, or any non-SELECT one) that fire under
/// session_replication_role, in name order.
pub(crate) fn firing_rules(
    cat: &crate::catalog::Catalog,
    ctx: &crate::exec::ExecCtx,
    oid: u32,
    event: Option<char>,
) -> Vec<Arc<Rule>> {
    let replica = ctx
        .sess
        .settings
        .borrow()
        .get_raw("session_replication_role")
        .as_deref()
        == Some("replica");
    let mut out: Vec<Arc<Rule>> = cat
        .rules
        .values()
        .filter(|r| r.table == oid && event.is_none_or(|e| r.event == e))
        .filter(|r| match r.enabled {
            'D' => false,
            'A' => true,
            'R' => replica,
            _ => !replica,
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    out
}

impl Binder<'_, '_> {
    /// `bind_statement` of an INSERT / UPDATE / DELETE (or MERGE) on a relation with rules:
    /// the statements the rule system makes of it, run in order as one plan. None: no rules
    /// apply.
    pub(crate) fn bind_rewritten(&mut self, n: &Node) -> Result<Option<(Plan, Vec<OutCol>)>> {
        let Some(pieces) = self.rewrite(n, QSrc::Original)? else {
            return Ok(None);
        };
        Ok(Some(self.assemble(command(n), pieces)))
    }

    /// One plan running the pieces in order. The piece that sets the command tag (the
    /// original statement, else the last INSTEAD one of its kind) counts its rows; a SELECT's
    /// rows are returned; the others run with their rows and counts dropped.
    fn assemble(&mut self, cmd: char, pieces: Vec<Piece>) -> (Plan, Vec<OutCol>) {
        let setter = pieces
            .iter()
            .position(|p| p.src == QSrc::Original)
            .or_else(|| {
                pieces.iter().rposition(|p| {
                    p.cmd == cmd && matches!(p.src, QSrc::Instead | QSrc::QualInstead)
                })
            });
        let mut inputs = Vec::new();
        let mut cols: Option<Vec<OutCol>> = None;
        for (i, p) in pieces.into_iter().enumerate() {
            if Some(i) == setter || p.cmd == 's' {
                if !p.cols.is_empty() && cols.is_none() {
                    cols = Some(p.cols.clone());
                }
                inputs.push(Arc::new(p.plan));
            } else if p.cmd != 'n' && !p.cols.is_empty() {
                // The statement's RETURNING, computed by an INSTEAD action of another command
                // (a DELETE done as an UPDATE): its rows, without its count.
                if cols.is_none() {
                    cols = Some(p.cols.clone());
                }
                let (id, ncols) = (self.fresh_id(), p.cols.len());
                inputs.push(Arc::new(Plan::With {
                    ctes: vec![(id, Arc::new(p.plan))],
                    body: Arc::new(Plan::CteScan {
                        id,
                        ncols,
                        name: RULE_RTE.into(),
                        alias: RULE_RTE.into(),
                        cols: vec![String::new(); ncols],
                        rti: Default::default(),
                    }),
                    names: vec![RULE_RTE.into()],
                }));
            } else {
                inputs.push(Arc::new(self.discard(p.plan, p.cols.len())));
            }
        }
        let plan = match inputs.len() {
            0 => Plan::Empty { ncols: 0, cols: None, info: None },
            1 => Arc::try_unwrap(inputs.pop().unwrap()).unwrap_or_else(|a| (*a).clone()),
            _ => Plan::Append { inputs, inh: None, alias: None, pw: None },
        };
        (plan, cols.unwrap_or_default())
    }

    /// Run `plan` to completion, dropping its rows and its count of rows processed.
    fn discard(&mut self, plan: Plan, ncols: usize) -> Plan {
        let id = self.fresh_id();
        Plan::With {
            ctes: vec![(id, Arc::new(plan))],
            body: Arc::new(Plan::Filter {
                input: Arc::new(Plan::CteScan {
                    id,
                    ncols,
                    name: RULE_RTE.into(),
                    alias: RULE_RTE.into(),
                    cols: vec![String::new(); ncols],
                    rti: Default::default(),
                }),
                pred: Expr::bool(false),
            }),
            names: vec![RULE_RTE.into()],
        }
    }

    /// A data-modifying statement in WITH on a relation with rules: only an unconditional
    /// single-statement INSTEAD rule can stand in for it.
    pub(crate) fn cte_rewritten(&mut self, n: &Node) -> Result<Option<(Plan, Vec<OutCol>)>> {
        self.single_rewritten(n, "data-modifying statements in WITH", "DO INSTEAD NOTIFY rules are not supported for data-modifying statements in WITH")
    }

    /// COPY (INSERT / UPDATE / DELETE ...) TO on a relation with rules: the same.
    pub(crate) fn copy_rewritten(&mut self, n: &Node) -> Result<Option<(Plan, Vec<OutCol>)>> {
        self.single_rewritten(n, "COPY", "COPY query must not be a utility command")
    }

    fn single_rewritten(
        &mut self,
        n: &Node,
        what: &str,
        utility: &str,
    ) -> Result<Option<(Plan, Vec<OutCol>)>> {
        let Some(mut pieces) = self.rewrite(n, QSrc::Original)? else {
            return Ok(None);
        };
        let unsupported = |m: String| Err(Error::new(code::FEATURE_NOT_SUPPORTED, m));
        match pieces.len() {
            0 => unsupported(format!("DO INSTEAD NOTHING rules are not supported for {what}")),
            1 => {
                let p = pieces.pop().unwrap();
                if p.cmd == 'n' {
                    return unsupported(utility.to_string());
                }
                Ok(Some((p.plan, p.cols)))
            }
            _ => {
                for p in &pieces {
                    if p.src == QSrc::QualInstead {
                        return unsupported(format!(
                            "conditional DO INSTEAD rules are not supported for {what}"
                        ));
                    }
                    if p.src == QSrc::Also {
                        return unsupported(format!("DO ALSO rules are not supported for {what}"));
                    }
                }
                unsupported(format!(
                    "multi-statement DO INSTEAD rules are not supported for {what}"
                ))
            }
        }
    }

    /// The statements `n` becomes under its relation's rules (None: no rules apply). The
    /// statement itself, if it stays, is labelled `src`.
    pub(crate) fn rewrite(&mut self, n: &Node, src: QSrc) -> Result<Option<Vec<Piece>>> {
        if self.cat.rules.is_empty() || self.deparse || self.rule_validate {
            return Ok(None);
        }
        if let Some(NodeEnum::MergeStmt(s)) = inner(n) {
            if let Some(rv) = s.relation.as_ref()
                && let Some((oid, ..)) = rule_relation(&self.cat, rv, &self.ctx.search_path)
            {
                // The relation, or the view an updatable view is written through, with rules.
                let mut with_rules = None;
                if !firing_rules(&self.cat, self.ctx, oid, None).is_empty() {
                    with_rules = Some(rv.relname.clone());
                } else if let Some(v) = self.cat.views.get(&oid).cloned()
                    && !v.materialized
                    && let Ok(vb) = self.view_update_layer(&v, false, false, true)?
                    && let Some(iv) = self.cat.views.get(&vb.base.oid)
                    && !firing_rules(&self.cat, self.ctx, iv.oid, None).is_empty()
                {
                    with_rules = Some(iv.name.clone());
                }
                if let Some(name) = with_rules {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!("cannot execute MERGE on relation \"{name}\""),
                    )
                    .detail("MERGE is not supported for relations with rules."));
                }
            }
            return Ok(None);
        }
        let Some(tg) = target(n) else {
            return Ok(None);
        };
        let Some((oid, table, owner, is_view)) =
            rule_relation(&self.cat, tg.rv, &self.ctx.search_path)
        else {
            return Ok(None);
        };
        let rules = firing_rules(&self.cat, self.ctx, oid, Some(tg.event));
        if rules.is_empty() {
            // An updatable view over a view with rules: the statement on that view
            // (rewriteTargetView), whose rules then apply.
            if let Some(n2) = self.through_view(n, &tg, oid)? {
                use crate::catalog::acl;
                let privs = match tg.event {
                    '3' => acl::INSERT,
                    '2' => acl::UPDATE,
                    _ => acl::DELETE,
                };
                self.require_rel(oid, privs);
                return self.rewrite(&n2, src);
            }
            return Ok(None);
        }
        let products = rules.iter().any(|r| !r.actions.is_empty());
        // (RewriteQuery: an INSERT ... SELECT action can't take a query's data-modifying
        // WITH along.)
        let modifying_with = tg.with.is_some_and(|w| {
            w.ctes.iter().any(|c| {
                matches!(inner(c), Some(NodeEnum::CommonTableExpr(c))
                    if c.ctequery.as_deref().and_then(inner).is_some_and(|q| !matches!(q, NodeEnum::SelectStmt(_))))
            })
        });
        if modifying_with
            && rules.iter().flat_map(|r| &r.actions).any(|a| {
                matches!(
                    ast::parse_stmt(a).ok().and_then(|n| n.node),
                    Some(NodeEnum::InsertStmt(i))
                        if i.select_stmt.as_deref().and_then(inner).is_some_and(|q| {
                            matches!(q, NodeEnum::SelectStmt(sel) if sel.values_lists.is_empty())
                        })
                )
            })
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "INSERT ... SELECT rule actions are not supported for queries having data-modifying statements in WITH",
            ));
        }
        if let Some(NodeEnum::InsertStmt(s)) = inner(n)
            && s.on_conflict_clause.is_some()
            && (products
                || self
                    .cat
                    .rules
                    .values()
                    .any(|r| r.table == oid && r.event == '2'))
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "INSERT with ON CONFLICT clause cannot be used with table that has INSERT or UPDATE rules",
            ));
        }
        if products && self.rule_events.contains(&(oid, tg.event)) {
            return Err(Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!(
                    "infinite recursion detected in rules for relation \"{}\"",
                    table.name
                ),
            ));
        }
        let refname = tg
            .rv
            .alias
            .as_ref()
            .map_or_else(|| tg.rv.relname.clone(), |a| a.aliasname.clone());
        // The statement's rows, as the actions see them.
        let mut full: Option<RuleSource> = None;
        let (w_refs, has_from) = match inner(n) {
            Some(NodeEnum::UpdateStmt(s)) => (
                s.where_clause.as_deref().is_some_and(|w| {
                    ast::deparse_expr(w).is_ok_and(|sql| refers_to(&sql, &refname, &table))
                }),
                !s.from_clause.is_empty(),
            ),
            Some(NodeEnum::DeleteStmt(s)) => (
                s.where_clause.as_deref().is_some_and(|w| {
                    ast::deparse_expr(w).is_ok_and(|sql| refers_to(&sql, &refname, &table))
                }),
                !s.using_clause.is_empty(),
            ),
            _ => (true, true),
        };
        // An INSERT's source is a relation of its own unless it's one VALUES row.
        let insert_rte = match inner(n) {
            Some(NodeEnum::InsertStmt(s)) => s.select_stmt.as_deref().is_some_and(|sel| {
                !matches!(inner(sel), Some(NodeEnum::SelectStmt(x)) if x.values_lists.len() == 1)
            }),
            _ => false,
        };
        let mut pieces: Vec<Piece> = Vec::new();
        let mut instead = false;
        let mut quals: Vec<Arc<Rule>> = Vec::new();
        let mut returning = false;
        self.rule_events.push((oid, tg.event));
        let r = (|| -> Result<()> {
            for r in &rules {
                let qsrc = match (r.instead, &r.qual) {
                    (true, Some(_)) => QSrc::QualInstead,
                    (true, None) => {
                        instead = true;
                        QSrc::Instead
                    }
                    (false, _) => QSrc::Also,
                };
                if qsrc == QSrc::QualInstead {
                    quals.push(r.clone());
                }
                for a in &r.actions {
                    let mut node = ast::parse_stmt(a)?;
                    expand_old_new_star(&mut node, &table);
                    let (uses_old, uses_new) = old_new_refs(a);
                    let needed = match tg.event {
                        '3' => uses_new || insert_rte || r.qual.is_some(),
                        e => {
                            uses_old
                                || (uses_new && e == '2')
                                || r.qual.is_some()
                                || w_refs
                                || has_from
                        }
                    };
                    // RETURNING: the statement's, computed from the action's.
                    let map_ret = if tg.returning.is_empty() {
                        strip_returning(&mut node);
                        false
                    } else if has_returning(&node) {
                        if returning {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                "cannot have RETURNING lists in multiple rules",
                            ));
                        }
                        returning = true;
                        true
                    } else {
                        false
                    };
                    // The statement's RETURNING old / new: the action's over its own rows.
                    let split = map_ret
                        && returning_reads_old_new(n)
                        && returning_with_old_new(&self.cat, &self.ctx.search_path, &mut node);
                    if let Some(v) = values_as_select(&node) {
                        node = v;
                    }
                    let injected = inject(&mut node, tg.event != '3' && !has_from);
                    if !injected && needed && command(&node) != 'n' {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            "conditional UNION/INTERSECT/EXCEPT statements are not implemented",
                        ));
                    }
                    let source = if tg.event == '3' || needed {
                        if full.is_none() {
                            full = Some(self.statement_rows(n, &table, oid, true)?);
                        }
                        full.clone().unwrap()
                    } else {
                        self.statement_rows(n, &table, oid, false)?
                    };
                    let source = match &r.qual {
                        Some(q) => self.rule_qual_rows(source, q)?,
                        None => source,
                    };
                    let mut got = self.bind_rule_action(&node, source, owner, qsrc)?;
                    if map_ret
                        && let Some(p) = got.iter_mut().rev().find(|p| p.cmd != 's' && p.cmd != 'n')
                    {
                        let rows = split.then_some(p.cmd);
                        self.returning_from_rule(p, tg.returning, &refname, &table, tg.event, rows)?;
                    }
                    pieces.extend(got);
                }
            }
            Ok(())
        })();
        self.rule_events.pop();
        r?;
        // (rewriteTargetView's refusal comes before RETURNING's.)
        if !instead
            && is_view
            && !quals.is_empty()
            && !self.view_has_instead_trigger(oid, tg.event)
        {
            let (msg, verb_ing) = match tg.event {
                '3' => (format!("cannot insert into view \"{}\"", table.name), "inserting into"),
                '2' => (format!("cannot update view \"{}\"", table.name), "updating"),
                _ => (format!("cannot delete from view \"{}\"", table.name), "deleting from"),
            };
            let upper = match tg.event {
                '3' => "INSERT",
                '2' => "UPDATE",
                _ => "DELETE",
            };
            return Err(Error::new(code::OBJECT_NOT_IN_PREREQUISITE_STATE, msg)
                .detail("Views with conditional DO INSTEAD rules are not automatically updatable.")
                .hint(format!("To enable {verb_ing} the view, provide an INSTEAD OF {upper} trigger or an unconditional ON {upper} DO INSTEAD rule.")));
        }
        if (instead || !quals.is_empty()) && !tg.returning.is_empty() && !returning {
            let (verb, upper) = match tg.event {
                '3' => ("INSERT", "INSERT"),
                '2' => ("UPDATE", "UPDATE"),
                _ => ("DELETE", "DELETE"),
            };
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!(
                    "cannot perform {verb} RETURNING on relation \"{}\"",
                    table.name
                ),
            )
            .hint(format!(
                "You need an unconditional ON {upper} DO INSTEAD rule with a RETURNING clause."
            )));
        }
        if !instead {
            let (mut plan, cols) = self.bind_plain(n)?;
            for r in &quals {
                if let Some(q) = &r.qual {
                    self.add_inverted_qual(&mut plan, q, &table)?;
                }
            }
            let p = Piece {
                plan,
                cols,
                src,
                cmd: tg.cmd,
            };
            if tg.event == '3' {
                pieces.insert(0, p);
            } else {
                pieces.push(p);
            }
        }
        if tg.with.is_some() && pieces.iter().filter(|p| p.cmd != 'n').count() > 1 {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "WITH cannot be used in a query that is rewritten by rules into multiple queries",
            ));
        }
        Ok(Some(pieces))
    }

    /// `rewriteTargetView` for an automatically updatable view `oid` whose relation is a
    /// view with rules for the event: the statement written against that view (the view's
    /// WHERE added, its columns renamed to the ones under them). None otherwise.
    fn through_view(&mut self, n: &Node, tg: &Target, oid: u32) -> Result<Option<Node>> {
        let Some(v) = self.cat.views.get(&oid).cloned() else {
            return Ok(None);
        };
        if v.materialized || self.view_has_instead_trigger(oid, tg.event) {
            return Ok(None);
        }
        let Ok(vb) = self.view_update_layer(&v, false, false, true)? else {
            return Ok(None);
        };
        // The relation under it: a view, or a table, with rules of its own.
        let (iv_oid, iv_schema, iv_name, iv_cols, iv_live): (
            u32,
            u32,
            String,
            Vec<String>,
            Vec<String>,
        ) = match self.cat.views.get(&vb.base.oid) {
            Some(iv) => {
                let cols: Vec<String> = iv.columns.iter().map(|c| c.0.clone()).collect();
                (iv.oid, iv.schema, iv.name.clone(), cols.clone(), cols)
            }
            None => match self.cat.tables.get(&vb.base.oid) {
                Some(t) => (
                    t.oid,
                    t.schema,
                    t.name.clone(),
                    t.columns.iter().map(|c| c.name.clone()).collect(),
                    t.columns
                        .iter()
                        .filter(|c| !c.dropped)
                        .map(|c| c.name.clone())
                        .collect(),
                ),
                None => return Ok(None),
            },
        };
        if firing_rules(&self.cat, self.ctx, iv_oid, Some(tg.event)).is_empty() {
            return Ok(None);
        }
        let Some(NodeEnum::SelectStmt(q)) = ast::parse_stmt(&v.query)?.node else {
            return Ok(None);
        };
        let Some(NodeEnum::RangeVar(from)) = q.from_clause.first().and_then(inner) else {
            return Ok(None);
        };
        let iv_ref = from
            .alias
            .as_ref()
            .map_or_else(|| from.relname.clone(), |a| a.aliasname.clone());
        let refname = tg
            .rv
            .alias
            .as_ref()
            .map_or_else(|| tg.rv.relname.clone(), |a| a.aliasname.clone());
        let map = &vb.layers[0].1;
        let under = |name: &str| -> Option<String> {
            let k = v.columns.iter().position(|c| c.0 == name)?;
            let i = map.get(k).copied().flatten()?;
            iv_cols.get(i).cloned()
        };
        let target = pb::RangeVar {
            schemaname: self.cat.schema_name(iv_schema).to_string(),
            relname: iv_name.clone(),
            inh: true,
            relpersistence: "p".into(),
            alias: from.alias.clone(),
            location: -1,
            ..Default::default()
        };
        // The view's WHERE, over the relation under it as the view names it.
        let vwhere = q.where_clause.as_deref().cloned();
        let and = |a: Option<Box<Node>>, b: Option<Node>| -> Option<Box<Node>> {
            match (a, b) {
                (Some(a), Some(b)) => Some(Box::new(ast::mk(NodeEnum::BoolExpr(Box::new(
                    pb::BoolExpr {
                        boolop: pb::BoolExprType::AndExpr as i32,
                        args: vec![*a, b],
                        location: -1,
                        ..Default::default()
                    },
                ))))),
                (a, None) => a,
                (None, Some(b)) => Some(Box::new(b)),
            }
        };
        // The statement's references to the view's columns, as the ones under them.
        let renames: Vec<(String, String)> = v
            .columns
            .iter()
            .filter_map(|c| under(&c.0).map(|u| (c.0.clone(), u)))
            .collect();
        // References qualified by the view's name now qualified by the relation's; OLD and
        // NEW in RETURNING keep theirs.
        let rename = |n: &mut Node| {
            map_colrefs(n, &|fields| {
                let k = fields.len().wrapping_sub(1);
                let ours = match fields.len() {
                    1 => true,
                    2 => match fields[0].node.as_mut() {
                        Some(NodeEnum::String(s)) if s.sval == refname => {
                            s.sval = iv_ref.clone();
                            true
                        }
                        Some(NodeEnum::String(s)) => {
                            s.sval.eq_ignore_ascii_case("old") || s.sval.eq_ignore_ascii_case("new")
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if ours
                    && let Some(NodeEnum::String(s)) = fields[k].node.as_mut()
                    && let Some((_, u)) = renames.iter().find(|(c, _)| *c == s.sval)
                {
                    s.sval = u.clone();
                }
            })
        };
        // RETURNING only when the view's columns are those under it, in order (a `*` there
        // expands to the relation's).
        let same_cols = v.columns.len() == iv_live.len()
            && renames.len() == v.columns.len()
            && renames.iter().zip(&iv_live).all(|((c, u), ic)| c == u && u == ic);
        if !tg.returning.is_empty() && !same_cols {
            return Ok(None);
        }
        let rename_returning = |list: &mut Vec<Node>| {
            for t in list {
                if let Some(NodeEnum::ResTarget(rt)) = t.node.as_mut()
                    && let Some(val) = rt.val.as_deref_mut()
                {
                    rename(val);
                }
            }
        };
        let mut node = n.clone();
        match node.node.as_mut() {
            Some(NodeEnum::InsertStmt(s)) => {
                let names: Vec<String> = if s.cols.is_empty() {
                    let width = match s.select_stmt.as_deref().and_then(inner) {
                        Some(NodeEnum::SelectStmt(sel)) => match sel.values_lists.first().and_then(inner) {
                            Some(NodeEnum::List(l)) => l.items.len(),
                            _ => sel.target_list.len(),
                        },
                        _ => 0,
                    };
                    v.columns.iter().take(width).map(|c| c.0.clone()).collect()
                } else {
                    s.cols
                        .iter()
                        .filter_map(|c| match inner(c) {
                            Some(NodeEnum::ResTarget(rt)) => Some(rt.name.clone()),
                            _ => None,
                        })
                        .collect()
                };
                let mut cols = Vec::new();
                for name in &names {
                    let Some(u) = under(name) else {
                        return Ok(None);
                    };
                    cols.push(ast::mk(NodeEnum::ResTarget(Box::new(pb::ResTarget {
                        name: u,
                        location: -1,
                        ..Default::default()
                    }))));
                }
                s.cols = cols;
                if let Some(r) = s.returning_clause.as_mut() {
                    rename_returning(&mut r.exprs);
                }
                s.relation = Some(target);
            }
            Some(NodeEnum::UpdateStmt(s)) => {
                for t in &mut s.target_list {
                    if let Some(NodeEnum::ResTarget(rt)) = t.node.as_mut() {
                        let Some(u) = under(&rt.name) else {
                            return Ok(None);
                        };
                        rt.name = u;
                        if let Some(val) = rt.val.as_deref_mut() {
                            rename(val);
                        }
                    }
                }
                if let Some(w) = s.where_clause.as_deref_mut() {
                    rename(w);
                }
                s.where_clause = and(s.where_clause.take(), vwhere);
                if let Some(r) = s.returning_clause.as_mut() {
                    rename_returning(&mut r.exprs);
                }
                s.relation = Some(target);
            }
            Some(NodeEnum::DeleteStmt(s)) => {
                if let Some(w) = s.where_clause.as_deref_mut() {
                    rename(w);
                }
                s.where_clause = and(s.where_clause.take(), vwhere);
                if let Some(r) = s.returning_clause.as_mut() {
                    rename_returning(&mut r.exprs);
                }
                s.relation = Some(target);
            }
            _ => return Ok(None),
        }
        Ok(Some(node))
    }

    fn view_has_instead_trigger(&self, oid: u32, event: char) -> bool {
        use crate::catalog::trigger_type as tt;
        let ev = match event {
            '3' => tt::INSERT,
            '2' => tt::UPDATE,
            _ => tt::DELETE,
        };
        self.cat.triggers.values().any(|x| {
            x.table == oid && x.tgtype & tt::INSTEAD != 0 && x.tgtype & ev != 0 && x.tgtype & tt::ROW != 0
        })
    }

    /// A statement bound as written (its relation's rules not applied again).
    fn bind_plain(&mut self, n: &Node) -> Result<(Plan, Vec<OutCol>)> {
        let (mut plan, cols) = match inner(n) {
            Some(NodeEnum::InsertStmt(s)) => self.bind_insert(s)?,
            Some(NodeEnum::UpdateStmt(s)) => self.bind_update(s)?,
            Some(NodeEnum::DeleteStmt(s)) => self.bind_delete(s)?,
            Some(NodeEnum::SelectStmt(s)) => return self.bind_select_stmt(s),
            Some(NodeEnum::NotifyStmt(s)) => return self.bind_notify(s),
            _ => return Err(crate::error::internal("not a rule action")),
        };
        if !self.deparse {
            self.hidden_rtes += crate::sql::paths::plan_query(self.ctx, &mut plan);
            if !self.unplanned {
                super::check_merge_keys(self.ctx, &mut plan)?;
            }
        }
        Ok((plan, cols))
    }

    /// NOTIFY as a rule action: `SELECT pg_notify(channel, payload)`.
    fn bind_notify(&mut self, s: &pb::NotifyStmt) -> Result<(Plan, Vec<OutCol>)> {
        let lit = |t: &str| format!("'{}'", t.replace('\'', "''"));
        let payload = if s.payload.is_empty() {
            "NULL".to_string()
        } else {
            lit(&s.payload)
        };
        let sql = format!(
            "SELECT pg_catalog.pg_notify({}, {payload})",
            lit(&s.conditionname)
        );
        let node = ast::parse_stmt(&sql)?;
        let Some(NodeEnum::SelectStmt(sel)) = inner(&node) else {
            return Err(crate::error::internal("NOTIFY"));
        };
        let (plan, _) = self.bind_select_stmt(sel)?;
        Ok((plan, Vec::new()))
    }

    /// Bind one rule action reading `source`, as the rule's owner, in a level of its own;
    /// the rules of the relation it writes apply in turn.
    fn bind_rule_action(
        &mut self,
        node: &Node,
        source: RuleSource,
        owner: u32,
        src: QSrc,
    ) -> Result<Vec<Piece>> {
        let saved_src = self.rule_src.replace(source);
        let saved_ctes = std::mem::take(&mut self.ctes);
        let saved_fns = std::mem::take(&mut self.fn_levels);
        let saved_as = self.check_as.replace(owner);
        let mut lvl = Level::new();
        lvl.barrier = true;
        self.levels.push(lvl);
        let r = (|| -> Result<Vec<Piece>> {
            if let Some(ps) = self.rewrite(node, src)? {
                return Ok(ps);
            }
            let (plan, cols) = self.bind_plain(node)?;
            Ok(vec![Piece {
                plan,
                cols,
                src,
                cmd: command(node),
            }])
        })();
        self.levels.pop();
        self.check_as = saved_as;
        self.fn_levels = saved_fns;
        self.ctes = saved_ctes;
        self.rule_src = saved_src;
        r.map_err(strip_position)
    }

    /// The statement's rows as its rules' actions read them. `full`: the target's rows
    /// (OLD, and for UPDATE, NEW); otherwise only what else limits the statement (its FROM
    /// items and WHERE), for actions that don't read the target.
    fn statement_rows(
        &mut self,
        n: &Node,
        table: &Arc<Table>,
        oid: u32,
        full: bool,
    ) -> Result<RuleSource> {
        let ncols = table.columns.len();
        match inner(n) {
            Some(NodeEnum::InsertStmt(s)) => {
                let mut s = (**s).clone();
                s.returning_clause = None;
                s.on_conflict_clause = None;
                let saved = self.rule_target.replace(oid);
                let r = self.bind_insert(&s);
                self.rule_target = saved;
                let (plan, _) = r?;
                let plan = new_rows(plan, table)?;
                Ok(RuleSource {
                    plan: Arc::new(plan),
                    table: table.clone(),
                    old: None,
                    new: Some(0),
                })
            }
            Some(NodeEnum::UpdateStmt(s)) => {
                let plan = self.modified_rows(
                    s.relation.as_ref().unwrap(),
                    &s.from_clause,
                    s.where_clause.as_deref(),
                    Some(&s.target_list),
                    s.with_clause.as_ref(),
                    table,
                    full,
                    crate::catalog::acl::UPDATE,
                )?;
                Ok(RuleSource {
                    plan: Arc::new(plan),
                    table: table.clone(),
                    old: full.then_some(0),
                    new: full.then_some(ncols),
                })
            }
            Some(NodeEnum::DeleteStmt(s)) => {
                let plan = self.modified_rows(
                    s.relation.as_ref().unwrap(),
                    &s.using_clause,
                    s.where_clause.as_deref(),
                    None,
                    s.with_clause.as_ref(),
                    table,
                    full,
                    crate::catalog::acl::DELETE,
                )?;
                Ok(RuleSource {
                    plan: Arc::new(plan),
                    table: table.clone(),
                    old: full.then_some(0),
                    new: None,
                })
            }
            _ => Err(crate::error::internal("not a rule event")),
        }
    }

    /// The rows an UPDATE / DELETE touches: OLD's columns, then (UPDATE) NEW's.
    #[allow(clippy::too_many_arguments)]
    fn modified_rows(
        &mut self,
        rv: &pb::RangeVar,
        from: &[Node],
        where_: Option<&Node>,
        sets: Option<&[Node]>,
        with: Option<&pb::WithClause>,
        table: &Arc<Table>,
        full: bool,
        privs: u32,
    ) -> Result<Plan> {
        let body = |b: &mut Self| -> Result<(Plan, Vec<OutCol>)> {
            let saved_scope = std::mem::take(&mut b.level().scope);
            let saved_st = std::mem::take(&mut b.level().st);
            let r = (|| -> Result<Plan> {
                let mut items = Vec::new();
                if full {
                    items.push(ast::mk(NodeEnum::RangeVar(rv.clone())));
                }
                items.extend(from.iter().cloned());
                let mut plan = b.bind_from(&items, None)?;
                if full {
                    // The target needs the statement's privilege, not SELECT (columns read
                    // need SELECT, as noted).
                    let as_user = b.check_as;
                    if let Some(p) = (b.perms.rels.iter_mut())
                        .find(|p| p.oid == table.oid && p.as_user == as_user)
                    {
                        p.privs = (p.privs & !crate::catalog::acl::SELECT) | privs;
                    }
                }
                if let Some(w) = where_ {
                    let pred = b.in_clause(Clause::Where, |b| b.bind_expr(w))?;
                    let pred = b.coerce_bool(pred, "WHERE")?;
                    plan = super::optimize::add_filter(&b.cat, plan, pred);
                }
                let mut exprs = Vec::new();
                if full {
                    let rel = b.scope().rels.first().cloned().unwrap_or_default();
                    let old: Vec<Expr> = table
                        .columns
                        .iter()
                        .map(|c| {
                            rel.cols
                                .iter()
                                .find(|x| !x.hidden && !c.dropped && x.name == c.name)
                                .map_or_else(|| Expr::null(c.ty.clone()), |x| x.expr.clone())
                        })
                        .collect();
                    exprs.extend(old.iter().cloned());
                    if let Some(list) = sets {
                        let sets = b.bind_set_list(list, table, None, 0)?;
                        for (i, o) in old.into_iter().enumerate() {
                            exprs.push(
                                sets.iter()
                                    .rev()
                                    .find(|(c, _)| *c == i)
                                    .map_or(o, |(_, e)| e.clone()),
                            );
                        }
                    }
                }
                Ok(super::optimize::project(plan, exprs))
            })();
            b.level().scope = saved_scope;
            b.level().st = saved_st;
            Ok((r?, Vec::new()))
        };
        let (plan, _) = match with {
            Some(w) => self.with_ctes(w, body)?,
            None => body(self)?,
        };
        Ok(plan)
    }

    /// `source` limited to the rows where the rule's WHERE condition holds.
    fn rule_qual_rows(&mut self, mut source: RuleSource, qual: &str) -> Result<RuleSource> {
        let n = ast::parse_expr(qual)?;
        let rels = self.rule_rels(&source, false);
        let pred = self.bind_over_rels(rels, &n)?;
        let plan = Arc::try_unwrap(std::mem::replace(&mut source.plan, Arc::new(Plan::One)))
            .unwrap_or_else(|a| (*a).clone());
        source.plan = Arc::new(super::optimize::add_filter(&self.cat, plan, pred));
        Ok(source)
    }

    /// A boolean condition over the given scope, in a level of its own.
    fn bind_over_rels(&mut self, rels: Vec<ScopeRel>, n: &Node) -> Result<Expr> {
        let mut lvl = Level::new();
        lvl.barrier = true;
        lvl.scope = Scope { rels };
        self.levels.push(lvl);
        let saved_ctes = std::mem::take(&mut self.ctes);
        let r = (|| {
            let e = self.in_clause(Clause::Where, |b| b.bind_expr(n))?;
            self.coerce_bool(e, "WHERE")
        })();
        self.ctes = saved_ctes;
        self.levels.pop();
        r
    }

    /// The statement kept beside qualified INSTEAD rules runs only for rows their
    /// condition isn't true for (`AddInvertedQual`).
    fn add_inverted_qual(&mut self, plan: &mut Plan, qual: &str, rel: &Arc<Table>) -> Result<()> {
        let n = ast::parse_expr(qual)?;
        match plan {
            Plan::With { body, .. } => {
                let mut b = (**body).clone();
                self.add_inverted_qual(&mut b, qual, rel)?;
                *body = Arc::new(b);
            }
            Plan::Append { inputs, .. } => {
                for i in inputs.iter_mut() {
                    let mut b = (**i).clone();
                    self.add_inverted_qual(&mut b, qual, rel)?;
                    *i = Arc::new(b);
                }
            }
            Plan::Insert(ip) => {
                let t = ip.rules.table.clone();
                // (One VALUES row of constants: its values are NEW's, put in for it
                // (ReplaceVarsFromTargetList), the qualification of no variable then.)
                let row: Option<Vec<Expr>> = match &*ip.source {
                    Plan::Values { rows, .. }
                        if rows.len() == 1
                            && rows[0].iter().all(|e| {
                                let mut plain = e.columns().is_empty() && !e.is_volatile() && !e.has_outer_refs();
                                e.walk(&mut |x| plain &= !matches!(x, Expr::Subquery(_) | Expr::Param(..)));
                                plain
                            }) =>
                    {
                        Some(rows[0].clone())
                    }
                    _ => None,
                };
                let new: Vec<Expr> = rel
                    .columns
                    .iter()
                    .map(|c| {
                        let j = t.column_index(&c.name);
                        match j.and_then(|j| ip.target.iter().position(|x| *x == j)) {
                            Some(p) if row.as_ref().is_some_and(|r| p < r.len()) => {
                                row.as_ref().expect("checked")[p].clone()
                            }
                            Some(p) => Expr::Col(p, c.ty.clone()),
                            None => j
                                .and_then(|j| ip.rules.defaults.get(j).cloned().flatten())
                                .unwrap_or_else(|| Expr::null(c.ty.clone())),
                        }
                    })
                    .collect();
                let pred = self.bind_over_rels(vec![expr_rel("new", rel, new)], &n)?;
                let src = (*ip.source).clone();
                ip.source = Arc::new(super::optimize::add_filter(
                    &self.cat,
                    src,
                    Expr::BoolTest(Box::new(pred), BoolTest::IsNotTrue),
                ));
            }
            Plan::Update(up) => {
                let t = up.rules.table.clone();
                let mut old = Vec::new();
                let mut new = Vec::new();
                for c in &rel.columns {
                    let j = t.column_index(&c.name);
                    let o = j.map_or_else(|| Expr::null(c.ty.clone()), |j| Expr::Col(j, c.ty.clone()));
                    new.push(
                        j.and_then(|j| up.sets.iter().rev().find(|(x, _)| *x == j))
                            .map_or_else(|| o.clone(), |(_, e)| e.clone()),
                    );
                    old.push(o);
                }
                // NEW's generated columns are computed from its other columns.
                let new_t: Vec<Expr> = t
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(j, c)| {
                        up.sets
                            .iter()
                            .rev()
                            .find(|(x, _)| *x == j)
                            .map_or_else(|| Expr::Col(j, c.ty.clone()), |(_, e)| e.clone())
                    })
                    .collect();
                for (k, c) in rel.columns.iter().enumerate() {
                    let Some(j) = t.column_index(&c.name) else { continue };
                    let tc = &t.columns[j];
                    let (Some(sql), false) = (&tc.generated, tc.dropped) else {
                        continue;
                    };
                    let Ok(g) = ast::parse_expr(sql) else { continue };
                    let Ok(e) = self.unrecorded(|b| b.bind_over_table(&t, &g, Clause::Generated)) else {
                        continue;
                    };
                    let Ok(e) = self.coerce_assign(e, &tc.ty, &tc.name) else {
                        continue;
                    };
                    if let Ok(e) = e.transform(&mut |x| {
                        Ok(match x {
                            Expr::Col(i, ty) => new_t.get(i).cloned().unwrap_or(Expr::Col(i, ty)),
                            x => x,
                        })
                    }) {
                        new[k] = e;
                    }
                }
                let pred = self.bind_over_rels(
                    vec![expr_rel("old", rel, old), expr_rel("new", rel, new)],
                    &n,
                )?;
                let src = (*up.source).clone();
                up.source = Arc::new(super::optimize::add_filter(
                    &self.cat,
                    src,
                    Expr::BoolTest(Box::new(pred), BoolTest::IsNotTrue),
                ));
            }
            Plan::Delete(dp) => {
                let t = dp.table.clone();
                let old: Vec<Expr> = rel
                    .columns
                    .iter()
                    .map(|c| {
                        t.column_index(&c.name)
                            .map_or_else(|| Expr::null(c.ty.clone()), |j| Expr::Col(j, c.ty.clone()))
                    })
                    .collect();
                let pred = self.bind_over_rels(vec![expr_rel("old", rel, old)], &n)?;
                let src = (*dp.source).clone();
                dp.source = Arc::new(super::optimize::add_filter(
                    &self.cat,
                    src,
                    Expr::BoolTest(Box::new(pred), BoolTest::IsNotTrue),
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// The triggering statement's RETURNING, computed from the rule action's (whose list
    /// has the relation's columns, in order).
    fn returning_from_rule(
        &mut self,
        p: &mut Piece,
        list: &[Node],
        refname: &str,
        table: &Arc<Table>,
        event: char,
        rows: Option<char>,
    ) -> Result<()> {
        // With its own OLD / NEW rows (see returning_with_old_new): three times as wide.
        if rows.is_some() {
            p.cols.truncate(p.cols.len() / 3);
        }
        let cols: Vec<OutCol> = p
            .cols
            .iter()
            .zip(table.columns.iter())
            .enumerate()
            .map(|(i, (oc, c))| OutCol {
                name: c.name.clone(),
                ty: oc.ty.clone(),
                table: table.oid,
                attnum: i as i16 + 1,
                domain_array: None,
                domain: c.domain,
                collation: None,
            })
            .collect();
        // OLD and NEW are the rule's row (or its OLD / NEW rows), NULLs where the command
        // has none: an INSERT no OLD, a DELETE no NEW.
        let n = cols.len();
        let input = std::mem::replace(&mut p.plan, Plan::One);
        let col = |i: usize, c: &OutCol| Expr::Col(i, c.ty.clone());
        let mut wide: Vec<Expr> = cols.iter().enumerate().map(|(i, c)| col(i, c)).collect();
        let (old_at, new_at) = match rows {
            Some(cmd) => {
                for (k, missing) in [(1, cmd == 'i'), (2, cmd == 'd')] {
                    wide.extend(cols.iter().enumerate().map(|(i, c)| {
                        if missing { Expr::null(c.ty.clone()) } else { col(k * n + i, c) }
                    }));
                }
                (n, 2 * n)
            }
            None => {
                wide.extend(cols.iter().map(|c| Expr::null(c.ty.clone())));
                match event {
                    '3' => (n, 0),
                    '4' => (0, n),
                    _ => (0, 0),
                }
            }
        };
        p.plan = super::optimize::project(input, wide);
        let mut old = derived_rel("old", &cols, old_at);
        old.qualified_only = true;
        let mut new = derived_rel("new", &cols, new_at);
        new.qualified_only = true;
        let mut lvl = Level::new();
        lvl.barrier = true;
        lvl.scope = Scope {
            rels: vec![derived_rel(refname, &cols, 0), old, new],
        };
        self.levels.push(lvl);
        let saved_ctes = std::mem::take(&mut self.ctes);
        let r = self.in_clause(Clause::Returning, |b| b.bind_targets(list));
        self.ctes = saved_ctes;
        self.levels.pop();
        let targets = r?;
        let mut exprs = Vec::new();
        let mut out = Vec::new();
        for t in targets {
            let e = if matches!(t.expr.ty(), DataType::Unknown) {
                self.coerce_implicit(t.expr, &DataType::Text)?
            } else {
                t.expr
            };
            out.push(OutCol {
                name: t.name,
                ty: e.ty(),
                table: t.table,
                attnum: t.attnum,
                domain_array: None,
                domain: None,
                collation: None,
            });
            exprs.push(e);
        }
        let plan = std::mem::replace(&mut p.plan, Plan::One);
        p.plan = super::optimize::project(plan, exprs);
        p.cols = out;
        Ok(())
    }

    /// OLD and NEW of a rule's rows (`RuleSource::rels`), NEW's generated columns computed
    /// from its other columns (the rewriter expands them there).
    fn rule_rels(&mut self, rs: &RuleSource, qualified_only: bool) -> Vec<ScopeRel> {
        let mut rels = rs.rels(qualified_only);
        let Some(at) = rs.new else {
            return rels;
        };
        let t = rs.table.clone();
        for r in rels.iter_mut().filter(|r| r.alias == "new") {
            for (i, c) in t.columns.iter().enumerate() {
                let (Some(sql), false) = (&c.generated, c.dropped) else {
                    continue;
                };
                let Ok(n) = ast::parse_expr(sql) else {
                    continue;
                };
                let Ok(e) = self.unrecorded(|b| b.bind_over_table(&t, &n, Clause::Generated)) else {
                    continue;
                };
                let Ok(e) = self.coerce_assign(e, &c.ty, &c.name) else {
                    continue;
                };
                if let Some(col) = r.cols.get_mut(i) {
                    col.expr = e.shift_cols(at as isize);
                }
            }
        }
        rels
    }

    /// The `*RULE*` FROM item of a rule action: its rows, and OLD / NEW over them.
    pub(crate) fn rule_from_item(&mut self) -> Option<(Plan, Scope)> {
        let rs = self.rule_src.clone()?;
        let rels = self.rule_rels(&rs, true);
        let mut plan = Arc::try_unwrap(rs.plan).unwrap_or_else(|a| (*a).clone());
        // (The statement's views and FROM subqueries are expanded (fireRIRrules) and pulled
        // up after rewriteRuleAction made the action's range table: their relations come
        // after the action's own entries.)
        let depth = self.levels.len();
        self.restamp_rti_below(&mut plan, depth);
        Some((plan, Scope { rels }))
    }

    /// An INSERT rule action's VALUES rows, evaluated for each of the triggering statement's
    /// rows (a VALUES list joined laterally to `*RULE*`). Returns the source plan and its
    /// columns.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rule_values(
        &mut self,
        sel: &pb::SelectStmt,
        target: &[usize],
        paths: &[(Vec<Node>, i32)],
        t: &Arc<Table>,
        rules: &crate::sql::plan::TableRules,
        over: (bool, bool),
        explicit_cols: Option<&[Node]>,
    ) -> Result<(Plan, Vec<usize>)> {
        let rs = self
            .rule_src
            .clone()
            .ok_or_else(|| crate::error::internal("rule action without its rows"))?;
        let rels = self.rule_rels(&rs, true);
        let mut rows: Vec<Vec<Expr>> = Vec::new();
        let mut merged: Vec<usize> = Vec::new();
        let mut width = None;
        for l in &sel.values_lists {
            let Some(NodeEnum::List(items)) = inner(l) else {
                continue;
            };
            // OLD.* / NEW.* expand to their columns.
            let mut list: Vec<Node> = Vec::new();
            for it in &items.items {
                if let Some(NodeEnum::ColumnRef(c)) = inner(it)
                    && let [q, star] = c.fields.as_slice()
                    && matches!(inner(star), Some(NodeEnum::AStar(_)))
                    && let Some(q) = ast::sval(q)
                    && (q == "old" && rs.old.is_some() || q == "new" && rs.new.is_some())
                {
                    for col in rs.table.columns.iter().filter(|c| !c.dropped) {
                        list.push(ast::mk(NodeEnum::ColumnRef(pb::ColumnRef {
                            fields: vec![
                                ast::mk(NodeEnum::String(pb::String { sval: q.to_string() })),
                                ast::mk(NodeEnum::String(pb::String {
                                    sval: col.name.clone(),
                                })),
                            ],
                            location: c.location,
                        })));
                    }
                } else {
                    list.push(it.clone());
                }
            }
            if list.len() > target.len() {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        "INSERT has more expressions than target columns",
                    ),
                    ast::location(&list[target.len()]),
                ));
            }
            if let Some(cols) = explicit_cols
                && list.len() < target.len()
            {
                return Err(with_pos(
                    Error::new(
                        code::SYNTAX_ERROR,
                        "INSERT has more target columns than expressions",
                    ),
                    ast::location(&cols[list.len()]),
                ));
            }
            if *width.get_or_insert(list.len()) != list.len() {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "VALUES lists must all be the same length",
                ));
            }
            let saved = std::mem::replace(
                &mut self.level().scope,
                Scope { rels: rels.clone() },
            );
            let row = (|| -> Result<Vec<Expr>> {
                let mut row = Vec::new();
                for item in &list {
                    row.push(self.in_clause(Clause::Values, |b| b.bind_expr(item))?);
                }
                Ok(row)
            })();
            self.level().scope = saved;
            let row = row?;
            let tgt = &target[..row.len().min(target.len())];
            let (row, cols) = self.insert_row(row, tgt, paths, t, rules, over.0, over.1)?;
            merged = cols;
            rows.push(row);
        }
        let plan = Arc::try_unwrap(rs.plan.clone()).unwrap_or_else(|a| (*a).clone());
        if rows.len() == 1 {
            return Ok((super::optimize::project(plan, rows.pop().unwrap()), merged));
        }
        // Several rows: each of them for each source row.
        let types = rs.types();
        let w = types.len();
        let ncols = merged.len();
        let outer = |e: Expr| {
            e.transform(&mut |x| {
                Ok(match x {
                    Expr::Col(i, t) => Expr::Outer(1, i, t),
                    x => x,
                })
            })
        };
        let rows = rows
            .into_iter()
            .map(|r| r.into_iter().map(outer).collect::<Result<Vec<_>>>())
            .collect::<Result<Vec<_>>>()?;
        let params = types
            .iter()
            .enumerate()
            .map(|(i, t)| Expr::Col(i, t.clone()))
            .collect();
        let join = Plan::Join {
            kind: JoinKind::Inner,
            left: Arc::new(plan),
            right: Arc::new(Plan::Values { rows, ncols, rti: Default::default() }),
            cond: None,
            hash: None,
            params,
            nleft: w,
            nright: ncols,
            meta: Default::default(),
        };
        let exprs = (0..ncols)
            .map(|j| Expr::Col(w + j, t.columns[merged[j]].ty.clone()))
            .collect();
        Ok((super::optimize::project(join, exprs), merged))
    }

    /// CREATE RULE's checks of the rule's WHERE condition and actions on `table` for
    /// `event` (parse analysis, `transformRuleStmt`, and `DefineQueryRewrite`'s RETURNING
    /// checks). Returns each action's text as stored.
    pub(crate) fn check_rule(
        &mut self,
        table: &Arc<Table>,
        event: char,
        qual: Option<&Node>,
        actions: &[Node],
        instead: bool,
    ) -> Result<Vec<String>> {
        let n = table.columns.len();
        let dummy = RuleSource {
            plan: Arc::new(Plan::Empty {
                ncols: 2 * n,
                cols: None,
                info: None,
            }),
            table: table.clone(),
            old: Some(0),
            new: Some(n),
        };
        let qual_sql = qual.map(ast::deparse_expr).transpose()?;
        if let Some(q) = qual {
            // OLD and NEW as the event has them, reachable unqualified too.
            let rels: Vec<ScopeRel> = dummy
                .rels(false)
                .into_iter()
                .filter(|r| match event {
                    '3' => r.alias == "new",
                    '4' | '1' => r.alias == "old",
                    _ => true,
                })
                .collect();
            self.bind_over_rels(rels, q)?;
        }
        let (q_old, q_new) = match (&qual_sql, event) {
            (Some(q), _) => {
                let (o, nw) = old_new_refs(q);
                // Unqualified names in the condition are NEW's (INSERT) or OLD's (DELETE).
                let bare = refers_to(q, "\u{0}", table);
                (o || bare && event == '4', nw || bare && event == '3')
            }
            (None, _) => (false, false),
        };
        let mut have_returning = false;
        let mut out = Vec::new();
        for a in actions {
            let text = ast::deparse_stmt(a)?;
            if matches!(inner(a), Some(NodeEnum::NotifyStmt(_))) {
                if qual.is_some() {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "rules with WHERE conditions can only have SELECT, INSERT, UPDATE, or DELETE actions",
                    ));
                }
                out.push(text);
                continue;
            }
            let (a_old, a_new) = old_new_refs(&text);
            let (has_old, has_new) = (a_old || q_old, a_new || q_new);
            let mut node = a.clone();
            expand_old_new_star(&mut node, table);
            if let Some(v) = values_as_select(&node) {
                node = v;
            }
            let setop = !inject(&mut node, false);
            if setop && (qual.is_some() || has_old || has_new && event == '2') {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "conditional UNION/INTERSECT/EXCEPT statements are not implemented",
                ));
            }
            match event {
                '3' if has_old => {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "ON INSERT rule cannot use OLD",
                    ));
                }
                '4' if has_new => {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        "ON DELETE rule cannot use NEW",
                    ));
                }
                _ => {}
            }
            if let Some(w) = with_clause(a) {
                let wsql = w
                    .ctes
                    .iter()
                    .filter_map(|c| match inner(c) {
                        Some(NodeEnum::CommonTableExpr(c)) => {
                            c.ctequery.as_deref().and_then(|q| ast::deparse_stmt(q).ok())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let (o, nw) = old_new_refs(&wsql);
                if o {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot refer to OLD within WITH query",
                    ));
                }
                if nw {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot refer to NEW within WITH query",
                    ));
                }
            }
            // Bind it over OLD / NEW.
            let saved_src = self.rule_src.replace(dummy.clone());
            let saved_ctes = std::mem::take(&mut self.ctes);
            let saved_v = std::mem::replace(&mut self.rule_validate, true);
            let mut lvl = Level::new();
            lvl.barrier = true;
            self.levels.push(lvl);
            let r = match inner(&node) {
                Some(NodeEnum::InsertStmt(s)) => self.bind_insert(s),
                Some(NodeEnum::UpdateStmt(s)) => self.bind_update(s),
                Some(NodeEnum::DeleteStmt(s)) => self.bind_delete(s),
                Some(NodeEnum::SelectStmt(s)) => self.bind_select_stmt(s),
                _ => Err(crate::error::unsupported("this rule action")),
            };
            self.levels.pop();
            self.rule_validate = saved_v;
            self.ctes = saved_ctes;
            self.rule_src = saved_src;
            let (_, cols) = r?;
            // RETURNING: one list at most, in an unconditional INSTEAD rule, matching the
            // relation's columns.
            if has_returning(a) {
                if have_returning {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "cannot have multiple RETURNING lists in a rule",
                    ));
                }
                have_returning = true;
                if qual.is_some() {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "RETURNING lists are not supported in conditional rules",
                    ));
                }
                if !instead {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "RETURNING lists are not supported in non-INSTEAD rules",
                    ));
                }
                check_result_list(&cols, table)?;
            }
            out.push(text);
        }
        Ok(out)
    }
}

impl Binder<'_, '_> {
    /// Bind a stored rule's condition and actions (for what they refer to).
    pub(crate) fn rule_refs(&mut self, r: &Rule) -> Result<()> {
        let table = match (self.cat.tables.get(&r.table), self.cat.views.get(&r.table)) {
            (Some(t), _) => t.clone(),
            (None, Some(v)) => v.as_table(),
            _ => return Ok(()),
        };
        let qual = r.qual.as_deref().map(ast::parse_expr).transpose()?;
        let actions = r
            .actions
            .iter()
            .map(|a| ast::parse_stmt(a))
            .collect::<Result<Vec<_>>>()?;
        self.check_rule(&table, r.event, qual.as_ref(), &actions, r.instead)
            .map(|_| ())
    }
}

/// Apply `f` to the fields of every column reference in `n`.
fn map_colrefs(n: &mut Node, f: &dyn Fn(&mut Vec<Node>)) {
    let mut tree = pg_query::ParseResult::new(
        pb::ParseResult {
            version: 0,
            stmts: vec![pb::RawStmt {
                stmt: Some(Box::new(ast::mk(NodeEnum::SelectStmt(Box::new(pb::SelectStmt {
                    where_clause: Some(Box::new(n.clone())),
                    ..Default::default()
                }))))),
                ..Default::default()
            }],
        },
        String::new(),
    );
    // SAFETY: the pointers come from `tree`, which outlives the loop.
    unsafe {
        for (node, _, _) in tree.protobuf.nodes_mut() {
            if let pg_query::NodeMut::ColumnRef(c) = node {
                f(&mut (*c).fields);
            }
        }
    }
    if let Some(NodeEnum::SelectStmt(s)) = tree
        .protobuf
        .stmts
        .pop()
        .and_then(|s| s.stmt)
        .and_then(|s| s.node)
        && let Some(w) = s.where_clause
    {
        *n = *w;
    }
}

/// A statement's WITH clause.
fn with_clause(n: &Node) -> Option<&pb::WithClause> {
    match inner(n)? {
        NodeEnum::InsertStmt(s) => s.with_clause.as_ref(),
        NodeEnum::UpdateStmt(s) => s.with_clause.as_ref(),
        NodeEnum::DeleteStmt(s) => s.with_clause.as_ref(),
        NodeEnum::SelectStmt(s) => s.with_clause.as_ref(),
        _ => None,
    }
}

/// `checkRuleResultList` for a RETURNING list.
fn check_result_list(cols: &[OutCol], t: &Table) -> Result<()> {
    let bad = |m: String| Error::new(code::INVALID_OBJECT_DEFINITION, m);
    for (i, oc) in cols.iter().enumerate() {
        let Some(c) = t.columns.get(i) else {
            return Err(bad("RETURNING list has too many entries".into()));
        };
        if c.dropped {
            return Err(bad(
                "cannot create a RETURNING list for a relation containing dropped columns".into(),
            ));
        }
        if oc.ty.oid() != c.ty.oid() {
            return Err(bad(format!(
                "RETURNING list's entry {} has different type from column \"{}\"",
                i + 1,
                c.name
            ))
            .detail(format!(
                "RETURNING list entry has type {}, but column has type {}.",
                oc.ty.display(),
                c.ty.display()
            )));
        }
    }
    if cols.len() != t.columns.len() {
        return Err(bad("RETURNING list has too few entries".into()));
    }
    Ok(())
}

/// A scope entry `name` whose columns (those of `t`) are the given expressions.
fn expr_rel(name: &str, t: &Arc<Table>, exprs: Vec<Expr>) -> ScopeRel {
    let mut r = table_rel(t, name, None);
    for (c, e) in r.cols.iter_mut().zip(exprs) {
        c.expr = e;
    }
    r.table = None;
    r.rel = Some(t.oid);
    r
}

/// An INSERT's NEW rows from its bound plan: one value per table column (the value given,
/// else the column's default, else NULL).
fn new_rows(plan: Plan, t: &Arc<Table>) -> Result<Plan> {
    match plan {
        Plan::With { ctes, body, names } => {
            let body = Arc::try_unwrap(body).unwrap_or_else(|a| (*a).clone());
            Ok(Plan::With {
                ctes,
                body: Arc::new(new_rows(body, t)?),
                names,
            })
        }
        Plan::Insert(ip) => {
            let exprs: Vec<Expr> = t
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| match ip.target.iter().position(|x| *x == i) {
                    Some(p) => Expr::Col(p, c.ty.clone()),
                    None => ip
                        .rules
                        .defaults
                        .get(i)
                        .cloned()
                        .flatten()
                        .unwrap_or_else(|| Expr::null(c.ty.clone())),
                })
                .collect();
            let source = Arc::try_unwrap(ip.source).unwrap_or_else(|a| (*a).clone());
            Ok(super::optimize::project(source, exprs))
        }
        _ => Err(crate::error::internal("INSERT rule rows")),
    }
}

/// Errors from statements the rule system made point nowhere in the query text.
fn strip_position(mut e: Error) -> Error {
    e.position = None;
    e
}
