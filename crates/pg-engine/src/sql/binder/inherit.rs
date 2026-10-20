//! Inheritance trees: a scan of a parent reads its descendants too, each projected to the
//! parent's columns.

use crate::catalog::{Catalog, Table};
use crate::exec::scan::NUM_SYS_COLS;
use crate::sql::expr::Expr;
use crate::sql::plan::{Plan, ScanPlan};
use std::sync::Arc;

/// `scan` of a parent table as an Append over the parent and every descendant (Postgres's
/// expand_inherited_rtentry), named `refname` in EXPLAIN. `excl`: each member's constraints
/// for constraint exclusion.
type Member<'a> = dyn FnMut(
        &Arc<Table>,
    ) -> crate::error::Result<(
        Option<Arc<Vec<Expr>>>,
        Option<Arc<Vec<crate::sql::plan::PartLevel>>>,
    )> + 'a;

fn expand(
    cat: &Catalog,
    scan: ScanPlan,
    refname: &str,
    member: &mut Member,
) -> crate::error::Result<Plan> {
    let parent = scan.table.clone();
    let mut inputs = Vec::new();
    for oid in cat.expansion(parent.oid) {
        let Some(t) = cat.table(oid) else { continue };
        // Partitioned tables hold no rows of their own.
        if t.partkey.is_some() {
            continue;
        }
        let mut s = scan.clone();
        s.table = t.clone();
        s.alias = Some(refname.to_string());
        (s.excl, s.part) = member(t)?;
        let p = Plan::Scan(Box::new(s));
        inputs.push(Arc::new(if oid == parent.oid {
            p
        } else {
            Plan::Project {
                exprs: child_columns(&parent, t, scan.with_key, scan.sys),
                input: Arc::new(p),
            }
        }));
    }
    if inputs.is_empty() {
        return Ok(Plan::Empty {
            ncols: super::optimize::width(&Plan::Scan(Box::new(scan))),
            cols: None,
        });
    }
    Ok(Plan::Append {
        inputs,
        inh: Some(refname.to_string()),
    })
}

/// Whether a member's plan reads nothing (excluded by its constraints).
pub fn excluded(p: &Plan) -> bool {
    match p {
        Plan::Empty { .. } => true,
        Plan::Filter { input, .. } | Plan::Project { input, .. } => excluded(input),
        Plan::Join { left, .. } => excluded(left),
        Plan::Update(u) => excluded(&u.source),
        Plan::Delete(d) => excluded(&d.source),
        _ => false,
    }
}

/// A child's scan row (its columns, optional key, optional system columns) as the parent's.
pub fn child_columns(parent: &Table, child: &Table, with_key: bool, sys: bool) -> Vec<Expr> {
    let mut out: Vec<Expr> = parent
        .columns
        .iter()
        .map(|c| {
            match if c.dropped {
                None
            } else {
                child.column_index(&c.name)
            } {
                Some(j) => Expr::Col(j, child.columns[j].ty.clone()),
                None => Expr::null(c.ty.clone()),
            }
        })
        .collect();
    let mut next = child.columns.len();
    if with_key {
        out.push(Expr::Col(next, crate::types::DataType::Bytea));
        next += 1;
    }
    if sys {
        for (k, (_, ty)) in crate::exec::scan::SYS_COLS
            .iter()
            .enumerate()
            .take(NUM_SYS_COLS)
        {
            out.push(Expr::Col(next + k, ty.clone()));
        }
    }
    out
}

/// An expression over the parent's row as one over the child's.
pub fn to_child(e: Expr, parent: &Table, child: &Table) -> Expr {
    if parent.oid == child.oid {
        return e;
    }
    e.remap_cols(&|i| {
        parent
            .columns
            .get(i)
            .and_then(|c| child.column_index(&c.name))
            .unwrap_or(i)
    })
}

impl super::Binder<'_, '_> {
    /// constraint_exclusion: `partition` (inheritance members only), `on` or `off`.
    pub(crate) fn exclusion_mode(&self) -> &'static str {
        match self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("constraint_exclusion")
            .as_deref()
        {
            Some("on") => "on",
            Some("off") => "off",
            _ => "partition",
        }
    }

    /// What constraint exclusion can prove a filter against: validated CHECK constraints
    /// and NOT NULL columns, over the table's row.
    pub(crate) fn exclusion_constraints(
        &mut self,
        t: &Arc<Table>,
    ) -> crate::error::Result<Option<Arc<Vec<Expr>>>> {
        let mut out = Vec::new();
        let saved = std::mem::take(&mut self.perms);
        for c in &t.constraints {
            if let crate::catalog::ConstraintKind::Check { expr, .. } = &c.kind
                && c.validated
            {
                let n = super::ast::parse_expr(expr)?;
                match self.bind_over_table(t, &n, super::Clause::Check) {
                    Ok(e) => out.push(e),
                    Err(e) => {
                        self.perms = saved;
                        return Err(e);
                    }
                }
            }
        }
        self.perms = saved;
        for (i, c) in t.live_columns() {
            if c.not_null {
                out.push(Expr::IsNull(Box::new(Expr::Col(i, c.ty.clone())), true));
            }
        }
        // Virtual generated columns read as their expressions (as the query's do).
        let mut virt: Vec<(usize, Expr)> = Vec::new();
        for (i, c) in t.columns.iter().enumerate() {
            if let (true, false, Some(sql)) = (c.virtual_gen, c.dropped, &c.generated)
                && let Ok(n) = super::ast::parse_expr(sql)
                && let Ok(e) = self.bind_over_table(t, &n, super::Clause::Generated)
                && let Ok(e) = self.coerce_assign(e, &c.ty, &c.name)
            {
                virt.push((i, e));
            }
        }
        if !virt.is_empty() {
            out = out
                .into_iter()
                .map(|e| {
                    e.clone()
                        .transform(&mut |x| {
                            Ok(match x {
                                Expr::Col(i, t) => virt
                                    .iter()
                                    .find(|(j, _)| *j == i)
                                    .map_or(Expr::Col(i, t), |(_, v)| v.clone()),
                                x => x,
                            })
                        })
                        .unwrap_or(e)
                })
                .collect();
        }
        // (Even with none, the restriction can contradict itself.)
        Ok(Some(Arc::new(out)))
    }

    /// Planning a scan of (or a change to) a foreign table: its wrapper's handler can't run
    /// (GetFdwRoutineForRelation fails as Postgres's does for a wrapper without one).
    pub(crate) fn check_foreign_scan(&self, t: &Table) -> crate::error::Result<()> {
        // (restrict_nonsystem_relation_kind is checked before the wrapper is looked at.)
        if t.is_foreign() && !self.deparse {
            self.check_foreign_restricted()?;
        }
        if t.is_foreign()
            && !self.unplanned
            && !self.deparse
            && self.deps.is_none()
            && let Some(e) = self.cat.fdw_routine_error(t)
        {
            return Err(e);
        }
        Ok(())
    }

    /// A parent table's scan over its whole inheritance tree.
    pub(crate) fn expand_inheritance(
        &mut self,
        scan: ScanPlan,
        refname: &str,
    ) -> crate::error::Result<Plan> {
        let cat = self.cat.clone();
        let on = self.exclusion_mode() != "off";
        let root = scan.table.oid;
        expand(&cat, scan, refname, &mut |t| {
            self.check_foreign_scan(t)?;
            Ok((
                if on {
                    self.exclusion_constraints(t)?
                } else {
                    None
                },
                self.partition_levels(t, Some(root))?,
            ))
        })
    }
}
