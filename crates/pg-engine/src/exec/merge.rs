//! MERGE: each row of the source joined with the target runs the first WHEN clause that
//! applies to it (MATCHED, NOT MATCHED BY SOURCE, NOT MATCHED [BY TARGET]) and whose
//! condition holds, through the same row paths as INSERT / UPDATE / DELETE.

use super::dml::{
    Inserted, Owner, delete_row, insert_new, insert_routed, key_of, move_row, new_row,
    partition_ok, update_row_rls,
};
use super::scan::{Locked, Xltw, data_key, decode_table_row, lock_row, sys_values};
use super::trigger::{self, AfterRow};
use super::{BoxNode, ExecCtx, Node, Row, eval_all};
use crate::catalog::trigger_type as tt;
use crate::error::{Error, Result, code};
use crate::sql::expr::{eval, eval_bool};
use crate::sql::plan::{MergeCmd, MergePlan, MergeWhen, RlsCheck, RowMark, Wait};
use crate::types::Value;
use std::collections::HashSet;

pub struct MergeNode {
    plan: Box<MergePlan>,
    /// The source joined with the target, read one row at a time (ExecMerge per tuple): each
    /// row's action, with its row triggers, runs before the next row is fetched. The join reads
    /// the target through the statement's snapshot, so it doesn't see what the actions write.
    source: BoxNode,
    /// Target rows updated or deleted so far (by their key in the join).
    touched: HashSet<Vec<u8>>,
    /// AFTER ROW events in the order the rows changed: of the target, an inheritance child
    /// or a partition rows were routed to.
    ordered: Vec<(Owner, AfterRow)>,
    /// The rows for the target's transition tables (as its rows).
    captured: Vec<AfterRow>,
    /// The target's triggers read transition tables.
    trans: bool,
    events: Vec<u16>,
    update_cols: Vec<usize>,
    done: bool,
    /// A joined row whose target a concurrent update made no longer match, waiting for its
    /// WHEN NOT MATCHED [BY TARGET] action after the target's NOT MATCHED BY SOURCE one.
    pending: Option<Row>,
}

impl MergeNode {
    pub fn new(plan: Box<MergePlan>, ctx: &ExecCtx) -> Result<Self> {
        ctx.txn.borrow_mut().output_cid();
        // CheckValidResultRel: each UPDATE / DELETE action, on the target and its members.
        if !plan.rules.view {
            use crate::catalog::pubsub::RiCmd;
            let tables = std::iter::once(&plan.rules.table)
                .chain(plan.members.iter().map(|m| &m.rules.table));
            for t in tables {
                // (A foreign table among them: no MERGE, whatever the wrapper. Not when a
                // constant false join condition leaves only the dummy root result relation.)
                let excluded = matches!(
                    &plan.join_cond,
                    Some(crate::sql::expr::Expr::Const(crate::types::Value::Bool(false) | crate::types::Value::Null, _))
                );
                if t.foreign.is_some() && !excluded {
                    return Err(crate::error::Error::new(
                        crate::error::code::FEATURE_NOT_SUPPORTED,
                        format!("cannot execute MERGE on relation \"{}\"", t.name),
                    )
                    .detail("This operation is not supported for foreign tables."));
                }
                for a in &plan.actions {
                    let cmd = match a.cmd {
                        crate::sql::plan::MergeCmd::Update { .. } => RiCmd::Update,
                        crate::sql::plan::MergeCmd::Delete { .. } => RiCmd::Delete,
                        _ => continue,
                    };
                    super::dml::check_replica_identity(t, cmd, ctx)?;
                }
            }
        }
        let events = plan.events();
        let update_cols = plan.update_cols();
        for e in &events {
            trigger::statement(
                &plan.rules.triggers,
                &plan.rules.table,
                true,
                *e,
                if *e == tt::UPDATE { &update_cols } else { &[] },
                ctx,
            )?;
        }
        let trans = !plan.rules.view
            && events
                .iter()
                .any(|e| super::dml::reads_transition(&plan.rules.triggers, *e));
        let source = super::nodes::open(&plan.source, ctx)?;
        Ok(MergeNode {
            plan,
            source,
            touched: HashSet::new(),
            ordered: Vec::new(),
            captured: Vec::new(),
            trans,
            events,
            update_cols,
            done: false,
            pending: None,
        })
    }

    /// Run the WHEN clause that applies to one joined row; the RETURNING row if it did
    /// something and the statement has RETURNING.
    fn merge_row(&mut self, mut row: Row, ctx: &ExecCtx) -> Result<Option<Row>> {
        let p = &self.plan;
        let rules = &p.rules;
        let t = rules.table.clone();
        let n = t.columns.len();
        let off = p.target;
        let kpos = off + n;
        let mut has_source = p.source_flag.is_none_or(|f| row[f] == Value::Bool(true));
        let mut has_target = !row[kpos].is_null();
        let mut locked = false;
        // A row of an inheritance child: (member, its full row).
        let member = match row.get(kpos + 1 + crate::exec::scan::NUM_SYS_COLS - 1) {
            Some(Value::Oid(o)) if has_target && p.sys && *o != t.oid => {
                p.members.iter().position(|m| m.rules.table.oid == *o)
            }
            _ => None,
        };
        let mut child_old: Option<Row> = None;
        loop {
            let when = match (has_target, has_source) {
                (true, true) => MergeWhen::Matched,
                (true, false) => MergeWhen::NotMatchedBySource,
                (false, true) => MergeWhen::NotMatchedByTarget,
                (false, false) => return Ok(None),
            };
            let mut chosen = None;
            for a in p.actions.iter().filter(|a| a.when == when) {
                if match &a.cond {
                    Some(c) => eval_bool(c, &row, ctx)?,
                    None => true,
                } {
                    chosen = Some(a);
                    break;
                }
            }
            let Some(action) = chosen else {
                return Ok(None);
            };
            let ai = p
                .actions
                .iter()
                .position(|a| std::ptr::eq(a, action))
                .expect("an action");
            match &action.cmd {
                MergeCmd::Nothing => return Ok(None),
                MergeCmd::Insert {
                    target,
                    values,
                    rls_new,
                } => {
                    let src = eval_all(values, &row, ctx)?;
                    if let Some(routing) = p.route.clone() {
                        let new = new_row(rules, target, src, ctx)?;
                        let mut routed = Vec::new();
                        let r = insert_routed(&routing, rls_new, new, &mut routed, ctx);
                        self.ordered
                            .extend(routed.into_iter().map(|(k, a)| (Owner::Leaf(k), a)));
                        let Some((leaf, stored, key)) = r? else {
                            return Ok(None);
                        };
                        if self.trans {
                            self.captured.push(AfterRow {
                                event: tt::INSERT,
                                new: Some(stored.clone()),
                                ..Default::default()
                            });
                        }
                        crate::exec::dml::check_view_options(rules, &stored, ctx)?;
                        ctx.processed.set(ctx.processed.get() + 1);
                        return self.returning(
                            row,
                            stored,
                            &key,
                            routing.leaves[leaf].rules.table.oid,
                            "INSERT",
                            ctx,
                        );
                    }
                    let mut queued = Vec::new();
                    let r = insert_new(rules, target, src, rls_new, None, &mut queued, ctx);
                    self.ordered
                        .extend(queued.into_iter().map(|a| (Owner::Root, a)));
                    return match r? {
                        Inserted::Stored(new, key) => {
                            if self.trans {
                                self.captured.push(AfterRow {
                                    event: tt::INSERT,
                                    new: Some(new.clone()),
                                    ..Default::default()
                                });
                            }
                            ctx.processed.set(ctx.processed.get() + 1);
                            self.returning(row, new, &key, t.oid, "INSERT", ctx)
                        }
                        _ => Ok(None),
                    };
                }
                MergeCmd::Update { .. } | MergeCmd::Delete { .. } => {}
            }
            let update = matches!(action.cmd, MergeCmd::Update { .. });
            if rules.view {
                // INSTEAD OF triggers do the work.
                let old: Row = row[off..off + n].to_vec();
                let (event, new) = match &action.cmd {
                    MergeCmd::Update { sets, .. } => {
                        let mut new = old.clone();
                        super::dml::assign(&mut new, sets, &row, ctx)?;
                        (tt::UPDATE, Some(new))
                    }
                    _ => (tt::DELETE, None),
                };
                let Some(stored) =
                    trigger::instead(&rules.triggers, &t, event, Some(&old), new, ctx)?
                else {
                    return Ok(None);
                };
                if update {
                    super::dml::check_view_options(rules, &stored, ctx)?;
                }
                ctx.processed.set(ctx.processed.get() + 1);
                return self.returning(
                    row,
                    if update { stored } else { old },
                    &[],
                    t.oid,
                    if update { "UPDATE" } else { "DELETE" },
                    ctx,
                );
            }
            // A child's row is changed in its own table, with its own rules.
            let (rules, t, cmd) = match member {
                Some(m) => (
                    &p.members[m].rules,
                    p.members[m].rules.table.clone(),
                    &p.members[m].cmds[ai],
                ),
                None => (rules, t.clone(), &action.cmd),
            };
            let dkey = key_of(&row[kpos])?;
            if self.touched.contains(&dkey) {
                return Err(second_time());
            }
            self_modified_check(&dkey, ctx)?;
            if !locked {
                locked = true;
                match lock_row(
                    &t,
                    &dkey,
                    if update {
                        RowMark::NoKeyUpdate
                    } else {
                        RowMark::Update
                    },
                    Wait::Block,
                    if update { Xltw::Update } else { Xltw::Delete },
                    ctx,
                // (A row moved to another partition fails as table_tuple_lock reports it,
                // "tuple to be locked was already moved ...", before ExecMergeMatched could.)
                )? {
                    Locked::Same => {
                        if member.is_some() {
                            let bytes = ctx.txn.borrow().get_latest(&dkey)?.and_then(|(_, b)| b);
                            child_old = bytes.map(|b| decode_table_row(&t, &b)).transpose()?;
                        }
                    }
                    Locked::Skip => {
                        // Deleted concurrently: the source row no longer matches.
                        has_target = false;
                        for v in &mut row[off..=kpos] {
                            *v = Value::Null;
                        }
                        continue;
                    }
                    Locked::Newer(bytes) => {
                        // Updated concurrently: re-check the join and the WHEN clauses
                        // against the new version.
                        let mut fresh = decode_table_row(&t, &bytes)?;
                        if let Some(m) = member {
                            child_old = Some(fresh.clone());
                            fresh = project(&p.members[m].map, &fresh);
                        }
                        row.splice(off..off + n, fresh);
                        if has_source
                            && let Some(c) = &p.join_cond
                            && !eval_bool(c, &row, ctx)?
                        {
                            let mut source_only = row.clone();
                            for v in &mut source_only[off..=kpos] {
                                *v = Value::Null;
                            }
                            if p.actions.iter().any(|a| a.when == MergeWhen::NotMatchedBySource) {
                                // The target row goes on to the WHEN NOT MATCHED BY SOURCE
                                // actions, and the source row, as if it had never matched,
                                // to WHEN NOT MATCHED [BY TARGET] after it
                                // (mt_merge_pending_not_matched). EXPLAIN ANALYZE counts the
                                // join's row twice when there are both kinds of actions.
                                has_source = false;
                                self.pending = Some(source_only);
                                if p.actions.iter().any(|a| a.when == MergeWhen::NotMatchedByTarget) {
                                    // (InstrUpdateTupleCount on the outer plan: the join
                                    // node shown, under the projections over it.)
                                    let mut node: &crate::sql::plan::Plan = &p.source;
                                    loop {
                                        let key = node as *const crate::sql::plan::Plan as usize;
                                        ctx.count(key, |s| s.rows += 1);
                                        match node {
                                            crate::sql::plan::Plan::Project { input, .. } => node = input,
                                            _ => break,
                                        }
                                    }
                                }
                            } else {
                                has_target = false;
                                row = source_only;
                            }
                        }
                        continue;
                    }
                }
            }
            let old: Row = match &child_old {
                Some(r) => r.clone(),
                None => row[off..off + n].to_vec(),
            };
            let owner = match member {
                Some(m) => Owner::Member(m),
                None => Owner::Root,
            };
            // The rows as the target's (its transition tables).
            let as_target = |r: &Row| match member {
                Some(m) => project(&p.members[m].map, r),
                None => r.clone(),
            };
            match cmd {
                MergeCmd::Update {
                    sets,
                    rls_existing,
                    rls_new,
                } => {
                    check_target_rls(rls_existing, &old, ctx)?;
                    let mut new = old.clone();
                    super::dml::assign(&mut new, sets, &row, ctx)?;
                    if trigger::any(&rules.triggers, true, true, tt::UPDATE, ctx) {
                        let cols: Vec<usize> = sets.iter().map(|(c, _)| *c).collect();
                        match trigger::before_row(
                            &rules.triggers,
                            &t,
                            tt::UPDATE,
                            Some(&old),
                            new,
                            &cols,
                            ctx,
                        )? {
                            Some(r) => new = r,
                            None => return Ok(None),
                        }
                    }
                    // Changed while the new values were computed, or by a BEFORE trigger.
                    self_modified_check(&dkey, ctx)?;
                    // A row leaving its partition moves through the partitioned target.
                    if let (Some(routing), Some(m)) = (&p.route, member) {
                        for (i, g) in rules.generated.iter().enumerate() {
                            if let Some(e) = g {
                                new[i] = eval(e, &new, ctx)?;
                            }
                        }
                        if !partition_ok(rules, &new, ctx)? {
                            super::dml::check_row_rls(rules, rls_new, &new, ctx)?;
                            let mut routed = Vec::new();
                            let r = move_row(rules, routing, &dkey, &old, new, &mut routed, ctx);
                            self.ordered
                                .extend(routed.into_iter().map(|(k, a)| (Owner::Leaf(k), a)));
                            let Some((stored, table, key)) = r? else {
                                return Ok(None);
                            };
                            self.touched.insert(dkey.clone());
                            ctx.processed.set(ctx.processed.get() + 1);
                            // (MERGE's transition tables take a moved row as deleted and
                            // inserted.)
                            if self.trans {
                                self.captured.push(AfterRow {
                                    event: tt::DELETE,
                                    old: Some(as_target(&old)),
                                    ..Default::default()
                                });
                                self.captured.push(AfterRow {
                                    event: tt::INSERT,
                                    new: Some(as_target(&stored)),
                                    ..Default::default()
                                });
                            }
                            let stored = project(&p.members[m].map, &stored);
                            return self.returning(row, stored, &key, table, "UPDATE", ctx);
                        }
                    }
                    let stored = update_row_rls(rules, rls_new, &dkey, &old, new, ctx)?;
                    self.touched.insert(dkey.clone());
                    if self.trans {
                        self.captured.push(AfterRow {
                            event: tt::UPDATE,
                            old: Some(as_target(&old)),
                            new: Some(as_target(&stored)),
                            key: Vec::new(),
                        });
                    }
                    if trigger::any(&rules.triggers, true, false, tt::UPDATE, ctx) {
                        self.ordered.push((
                            owner,
                            AfterRow {
                                event: tt::UPDATE,
                                old: Some(old),
                                new: Some(stored.clone()),
                                key: super::dml::stored_key(&t, &stored, &dkey),
                            },
                        ));
                    }
                    ctx.processed.set(ctx.processed.get() + 1);
                    let key = if t.storage_pkey().is_some() {
                        data_key(&t, &stored, None)
                    } else {
                        dkey
                    };
                    let stored = match member {
                        Some(m) => project(&p.members[m].map, &stored),
                        None => stored,
                    };
                    return self.returning(row, stored, &key, t.oid, "UPDATE", ctx);
                }
                MergeCmd::Delete { rls_existing } => {
                    check_target_rls(rls_existing, &old, ctx)?;
                    if trigger::any(&rules.triggers, true, true, tt::DELETE, ctx)
                        && !trigger::before_delete(&rules.triggers, &t, &old, ctx)?
                    {
                        return Ok(None);
                    }
                    self_modified_check(&dkey, ctx)?;
                    delete_row(&t, &rules.indexes, &dkey, &old, ctx)?;
                    self.touched.insert(dkey.clone());
                    if self.trans {
                        self.captured.push(AfterRow {
                            event: tt::DELETE,
                            old: Some(as_target(&old)),
                            ..Default::default()
                        });
                    }
                    if trigger::any(&rules.triggers, true, false, tt::DELETE, ctx) {
                        self.ordered.push((
                            owner,
                            AfterRow {
                                event: tt::DELETE,
                                old: Some(old.clone()),
                                ..Default::default()
                            },
                        ));
                    }
                    ctx.processed.set(ctx.processed.get() + 1);
                    let old = match member {
                        Some(m) => project(&p.members[m].map, &old),
                        None => old,
                    };
                    return self.returning(row, old, &dkey, t.oid, "DELETE", ctx);
                }
                _ => unreachable!(),
            }
        }
    }

    /// RETURNING over the joined row with the target's columns replaced by `target`.
    fn returning(
        &self,
        mut row: Row,
        target: Row,
        key: &[u8],
        table: u32,
        action: &str,
        ctx: &ExecCtx,
    ) -> Result<Option<Row>> {
        let p = &self.plan;
        // EXPLAIN ANALYZE's "Tuples:" line, counted under the source's key.
        let i = match action {
            "INSERT" => 0,
            "UPDATE" => 1,
            _ => 2,
        };
        ctx.count(std::sync::Arc::as_ptr(&p.source) as usize, |s| s.merged[i] += 1);
        if p.returning.is_empty() {
            return Ok(None);
        }
        let off = p.target;
        let n = target.len();
        let sys = (p.sys && !key.is_empty()).then_some(off + n + 1);
        let old = (p.old_new.is_some() && action == "UPDATE")
            .then(|| super::dml::image(&row, off + n, sys).split_off(off));
        row.splice(off..off + n, target);
        if p.sys && !key.is_empty() {
            let xid = ctx.txn.borrow_mut().cur_xid();
            for (i, v) in sys_values(key, xid, 0, table).into_iter().enumerate() {
                row[off + n + 1 + i] = v;
            }
        }
        row.push(Value::Text(action.into()));
        let (old, new) = match action {
            _ if p.old_new.is_none() => (None, None),
            "INSERT" => (
                None,
                Some(super::dml::image(&row, off + n, sys).split_off(off)),
            ),
            "UPDATE" => (
                old,
                Some(super::dml::image(&row, off + n, sys).split_off(off)),
            ),
            _ => (
                Some(super::dml::image(&row, off + n, sys).split_off(off)),
                None,
            ),
        };
        let row = super::dml::with_old_new(row, p.old_new, old.as_deref(), new.as_deref());
        Ok(Some(eval_all(&p.returning, &row, ctx)?))
    }
}

/// A child's row in its parent's column order.
fn project(map: &[Option<usize>], child: &[Value]) -> Row {
    map.iter()
        .map(|m| m.and_then(|j| child.get(j).cloned()).unwrap_or(Value::Null))
        .collect()
}

/// Row-level security USING checks of the target row an UPDATE / DELETE action changes: a
/// row that fails is an error, not skipped.
fn check_target_rls(checks: &[RlsCheck], row: &[Value], ctx: &ExecCtx) -> Result<()> {
    for c in checks {
        if eval(&c.expr, row, ctx)? != Value::Bool(true) {
            let msg = match &c.policy {
                Some(p) => format!(
                    "target row violates row-level security policy \"{p}\" (USING expression) for table \"{}\"",
                    c.table
                ),
                None => format!(
                    "target row violates row-level security policy (USING expression) for table \"{}\"",
                    c.table
                ),
            };
            return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, msg));
        }
    }
    Ok(())
}

impl Node for MergeNode {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        if self.done {
            return Ok(None);
        }
        loop {
            ctx.check()?;
            if let Some(row) = self.pending.take() {
                if let Some(out) = self.merge_row(row, ctx)? {
                    return Ok(Some(out));
                }
                continue;
            }
            let Some(row) = self.source.next(ctx)? else {
                self.done = true;
                super::dml::check_statement_rechecks(ctx)?;
                trigger::batch(ctx, || self.finish(ctx))?;
                return Ok(None);
            };
            if let Some(out) = self.merge_row(row, ctx)? {
                return Ok(Some(out));
            }
        }
    }
}

impl MergeNode {
    /// The AFTER events: each table's row events in the order the rows changed, then the
    /// target's statement events (in the reverse order).
    fn finish(&mut self, ctx: &ExecCtx) -> Result<()> {
        let p = &self.plan;
        let events: Vec<u16> = self.events.iter().rev().copied().collect();
        let mut runs: Vec<trigger::Run> = Vec::new();
        let mut cur: Option<Owner> = None;
        for (o, a) in std::mem::take(&mut self.ordered) {
            if cur != Some(o) {
                cur = Some(o);
                runs.push(match o {
                    Owner::Root => trigger::Run {
                        trigs: &p.rules.triggers,
                        table: &p.rules.table,
                        set_cols: &self.update_cols,
                        rows: Vec::new(),
                        root: true,
                    },
                    Owner::Member(m) => trigger::Run {
                        trigs: &p.members[m].rules.triggers,
                        table: &p.members[m].rules.table,
                        set_cols: &p.members[m].update_cols,
                        rows: Vec::new(),
                        root: false,
                    },
                    Owner::Leaf(k) => {
                        let l = &p.route.as_ref().expect("a routed row").leaves[k];
                        trigger::Run {
                            trigs: &l.rules.triggers,
                            table: &l.rules.table,
                            set_cols: &[],
                            rows: Vec::new(),
                            root: false,
                        }
                    }
                });
            }
            runs.last_mut().expect("a run").rows.push(a);
        }
        trigger::fire_runs(
            &p.rules.triggers,
            &p.rules.table,
            &events,
            &self.update_cols,
            &self.captured,
            runs,
            ctx,
        )
    }
}

fn second_time() -> Error {
    Error::new(
        code::CARDINALITY_VIOLATION,
        "MERGE command cannot affect row a second time",
    )
    .hint("Ensure that not more than one source row matches any one target row.")
}

/// A target row this transaction changed after the statement's snapshot (TM_SelfModified):
/// by MERGE itself (or another part of its statement), or by a later command (a trigger's).
fn self_modified_check(dkey: &[u8], ctx: &ExecCtx) -> Result<()> {
    match super::dml::self_modified(dkey, ctx) {
        super::dml::SelfModified::No => Ok(()),
        super::dml::SelfModified::ByThis => Err(second_time()),
        super::dml::SelfModified::ByLater => {
            Err(super::dml::triggered_change("updated or deleted"))
        }
    }
}
