//! Automatically updatable views (rewriteHandler.c's `rewriteTargetView`): INSERT / UPDATE /
//! DELETE on a simple view becomes the same statement on its base table, with the view's
//! columns as expressions over the base row and its WHERE added as a filter.

use super::ast::{self, inner};
use super::{Binder, Level, Scope, ScopeRel, derived_rel, table_rel};
use crate::catalog::{Relation, Table, ViewDef};
use crate::error::Result;
use crate::sql::expr::Expr;
use crate::sql::plan::OutCol;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// A view column: its value over the base row, and the base column when it is a plain one.
#[derive(Clone, Debug)]
pub struct ViewCol {
    pub name: String,
    pub expr: Expr,
    pub base: Option<usize>,
    /// Why it can't be written (when `base` is None).
    pub why: &'static str,
}

/// An automatically updatable view resolved down to its base table.
#[derive(Clone, Debug)]
pub struct ViewBase {
    pub view: Arc<ViewDef>,
    pub base: Arc<Table>,
    pub cols: Vec<ViewCol>,
    /// Every view layer's WHERE over the base row, innermost first: (view, condition, whether
    /// a check option makes new rows satisfy it).
    pub conds: Vec<(String, Expr, bool)>,
    /// The views written through, outermost first, each with its columns' places in the
    /// relation under it (and why a column has none).
    pub layers: Vec<(Arc<ViewDef>, Vec<Option<usize>>, Vec<&'static str>)>,
}

impl ViewBase {
    /// The innermost view's alias for its relation (`FROM base_tbl b`): the result
    /// relation's name once the rewriter makes it the target.
    pub fn base_alias(&self) -> Option<String> {
        let v = self.layers.last().map_or(&self.view, |l| &l.0);
        match ast::parse_stmt(&v.query).ok().and_then(|n| n.node) {
            Some(NodeEnum::SelectStmt(s)) => match s.from_clause.first().and_then(inner) {
                Some(NodeEnum::RangeVar(rv)) => rv.alias.as_ref().map(|a| a.aliasname.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether the innermost view reads its relation with inheritance (no ONLY).
    pub fn base_inh(&self) -> bool {
        let v = self.layers.last().map_or(&self.view, |l| &l.0);
        match ast::parse_stmt(&v.query).ok().and_then(|n| n.node) {
            Some(NodeEnum::SelectStmt(s)) => match s.from_clause.first().and_then(inner) {
                Some(NodeEnum::RangeVar(rv)) => rv.inh,
                _ => true,
            },
            _ => true,
        }
    }
}

/// Why a view isn't automatically updatable: the view at fault and the error detail.
pub type NotUpdatable = (String, &'static str);

pub(crate) const NON_COLUMN: &str =
    "View columns that are not columns of their base relation are not updatable.";
const SYSTEM_COLUMN: &str = "View columns that refer to system columns are not updatable.";

impl ViewBase {
    /// The view's columns as a FROM item named `alias` (expressions over the base row).
    pub fn scope_rel(&self, alias: &str) -> ScopeRel {
        let cols: Vec<OutCol> = self
            .cols
            .iter()
            .map(|c| OutCol {
                name: c.name.clone(),
                ty: c.expr.ty(),
                table: self.view.oid,
                attnum: 0,
                domain_array: None,
                domain: None,
                collation: None,
            })
            .collect();
        let mut rel = derived_rel(alias, &cols, 0);
        for (i, (sc, vc)) in rel.cols.iter_mut().zip(&self.cols).enumerate() {
            sc.expr = vc.expr.clone();
            sc.attnum = i as i16 + 1;
        }
        // Its whole row (RETURNING `v`, `old`, `new`) has the view's row type.
        rel.rel = Some(self.view.oid);
        rel
    }

    /// `view_cols_are_auto_updatable` view by view, outermost first: the first of the
    /// `required` top-level columns a view doesn't have as a plain column of the relation
    /// under it (in its column order): the view, its column, and why.
    pub fn first_not_updatable(&self, required: &[usize]) -> Option<(String, String, &'static str)> {
        let mut req = required.to_vec();
        for (view, map, why) in &self.layers {
            for (k, m) in map.iter().enumerate() {
                if m.is_none() && req.contains(&k) {
                    let name = view.columns.get(k).map_or_else(String::new, |c| c.0.clone());
                    return Some((view.name.clone(), name, why[k]));
                }
            }
            req = req.iter().filter_map(|k| map.get(*k).copied().flatten()).collect();
        }
        None
    }

    /// The conditions new rows must satisfy, innermost view first.
    pub fn checks(&self) -> Vec<(String, Expr)> {
        self.conds
            .iter()
            .filter(|c| c.2)
            .map(|c| (c.0.clone(), c.1.clone()))
            .collect()
    }

    /// The views' WHERE conditions split as rewriteTargetView adds them: a security barrier
    /// view's become security quals of the base relation (sublinks in them stay
    /// SubPlans), the others the statement's quals (pull_up_sublinks sees them). Each
    /// ANDed, the outer view's first.
    pub fn filters_by_security(&self) -> (Option<Expr>, Option<Expr>) {
        let secure = |name: &str| {
            std::iter::once(&self.view)
                .chain(self.layers.iter().map(|l| &l.0))
                .any(|v| v.name == name && v.option("security_barrier"))
        };
        let and = |v: Vec<Expr>| (!v.is_empty()).then(|| Expr::And(v));
        let (s, p): (Vec<_>, Vec<_>) = self.conds.iter().rev().partition(|c| secure(&c.0));
        (and(s.into_iter().map(|c| c.1.clone()).collect()), and(p.into_iter().map(|c| c.1.clone()).collect()))
    }

    /// All the views' WHERE conditions, ANDed.
    pub fn filter(&self) -> Option<Expr> {
        match self.conds.len() {
            0 => None,
            // (The outer view's first: the rewriter adds each view's as it gets to it.)
            _ => Some(Expr::And(self.conds.iter().rev().map(|c| c.1.clone()).collect())),
        }
    }
}

/// Whether a select-list item names a system column of the base relation (`t`'s, when it
/// has no user column of that name).
fn system_column_ref(n: &pb::Node, t: &Table, alias: &str) -> Option<crate::types::DataType> {
    let Some(NodeEnum::ResTarget(rt)) = inner(n) else {
        return None;
    };
    let Some(NodeEnum::ColumnRef(c)) = rt.val.as_deref().and_then(inner) else {
        return None;
    };
    let names: Vec<&str> = c.fields.iter().filter_map(ast::sval).collect();
    if names.len() != c.fields.len() {
        return None;
    }
    let col = match names.as_slice() {
        [col] => *col,
        [q, col] if *q == alias => *col,
        _ => return None,
    };
    if t.column_index(col).is_some() {
        return None;
    }
    if col == "tableoid" {
        return Some(crate::types::DataType::Oid);
    }
    crate::exec::scan::SYS_COLS
        .iter()
        .find(|(n, _)| *n == col)
        .map(|(_, ty)| ty.clone())
}

impl Binder<'_, '_> {
    /// Resolve a view for INSERT / UPDATE / DELETE (`cols`: whether the statement assigns
    /// columns). `cascaded`: an outer view has WITH CASCADED CHECK OPTION.
    pub(crate) fn auto_update_view(
        &mut self,
        v: &Arc<ViewDef>,
        cols: bool,
        cascaded: bool,
    ) -> Result<std::result::Result<ViewBase, NotUpdatable>> {
        self.check_view_restricted(v)?;
        self.view_update_layer(v, cols, cascaded, false)
    }

    /// `auto_update_view`, down to the base table; `shallow`: only this view's own query
    /// (`view_query_is_auto_updatable`), its base relation a view written as itself.
    pub(crate) fn view_update_layer(
        &mut self,
        v: &Arc<ViewDef>,
        cols: bool,
        cascaded: bool,
        shallow: bool,
    ) -> Result<std::result::Result<ViewBase, NotUpdatable>> {
        let fail = |why: &'static str| Ok(Err((v.name.clone(), why)));
        let Some(NodeEnum::SelectStmt(s)) = ast::parse_stmt(&v.query)?.node else {
            return fail(SINGLE);
        };
        if !s.distinct_clause.is_empty() {
            return fail("Views containing DISTINCT are not automatically updatable.");
        }
        if !s.group_clause.is_empty() {
            return fail("Views containing GROUP BY are not automatically updatable.");
        }
        if s.having_clause.is_some() {
            return fail("Views containing HAVING are not automatically updatable.");
        }
        if s.op != pb::SetOperation::SetopNone as i32 {
            return fail(
                "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.",
            );
        }
        if s.with_clause.is_some() {
            return fail("Views containing WITH are not automatically updatable.");
        }
        if s.limit_count.is_some() || s.limit_offset.is_some() {
            return fail("Views containing LIMIT or OFFSET are not automatically updatable.");
        }
        if !s.values_lists.is_empty() || s.from_clause.len() != 1 {
            return fail(SINGLE);
        }
        let rv = match inner(&s.from_clause[0]) {
            Some(NodeEnum::RangeVar(rv)) => rv,
            Some(NodeEnum::RangeTableSample(_)) => {
                return fail("Views containing TABLESAMPLE are not automatically updatable.");
            }
            _ => return fail(SINGLE),
        };
        let schema = if rv.schemaname.is_empty() {
            None
        } else {
            Some(rv.schemaname.as_str())
        };
        let alias = rv
            .alias
            .as_ref()
            .map(|a| a.aliasname.clone())
            .unwrap_or_else(|| rv.relname.clone());
        let cascade_down = cascaded || v.check_option.as_deref() == Some("cascaded");
        // The base relation: a table, or another automatically updatable view.
        let (base, inner_view, rel) =
            match self.cat.find(schema, &rv.relname, &self.ctx.search_path) {
                Some(Relation::Table(t)) => {
                    let rel = table_rel(&t, &alias, None);
                    (t, None, rel)
                }
                // A view with INSTEAD OF triggers for the statement's events takes the write
                // itself (the rewrite stops there).
                Some(Relation::View(iv))
                    if !iv.materialized
                        && (shallow || self.instead_triggers_for(iv.oid)) =>
                {
                    let t = iv.as_table();
                    let rel = table_rel(&t, &alias, None);
                    (t, None, rel)
                }
                Some(Relation::View(iv)) if !iv.materialized => {
                    match self.auto_update_view(&iv, cols, cascade_down)? {
                        Ok(vb) => {
                            let rel = vb.scope_rel(&alias);
                            (vb.base.clone(), Some(vb), rel)
                        }
                        Err(e) => return Ok(Err(e)),
                    }
                }
                _ => return fail(SINGLE),
            };
        // Select-list items naming a system column of the base table stand in as NULLs:
        // they read, but can't be written.
        let mut targets_list = s.target_list.clone();
        let mut sys_cols: Vec<usize> = Vec::new();
        // Over a view: which of its columns each item names (a plain reference).
        let mut names_under: Vec<Option<usize>> = Vec::new();
        {
            // The columns of the relation right under the view.
            let under_names: Vec<Option<String>> = match &inner_view {
                Some(iv) => iv.view.columns.iter().map(|c| Some(c.0.clone())).collect(),
                None => (base.columns.iter())
                    .map(|c| (!c.dropped).then(|| c.name.clone()))
                    .collect(),
            };
            let col_index =
                |n: &str| under_names.iter().position(|x| x.as_deref() == Some(n));
            let live = under_names.iter().filter(|c| c.is_some()).count();
            let mut pos = 0;
            for t in &mut targets_list {
                let star = matches!(inner(t), Some(NodeEnum::ResTarget(rt))
                    if matches!(rt.val.as_deref().and_then(inner), Some(NodeEnum::ColumnRef(c))
                        if matches!(c.fields.last().and_then(inner), Some(NodeEnum::AStar(_)))));
                if star {
                    pos += live;
                    names_under.extend(
                        (0..under_names.len())
                            .filter(|i| under_names[*i].is_some())
                            .map(Some),
                    );
                    continue;
                }
                names_under.push(match inner(t) {
                    Some(NodeEnum::ResTarget(rt)) => match rt.val.as_deref().and_then(inner) {
                        Some(NodeEnum::ColumnRef(c)) => {
                            let names: Vec<&str> = c.fields.iter().filter_map(ast::sval).collect();
                            match names.as_slice() {
                                [n] => col_index(n),
                                [q, n] if *q == alias => col_index(n),
                                _ => None,
                            }
                        }
                        _ => None,
                    },
                    _ => None,
                });
                if inner_view.is_none()
                    && system_column_ref(t, &base, &alias).is_some()
                    && let Some(NodeEnum::ResTarget(rt)) = t.node.as_mut()
                {
                    rt.val = Some(Box::new(ast::mk(NodeEnum::AConst(pb::AConst {
                        isnull: true,
                        location: -1,
                        val: None,
                    }))));
                    sys_cols.push(pos);
                }
                pos += 1;
            }
        }
        // Bind the view's select list and WHERE over the base row, as the view owner, without
        // the column reads counting as SELECTs.
        let saved_perms = self.perms.clone();
        let saved_as = self.check_as;
        self.check_as = if v.option("security_invoker") {
            self.definer
        } else {
            Some(v.owner)
        };
        let mut lvl = Level::new();
        lvl.barrier = true;
        lvl.scope = Scope { rels: vec![rel] };
        self.levels.push(lvl);
        let saved_ctes = std::mem::take(&mut self.ctes);
        let saved_fns = std::mem::take(&mut self.fn_levels);
        let r = (|| -> Result<_> {
            let targets =
                self.in_clause(super::Clause::Select, |b| b.bind_targets(&targets_list))?;
            let st = std::mem::take(&mut self.level().st);
            let cond = match s.where_clause.as_deref() {
                Some(w) => {
                    let e = self.in_clause(super::Clause::Where, |b| b.bind_expr(w))?;
                    Some(self.coerce_bool(e, "WHERE")?)
                }
                None => None,
            };
            Ok((targets, st, cond))
        })();
        self.fn_levels = saved_fns;
        self.ctes = saved_ctes;
        self.levels.pop();
        self.check_as = saved_as;
        self.perms = saved_perms;
        let (targets, st, cond) = r?;
        if !st.aggs.is_empty() {
            return fail("Views that return aggregate functions are not automatically updatable.");
        }
        if !st.windows.is_empty() {
            return fail("Views that return window functions are not automatically updatable.");
        }
        if !st.srfs.is_empty() {
            return fail(
                "Views that return set-returning functions are not automatically updatable.",
            );
        }
        let n = base.columns.len();
        // Each column's place in the relation right under the view.
        let under: Vec<Option<usize>> = targets
            .iter()
            .enumerate()
            .map(|(k, t)| match &inner_view {
                _ if sys_cols.contains(&k) => None,
                Some(_) => names_under.get(k).copied().flatten(),
                None => match &t.expr {
                    Expr::Col(c, _) if *c < n => Some(*c),
                    _ => None,
                },
            })
            .collect();
        let cols_out: Vec<ViewCol> = targets
            .into_iter()
            .zip(&v.columns)
            .enumerate()
            .map(|(k, (t, (name, ty)))| {
                if sys_cols.contains(&k) {
                    return ViewCol {
                        name: name.clone(),
                        expr: Expr::null(ty.clone()),
                        base: None,
                        why: SYSTEM_COLUMN,
                    };
                }
                let base_col = match &t.expr {
                    Expr::Col(c, _) if *c < n => Some(*c),
                    _ => None,
                };
                ViewCol {
                    name: name.clone(),
                    expr: t.expr,
                    base: base_col,
                    why: NON_COLUMN,
                }
            })
            .collect();
        // (Its own columns: a lower view's computed ones make an error about them later.)
        if cols && under.iter().all(|c| c.is_none()) {
            return fail("Views that have no updatable columns are not automatically updatable.");
        }
        let whys: Vec<&'static str> = (0..under.len())
            .map(|k| if sys_cols.contains(&k) { SYSTEM_COLUMN } else { NON_COLUMN })
            .collect();
        let mut layers = vec![(v.clone(), under, whys)];
        let mut conds = match inner_view {
            Some(iv) => {
                layers.extend(iv.layers);
                iv.conds
            }
            None => Vec::new(),
        };
        if let Some(c) = cond {
            conds.push((v.name.clone(), c, cascaded || v.check_option.is_some()));
        }
        Ok(Ok(ViewBase {
            view: v.clone(),
            base,
            cols: cols_out,
            conds,
            layers,
        }))
    }
}

/// An INSERT ... VALUES on a view with column defaults, as `rewriteTargetListIU` makes it:
/// DEFAULT items and the columns not given take the view's defaults. None: no change.
pub(crate) fn with_view_defaults(s: &pb::InsertStmt, v: &ViewDef) -> Option<pb::InsertStmt> {
    if v.defaults.iter().all(Option::is_none) {
        return None;
    }
    let sel = match s.select_stmt.as_deref().and_then(inner) {
        Some(NodeEnum::SelectStmt(sel)) if !sel.values_lists.is_empty() => sel,
        _ => return None,
    };
    let width = match sel.values_lists.first().and_then(inner) {
        Some(NodeEnum::List(l)) => l.items.len(),
        _ => return None,
    };
    // The columns given, and whether only part of each (a subscript or field).
    let (given, partial): (Vec<usize>, Vec<bool>) = if s.cols.is_empty() {
        (0..width.min(v.columns.len())).map(|k| (k, false)).unzip()
    } else {
        s.cols
            .iter()
            .map(|c| match inner(c) {
                Some(NodeEnum::ResTarget(rt)) => v
                    .columns
                    .iter()
                    .position(|x| x.0 == rt.name)
                    .map(|k| (k, !rt.indirection.is_empty())),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .unzip()
    };
    let default = |k: usize| {
        v.defaults
            .get(k)
            .cloned()
            .flatten()
            .and_then(|d| ast::parse_expr(&d).ok())
    };
    let missing: Vec<usize> = (0..v.columns.len())
        .filter(|k| !given.contains(k) && default(*k).is_some())
        .collect();
    let mut ns = s.clone();
    let Some(NodeEnum::SelectStmt(nsel)) = ns.select_stmt.as_deref_mut().and_then(|n| n.node.as_mut())
    else {
        return None;
    };
    for row in &mut nsel.values_lists {
        let Some(NodeEnum::List(l)) = row.node.as_mut() else {
            continue;
        };
        for (j, item) in l.items.iter_mut().enumerate() {
            if matches!(inner(item), Some(NodeEnum::SetToDefault(_)))
                && !partial.get(j).copied().unwrap_or(false)
                && let Some(d) = given.get(j).and_then(|k| default(*k))
            {
                *item = d;
            }
        }
        for k in &missing {
            l.items.push(default(*k)?);
        }
    }
    if !missing.is_empty() {
        let target = |name: &str| {
            ast::mk(NodeEnum::ResTarget(Box::new(pb::ResTarget {
                name: name.to_string(),
                location: -1,
                ..Default::default()
            })))
        };
        if ns.cols.is_empty() {
            ns.cols = given.iter().map(|k| target(&v.columns[*k].0)).collect();
        }
        ns.cols
            .extend(missing.iter().map(|k| target(&v.columns[*k].0)));
    }
    Some(ns)
}

impl Binder<'_, '_> {
    /// Whether view `oid` has INSTEAD OF row triggers for every event being written
    /// (`view_events`).
    fn instead_triggers_for(&self, oid: u32) -> bool {
        use crate::catalog::trigger_type as tt;
        let events = self.view_events;
        events != 0
            && [tt::INSERT, tt::UPDATE, tt::DELETE]
                .into_iter()
                .filter(|e| events & e != 0)
                .all(|e| {
                    self.cat.triggers.values().any(|x| {
                        x.table == oid
                            && x.tgtype & tt::INSTEAD != 0
                            && x.tgtype & tt::ROW != 0
                            && x.tgtype & e != 0
                    })
                })
    }

    /// The privileges a write through updatable view `vb` needs under it: each relation
    /// under a view needs `privs` as that view's owner (its invoker with security_invoker),
    /// outermost first. `cols`: the top view's columns written (as INSERT or UPDATE
    /// columns), noted at each layer.
    pub(crate) fn require_view_layers(
        &mut self,
        vb: &ViewBase,
        privs: u32,
        cols: Option<(&[usize], bool)>,
    ) {
        let saved = self.check_as;
        let mut cur: Vec<usize> = cols.map(|c| c.0.to_vec()).unwrap_or_default();
        for (k, (view, map, _)) in vb.layers.iter().enumerate() {
            self.check_as = if view.option("security_invoker") {
                self.definer
            } else {
                Some(view.owner)
            };
            let next = vb.layers.get(k + 1).map_or(vb.base.oid, |l| l.0.oid);
            cur = cur
                .iter()
                .filter_map(|c| map.get(*c).copied().flatten())
                .collect();
            let idx = self.require_rel(next, privs);
            if let Some((_, insert)) = cols {
                self.note_target_cols(idx, &cur, insert);
            }
        }
        self.check_as = saved;
    }

    /// A write through updatable view `vb` that reads the view: each view's owner (its
    /// invoker with security_invoker) needs SELECT on every column of the relation under
    /// it that the view's query mentions (`rewriteTargetView` leaves selectedCols whole).
    pub(crate) fn require_view_reads(&mut self, vb: &ViewBase) {
        let saved = self.check_as;
        for (k, (view, ..)) in vb.layers.iter().enumerate() {
            self.check_as = if view.option("security_invoker") {
                self.definer
            } else {
                Some(view.owner)
            };
            let (next, under): (u32, Vec<Option<String>>) = match vb.layers.get(k + 1) {
                Some(l) => (l.0.oid, l.0.columns.iter().map(|c| Some(c.0.clone())).collect()),
                None => (
                    vb.base.oid,
                    (vb.base.columns.iter())
                        .map(|c| (!c.dropped).then(|| c.name.clone()))
                        .collect(),
                ),
            };
            let named = mentioned_columns(&view.query);
            let idx = self.require_rel(next, crate::catalog::acl::SELECT);
            for (i, c) in under.iter().enumerate() {
                if let Some(c) = c
                    && (named.contains(&None) || named.contains(&Some(c.clone())))
                {
                    self.perms.rels[idx].select_cols.insert(i as i16 + 1);
                }
            }
        }
        self.check_as = saved;
    }
}

/// The column names a view's query mentions (None: a `*`).
fn mentioned_columns(query: &str) -> Vec<Option<String>> {
    let Ok(tree) = pg_query::parse(query, 0) else {
        return vec![None];
    };
    let mut out = Vec::new();
    for (node, ..) in tree.protobuf.nodes() {
        if let pg_query::NodeRef::ColumnRef(c) = node {
            match c.fields.last().and_then(|f| f.node.as_ref()) {
                Some(NodeEnum::String(s)) => out.push(Some(s.sval.clone())),
                Some(NodeEnum::AStar(_)) => out.push(None),
                _ => {}
            }
        }
    }
    out
}

const SINGLE: &str =
    "Views that do not select from a single table or view are not automatically updatable.";

const UPDATE: i32 = 1 << 2;
const INSERT: i32 = 1 << 3;
const DELETE: i32 = 1 << 4;
const ALL_EVENTS: i32 = UPDATE | INSERT | DELETE;

/// `relation_is_updatable`: the events (1 << CMD_UPDATE | 1 << CMD_INSERT | 1 << CMD_DELETE)
/// relation `oid` supports through its unconditional INSTEAD rules, its INSTEAD OF triggers
/// (`include_triggers`), or as an automatically updatable view (limited to `include_cols`,
/// view column indexes, when given). `outer`: the views being looked through.
fn relation_events(
    b: &mut Binder,
    oid: u32,
    outer: &mut Vec<u32>,
    include_triggers: bool,
    include_cols: Option<&[usize]>,
) -> i32 {
    use crate::catalog::trigger_type as tt;
    if outer.contains(&oid) {
        return 0;
    }
    if b.cat.tables.contains_key(&oid) {
        return ALL_EVENTS;
    }
    let Some(v) = b.cat.views.get(&oid).cloned() else {
        return 0;
    };
    if v.materialized {
        return 0;
    }
    let mut events = 0;
    for r in b.cat.rules.values().filter(|r| r.table == oid) {
        if r.instead && r.qual.is_none() {
            events |= match r.event {
                '2' => UPDATE,
                '3' => INSERT,
                '4' => DELETE,
                _ => 0,
            };
        }
    }
    if events == ALL_EVENTS {
        return events;
    }
    if include_triggers {
        for x in b
            .cat
            .triggers
            .values()
            .filter(|x| x.table == oid && x.tgtype & tt::INSTEAD != 0 && x.tgtype & tt::ROW != 0)
        {
            for (bit, ev) in [(UPDATE, tt::UPDATE), (INSERT, tt::INSERT), (DELETE, tt::DELETE)] {
                if x.tgtype & ev != 0 {
                    events |= bit;
                }
            }
        }
        if events == ALL_EVENTS {
            return events;
        }
    }
    if let Ok(Ok(vb)) = b.view_update_layer(&v, false, false, true) {
        let updatable: Vec<usize> = (0..vb.cols.len())
            .filter(|k| vb.cols[*k].base.is_some())
            .filter(|k| include_cols.is_none_or(|i| i.contains(k)))
            .collect();
        let mut auto = if updatable.is_empty() {
            DELETE
        } else {
            ALL_EVENTS
        };
        if b.cat.views.contains_key(&vb.base.oid) {
            let base_cols: Vec<usize> = updatable.iter().filter_map(|k| vb.cols[*k].base).collect();
            outer.push(oid);
            auto &= relation_events(b, vb.base.oid, outer, include_triggers, Some(&base_cols));
            outer.pop();
        }
        events |= auto;
    }
    events
}

/// pg_relation_is_updatable's event mask for a view and, per column, pg_column_is_updatable
/// (it supports both UPDATE and DELETE when only that column is looked at).
pub fn view_updatability(
    ctx: &crate::exec::ExecCtx,
    v: &Arc<ViewDef>,
    include_triggers: bool,
) -> (i32, Vec<bool>) {
    let mut b = Binder::new(ctx, Vec::new());
    let mask = relation_events(&mut b, v.oid, &mut Vec::new(), include_triggers, None);
    let cols = (0..v.columns.len())
        .map(|k| {
            relation_events(&mut b, v.oid, &mut Vec::new(), include_triggers, Some(&[k]))
                & (UPDATE | DELETE)
                == UPDATE | DELETE
        })
        .collect();
    (mask, cols)
}
