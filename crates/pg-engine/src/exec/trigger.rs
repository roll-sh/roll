//! Firing triggers around INSERT / UPDATE / DELETE / TRUNCATE.

use super::{Enr, ExecCtx, Row};
use crate::catalog::{Table, trigger_type as tt};
use crate::error::{Error, Result, code};
use crate::sql::expr::eval_bool;
use crate::sql::plan::BoundTrigger;
use crate::types::Value;
use std::rc::Rc;
use std::sync::Arc;

pub fn event_name(event: u16) -> &'static str {
    match event {
        tt::INSERT => "INSERT",
        tt::UPDATE => "UPDATE",
        tt::DELETE => "DELETE",
        _ => "TRUNCATE",
    }
}

/// Whether the trigger is enabled under session_replication_role: ENABLE (origin / local),
/// ENABLE REPLICA (replica) or ENABLE ALWAYS.
fn enabled(bt: &BoundTrigger, ctx: &ExecCtx) -> bool {
    match bt.trig.enabled {
        'D' => false,
        'A' => true,
        e => (e == 'R') == ctx.sess.settings.borrow().replica_role(),
    }
}

fn applies(bt: &BoundTrigger, row: bool, before: bool, event: u16, ctx: &ExecCtx) -> bool {
    let ty = bt.trig.tgtype;
    enabled(bt, ctx)
        && (ty & tt::ROW != 0) == row
        && (ty & tt::BEFORE != 0) == before
        && ty & tt::INSTEAD == 0
        && ty & event != 0
}

/// Whether any enabled trigger fires for this timing / level / event.
pub fn any(trigs: &[BoundTrigger], row: bool, before: bool, event: u16, ctx: &ExecCtx) -> bool {
    trigs.iter().any(|t| applies(t, row, before, event, ctx))
}

/// Whether rows of `event` must be queued: for AFTER ROW triggers or transition tables.
pub fn wants_after(trigs: &[BoundTrigger], event: u16, ctx: &ExecCtx) -> bool {
    any(trigs, true, false, event, ctx)
        || trigs.iter().any(|b| {
            enabled(b, ctx)
                && b.trig.tgtype & event != 0
                && (b.trig.old_table.is_some() || b.trig.new_table.is_some())
        })
}

/// Marks a queued event kept only for a statement's transition tables (rows a partition or
/// inheritance child changed, as the parent's): no row trigger fires for it.
pub const CAPTURE: u16 = 0x8000;

/// Marks the DELETE half of a row moving to another partition: the foreign keys referencing
/// the partition through its partitioned table act on the partitioned table's UPDATE instead.
pub const CROSS: u16 = 0x4000;

/// Whether a table's statement triggers for `event` have transition tables.
pub fn has_transition(trigs: &[BoundTrigger], event: u16) -> bool {
    trigs.iter().any(|b| {
        b.trig.tgtype & tt::ROW == 0
            && b.trig.tgtype & event != 0
            && (b.trig.old_table.is_some() || b.trig.new_table.is_some())
    })
}

/// A statement's transition tables for one event (live columns only).
#[derive(Default)]
pub struct Transition {
    old: Rc<Vec<Row>>,
    new: Rc<Vec<Row>>,
}

/// One table's transition table data for one event in a statement (AfterTriggersTableData):
/// its commands, and the referential actions they set off, add their rows to it, until a
/// trigger reading it fires; it is then closed, and rows changed later go into another
/// (whose BEFORE STATEMENT triggers fire again).
pub struct TransData {
    table: u32,
    event: u16,
    old: std::cell::RefCell<Vec<Row>>,
    new: std::cell::RefCell<Vec<Row>>,
    closed: std::cell::Cell<bool>,
}

impl TransData {
    /// The open data of table `t` for `event` in the statement, made if there is none.
    fn open(t: &Table, event: u16, ctx: &ExecCtx) -> Rc<TransData> {
        let mut all = ctx.trans_data.borrow_mut();
        if let Some(d) = all
            .iter()
            .rev()
            .find(|d| d.table == t.oid && d.event == event && !d.closed.get())
        {
            return d.clone();
        }
        let d = Rc::new(TransData {
            table: t.oid,
            event,
            old: Default::default(),
            new: Default::default(),
            closed: Default::default(),
        });
        all.push(d.clone());
        d
    }

    fn add(&self, tr: Transition) {
        self.old.borrow_mut().extend(tr.old.iter().cloned());
        self.new.borrow_mut().extend(tr.new.iter().cloned());
    }

    /// The rows as a trigger reads them; reading closes it.
    fn read(&self, close: bool, ctx: &ExecCtx) -> Transition {
        if close && !self.closed.replace(true) {
            ctx.stmt_fired.borrow_mut().remove(&(self.table, self.event));
        }
        Transition {
            old: Rc::new(self.old.borrow().clone()),
            new: Rc::new(self.new.borrow().clone()),
        }
    }
}

impl Transition {
    fn of(t: &Table, queue: &[AfterRow], event: u16) -> Transition {
        let live: Vec<usize> = t.live_columns().map(|(i, _)| i).collect();
        let project = |r: &Row| live.iter().map(|&i| r[i].clone()).collect::<Row>();
        let evs = queue
            .iter()
            .filter(|q| q.event & !(CAPTURE | CROSS) == event);
        Transition {
            old: Rc::new(
                evs.clone()
                    .filter_map(|q| q.old.as_ref().map(project))
                    .collect(),
            ),
            new: Rc::new(evs.filter_map(|q| q.new.as_ref().map(project)).collect()),
        }
    }
}

/// `UPDATE OF cols` triggers fire only when one of their columns is assigned.
fn columns_hit(bt: &BoundTrigger, event: u16, set_cols: &[usize]) -> bool {
    event != tt::UPDATE
        || bt.trig.columns.is_empty()
        || bt.trig.columns.iter().any(|c| set_cols.contains(c))
}

fn when_ok(
    bt: &BoundTrigger,
    t: &Table,
    old: Option<&Row>,
    new: Option<&Row>,
    ctx: &ExecCtx,
) -> Result<bool> {
    let Some(w) = &bt.when else { return Ok(true) };
    let n = t.columns.len();
    let mut r: Row = old.cloned().unwrap_or_else(|| vec![Value::Null; n]);
    r.extend(new.cloned().unwrap_or_else(|| vec![Value::Null; n]));
    eval_bool(w, &r, ctx)
}

fn call(
    bt: &BoundTrigger,
    t: &Table,
    before: bool,
    row: bool,
    event: u16,
    old: Option<Row>,
    new: Option<Row>,
    ctx: &ExecCtx,
) -> Result<Option<Row>> {
    call_with(bt, t, before, row, event, old, new, None, ctx)
}

#[allow(clippy::too_many_arguments)]
fn call_with(
    bt: &BoundTrigger,
    t: &Table,
    before: bool,
    row: bool,
    event: u16,
    old: Option<Row>,
    new: Option<Row>,
    trans: Option<&Transition>,
    ctx: &ExecCtx,
) -> Result<Option<Row>> {
    let schema = ctx.catalog().schema_name(t.schema).to_string();
    // Only this trigger's transition tables are visible to its function.
    let mut enrs = Vec::new();
    if let Some(tr) = trans {
        let cols: Vec<(String, crate::types::DataType)> = t
            .live_columns()
            .map(|(_, c)| (c.name.clone(), c.ty.clone()))
            .collect();
        if let Some(n) = &bt.trig.old_table {
            enrs.push(Rc::new(Enr {
                name: n.clone(),
                cols: cols.clone(),
                rows: tr.old.clone(),
            }));
        }
        if let Some(n) = &bt.trig.new_table {
            enrs.push(Rc::new(Enr {
                name: n.clone(),
                cols,
                rows: tr.new.clone(),
            }));
        }
    }
    let saved = std::mem::replace(&mut *ctx.enrs.borrow_mut(), enrs);
    let depth = {
        let mut u = ctx.sess.in_use.borrow_mut();
        u.push(t.oid);
        u.len() - 1
    };
    crate::scoped::checkpoint("session::in_use");
    let r = call_trigger_fn(bt, t, before, row, event, old, new, &schema, ctx);
    ctx.sess.in_use.borrow_mut().truncate(depth);
    *ctx.enrs.borrow_mut() = saved;
    r
}

#[allow(clippy::too_many_arguments)]
fn call_trigger_fn(
    bt: &BoundTrigger,
    t: &Table,
    before: bool,
    row: bool,
    event: u16,
    old: Option<Row>,
    new: Option<Row>,
    schema: &str,
    ctx: &ExecCtx,
) -> Result<Option<Row>> {
    // Triggers don't see virtual generated columns' values (BEFORE row triggers get the new
    // row before any generated column is computed: `before_row_in`).
    let hide = |r: Option<Row>, all: bool| {
        r.map(|mut r| {
            for (i, c) in t.columns.iter().enumerate() {
                if (c.virtual_gen || all && c.generated.is_some()) && i < r.len() {
                    r[i] = crate::types::Value::Null;
                }
            }
            r
        })
    };
    if bt.func.language == "internal" {
        if let Some(by_column) = match bt.func.name.as_str() {
            "tsvector_update_trigger" => Some(false),
            "tsvector_update_trigger_column" => Some(true),
            _ => None,
        } {
            let set_cols = UPDATED_COLS.with(|c| c.borrow().clone());
            return crate::sql::func::tsearch::update_trigger(
                by_column,
                t,
                &bt.trig.args,
                before,
                row,
                event,
                new,
                &set_cols,
                ctx,
            );
        }
        return builtin_trigger(&bt.func.name, before, row, event, old, new);
    }
    // A C trigger function (an extension's): its native code, with the TriggerData it reads.
    if bt.func.language == "c"
        && let Some(f) = bt.func.probin.as_deref().and_then(|bin| crate::ext::native::trigger(bin, &bt.func.body))
    {
        let set_cols = UPDATED_COLS.with(|c| c.borrow().clone());
        return f(
            crate::ext::native::TriggerData {
                name: &bt.trig.name,
                before,
                row,
                update: event & tt::UPDATE != 0,
                delete: event & tt::DELETE != 0,
                table: t,
                args: &bt.trig.args,
                old,
                new,
                updated_cols: &set_cols,
            },
            ctx,
        );
    }
    // (An UPDATE's BEFORE triggers each see them unset again; an INSERT's see what the
    // trigger before set.)
    let (old, new) = (hide(old, false), hide(new, before && row && event & tt::UPDATE != 0));
    crate::plpgsql::call_trigger(
        ctx,
        &bt.func,
        crate::plpgsql::TriggerCall {
            name: &bt.trig.name,
            when: if bt.trig.tgtype & tt::INSTEAD != 0 {
                "INSTEAD OF"
            } else if before {
                "BEFORE"
            } else {
                "AFTER"
            },
            level: if row { "ROW" } else { "STATEMENT" },
            op: event_name(event),
            table: t,
            schema,
            args: &bt.trig.args,
            new,
            old,
        },
    )
}

thread_local! {
    /// The columns the running UPDATE assigns, for C-style trigger functions that look at
    /// them (`tg_updatedcols`).
    static UPDATED_COLS: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Per-statement scratch back at rest (`scoped::reset_statement_scratch`).
pub(crate) fn reset_scratch() {
    UPDATED_COLS.with_borrow_mut(|c| c.clear());
    MOVING_ROW.set(false);
}

/// The flags not at rest (`scoped::unsettled_for_test`).
pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    if UPDATED_COLS.with_borrow(|c| !c.is_empty()) {
        out.push("trigger::UPDATED_COLS");
    }
    if MOVING_ROW.get() {
        out.push("trigger::MOVING_ROW");
    }
}

/// BEFORE ROW triggers for INSERT / UPDATE: each may change NEW; `None` skips the row.
pub fn before_row(
    trigs: &[BoundTrigger],
    t: &Table,
    event: u16,
    old: Option<&Row>,
    new: Row,
    set_cols: &[usize],
    ctx: &ExecCtx,
) -> Result<Option<Row>> {
    before_row_in(trigs, t, event, old, new, set_cols, None, ctx)
}

thread_local! {
    /// The next BEFORE ROW triggers get a row moving to another partition (see
    /// `moving_row`).
    static MOVING_ROW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f`, which inserts a row an UPDATE moves to another partition (ExecCrossPartitionUpdate):
/// the new partition's BEFORE INSERT triggers see its stored generated columns as computed
/// for the partition it left.
pub fn moving_row<R>(f: impl FnOnce() -> R) -> R {
    let _g = crate::scoped::set(&MOVING_ROW, true, "trigger::MOVING_ROW");
    f()
}

/// before_row for a partition's new rows: one of its partitioned table's triggers may not
/// move the row out of it (`fits` is its partition constraint).
#[allow(clippy::too_many_arguments)]
pub fn before_row_in(
    trigs: &[BoundTrigger],
    t: &Table,
    event: u16,
    old: Option<&Row>,
    new: Row,
    set_cols: &[usize],
    fits: Option<&dyn Fn(&Row) -> Result<bool>>,
    ctx: &ExecCtx,
) -> Result<Option<Row>> {
    let mut new = new;
    // The stored generated columns aren't computed yet: NULL to the first trigger; what one
    // trigger puts there the next sees (and the computed value replaces). (A row moving to
    // another partition has them as the partition it left computed them.)
    let moving = MOVING_ROW.with(|m| m.replace(false));
    if !moving
        && trigs
            .iter()
            .any(|b| applies(b, true, true, event, ctx) && columns_hit(b, event, set_cols))
    {
        for (i, c) in t.columns.iter().enumerate() {
            if c.generated.is_some() && !c.virtual_gen && i < new.len() {
                new[i] = crate::types::Value::Null;
            }
        }
    }
    for bt in trigs
        .iter()
        .filter(|b| applies(b, true, true, event, ctx) && columns_hit(b, event, set_cols))
    {
        if !when_ok(bt, t, old, Some(&new), ctx)? {
            continue;
        }
        let r = {
            let _g = crate::scoped::set_ref(&UPDATED_COLS, set_cols.to_vec(), "trigger::UPDATED_COLS");
            call(bt, t, true, true, event, old.cloned(), Some(new), ctx)
        };
        match r? {
            Some(r) => new = r,
            None => return Ok(None),
        }
        if let Some(fits) = fits
            && bt.trig.parent != 0
            && !fits(&new)?
        {
            let schema = ctx.catalog().schema_name(t.schema).to_string();
            return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "moving row to another partition during a BEFORE FOR EACH ROW trigger is not supported")
                .detail(format!("Before executing trigger \"{}\", the row was to be in partition \"{schema}.{}\".", bt.trig.name, t.name)));
        }
    }
    Ok(Some(new))
}

/// BEFORE ROW DELETE triggers: false skips the delete.
pub fn before_delete(trigs: &[BoundTrigger], t: &Table, old: &Row, ctx: &ExecCtx) -> Result<bool> {
    for bt in trigs
        .iter()
        .filter(|b| applies(b, true, true, tt::DELETE, ctx))
    {
        if !when_ok(bt, t, Some(old), None, ctx)? {
            continue;
        }
        if call(bt, t, true, true, tt::DELETE, Some(old.clone()), None, ctx)?.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// INSTEAD OF row triggers on a view: each may change the row; `None` means nothing was done.
pub fn instead(
    trigs: &[BoundTrigger],
    t: &Table,
    event: u16,
    old: Option<&Row>,
    new: Option<Row>,
    ctx: &ExecCtx,
) -> Result<Option<Row>> {
    let mut new = new;
    let mut out = None;
    for bt in trigs.iter().filter(|b| {
        enabled(b, ctx) && b.trig.tgtype & tt::INSTEAD != 0 && b.trig.tgtype & event != 0
    }) {
        match call(bt, t, false, true, event, old.cloned(), new.clone(), ctx)? {
            Some(r) => {
                if event != tt::DELETE {
                    new = Some(r.clone());
                }
                out = Some(r);
            }
            None => return Ok(None),
        }
    }
    Ok(out.map(|r| {
        if event == tt::DELETE {
            old.cloned().unwrap_or(r)
        } else {
            r
        }
    }))
}

/// An AFTER ROW event, queued until the statement finishes.
#[derive(Clone, Default)]
pub struct AfterRow {
    pub event: u16,
    pub old: Option<Row>,
    pub new: Option<Row>,
    /// The new row's data key (empty: unknown): a foreign key check skips a row gone since.
    pub key: Vec<u8>,
}

/// Fire queued AFTER ROW events, then AFTER STATEMENT triggers for `events`.
pub fn finish(
    trigs: &[BoundTrigger],
    t: &Table,
    queue: Vec<AfterRow>,
    events: &[u16],
    set_cols: &[usize],
    ctx: &ExecCtx,
) -> Result<()> {
    fire_after(trigs, t, queue, events, set_cols, true, true, ctx)
}

/// A DML node's queued events for `fire_after`: kept for its statement triggers' transition
/// tables if there are any.
pub fn take_queue(trigs: &[BoundTrigger], queue: &mut Vec<AfterRow>) -> Vec<AfterRow> {
    if trigs
        .iter()
        .any(|b| b.trig.old_table.is_some() || b.trig.new_table.is_some())
    {
        queue.clone()
    } else {
        std::mem::take(queue)
    }
}

/// Events waiting in a statement's AFTER trigger queue: a table's row events, or its
/// statement events.
pub struct Pending {
    trigs: Rc<Vec<BoundTrigger>>,
    table: Arc<Table>,
    rows: Vec<AfterRow>,
    events: Vec<u16>,
    set_cols: Vec<usize>,
    /// Transition table data per event.
    trans: Rc<Vec<(u16, Rc<TransData>)>>,
    /// Per row event: the triggers whose WHEN condition, checked as it was queued, failed.
    when_failed: Vec<Vec<u32>>,
}

/// AFTER triggers: the queued row events (`rows`), then the statement's (`stmt`), whose
/// transition tables hold the queued rows. They join the statement's queue of AFTER events
/// (AfterTriggerSaveEvent), run now unless it is already running: referential actions, which
/// fire from it, add the events of the rows they change at its end. A table's statement
/// events fire once there, after all of them (cancel_prior_stmt_triggers).
#[allow(clippy::too_many_arguments)]
pub fn fire_after(
    trigs: &[BoundTrigger],
    t: &Table,
    queue: Vec<AfterRow>,
    events: &[u16],
    set_cols: &[usize],
    rows: bool,
    stmt: bool,
    ctx: &ExecCtx,
) -> Result<()> {
    let has_trans = trigs
        .iter()
        .any(|b| b.trig.old_table.is_some() || b.trig.new_table.is_some());
    let wanted = |row: bool| {
        trigs.iter().any(|b| {
            (b.trig.tgtype & tt::ROW != 0) == row && b.trig.tgtype & (tt::BEFORE | tt::INSTEAD) == 0
        })
    };
    // A command fires its row events and its statement events apart, over the same rows:
    // they join the transition data once (with the row events, if there are any).
    let adds = if rows { wanted(true) || stmt } else { !wanted(true) };
    let rows = rows && !queue.is_empty() && wanted(true);
    let stmt = stmt && !events.is_empty() && wanted(false);
    if !rows && !stmt {
        return run_queue(ctx);
    }
    let mut trans: Vec<(u16, Rc<TransData>)> = Vec::new();
    if has_trans {
        for &e in events {
            let d = TransData::open(t, e, ctx);
            if adds {
                d.add(Transition::of(t, &queue, e));
            }
            trans.push((e, d));
        }
    }
    queue_after(trigs, t, queue, events, set_cols, rows, stmt, trans, ctx)?;
    run_queue(ctx)
}

/// One table's AFTER ROW events among a statement's, in the order its rows changed.
pub struct Run<'a> {
    pub trigs: &'a [BoundTrigger],
    pub table: &'a Table,
    /// The columns its UPDATE assigns (for UPDATE OF triggers).
    pub set_cols: &'a [usize],
    pub rows: Vec<AfterRow>,
    /// The table the statement names (its row triggers read the statement's transition
    /// tables).
    pub root: bool,
}

/// The AFTER events of a statement that changed rows of several tables (an inheritance
/// tree's members, the partitions rows moved to) through the one it names, `root`: the row
/// events in the order the rows changed (runs of one table's), then `root`'s statement events
/// for `events`. `root`'s transition tables hold `captured` (every row changed, as its rows),
/// complete before any trigger reads them. They join the statement's queue (see `fire_after`).
#[allow(clippy::too_many_arguments)]
pub fn fire_runs(
    root_trigs: &[BoundTrigger],
    root: &Table,
    events: &[u16],
    set_cols: &[usize],
    captured: &[AfterRow],
    runs: Vec<Run>,
    ctx: &ExecCtx,
) -> Result<()> {
    let has_trans = root_trigs.iter().any(|b| {
        b.trig.tgtype & events.iter().fold(0, |a, e| a | e) != 0
            && (b.trig.old_table.is_some() || b.trig.new_table.is_some())
    });
    let mut trans: Vec<(u16, Rc<TransData>)> = Vec::new();
    if has_trans {
        for &e in events {
            let d = TransData::open(root, e, ctx);
            d.add(Transition::of(root, captured, e));
            trans.push((e, d));
        }
    }
    let after_row = |trigs: &[BoundTrigger]| {
        trigs.iter().any(|b| {
            b.trig.tgtype & tt::ROW != 0 && b.trig.tgtype & (tt::BEFORE | tt::INSTEAD) == 0
        })
    };
    for r in runs {
        if r.rows.is_empty() || !after_row(r.trigs) {
            continue;
        }
        let t = if r.root { trans.clone() } else { Vec::new() };
        queue_after(r.trigs, r.table, r.rows, &[], r.set_cols, true, false, t, ctx)?;
    }
    let stmt = !events.is_empty()
        && root_trigs.iter().any(|b| {
            b.trig.tgtype & tt::ROW == 0 && b.trig.tgtype & (tt::BEFORE | tt::INSTEAD) == 0
        });
    if stmt {
        queue_after(root_trigs, root, Vec::new(), events, set_cols, false, true, trans, ctx)?;
    }
    run_queue(ctx)
}

/// A command's row events were queued as each row went in (AfterTriggerSaveEvent), so
/// before the statement events of the commands it read rows from (a data-modifying CTE that
/// ended under it): those queued from `before` on that are row events move ahead of the
/// first statement event queued since the command started (`mark`).
pub fn hoist_row_events(ctx: &ExecCtx, mark: usize, before: usize) {
    let mut q = ctx.after_queue.borrow_mut();
    if before > q.len() || mark >= before {
        return;
    }
    let Some(at) = (mark..before).find(|&i| !q[i].events.is_empty()) else {
        return;
    };
    let added: Vec<Pending> = q.drain(before..).collect();
    let (rows, stmts): (Vec<Pending>, Vec<Pending>) =
        added.into_iter().partition(|p| p.events.is_empty());
    for (k, p) in rows.into_iter().enumerate() {
        q.insert(at + k, p);
    }
    q.extend(stmts);
}

/// Add events to the statement's AFTER trigger queue (see `fire_after`) without running it.
#[allow(clippy::too_many_arguments)]
pub fn queue_after(
    trigs: &[BoundTrigger],
    t: &Table,
    queue: Vec<AfterRow>,
    events: &[u16],
    set_cols: &[usize],
    rows: bool,
    stmt: bool,
    trans: Vec<(u16, Rc<TransData>)>,
    ctx: &ExecCtx,
) -> Result<()> {
    // AfterTriggerSaveEvent: a row trigger's WHEN condition is checked as the event is
    // queued (not when it fires).
    let mut when_failed = Vec::new();
    if rows {
        for ev in &queue {
            let mut failed = Vec::new();
            if ev.event & CAPTURE == 0 {
                let event = ev.event & !CROSS;
                for bt in trigs.iter().filter(|b| {
                    b.ri.is_none()
                        && b.when.is_some()
                        && applies(b, true, false, event, ctx)
                        && columns_hit(b, event, set_cols)
                }) {
                    if !when_ok(bt, t, ev.old.as_ref(), ev.new.as_ref(), ctx)? {
                        failed.push(bt.trig.oid);
                    }
                }
            }
            when_failed.push(failed);
        }
    }
    let trans = Rc::new(trans);
    let trigs = Rc::new(trigs.to_vec());
    let table = Arc::new(t.clone());
    {
        let mut q = ctx.after_queue.borrow_mut();
        if rows {
            q.push_back(Pending {
                trigs: trigs.clone(),
                table: table.clone(),
                rows: queue,
                events: Vec::new(),
                set_cols: set_cols.to_vec(),
                trans: trans.clone(),
                when_failed,
            });
        }
        if stmt {
            // cancel_prior_stmt_triggers: an earlier queued firing of these statement
            // triggers over the same transition data gives way to this one, at the end.
            let same = |p: &Pending| {
                p.trans.len() == trans.len()
                    && p.trans
                        .iter()
                        .zip(trans.iter())
                        .all(|(a, b)| a.0 == b.0 && Rc::ptr_eq(&a.1, &b.1))
            };
            if let Some(i) = q
                .iter()
                .position(|p| p.table.oid == t.oid && p.events == events && same(p))
            {
                q.remove(i);
            }
            q.push_back(Pending {
                trigs,
                table,
                rows: Vec::new(),
                events: events.to_vec(),
                set_cols: set_cols.to_vec(),
                trans,
                when_failed: Vec::new(),
            });
        }
    }
    Ok(())
}

/// Run `f`, which queues AFTER events, then fire them all in queue order (unless the queue
/// is running already): a statement's row events, then its statement events, before the
/// events of what its referential actions changed.
pub fn batch(ctx: &ExecCtx, f: impl FnOnce() -> Result<()>) -> Result<()> {
    if ctx.after_running.get() {
        return f();
    }
    ctx.after_running.set(true);
    let r = f();
    ctx.after_running.set(false);
    if r.is_err() {
        ctx.after_queue.borrow_mut().clear();
        return r;
    }
    run_queue(ctx)
}

/// Run the statement's queued AFTER events, unless they are being run already.
pub fn run_queue(ctx: &ExecCtx) -> Result<()> {
    if ctx.after_running.get() || ctx.after_queue.borrow().is_empty() {
        return Ok(());
    }
    ctx.after_running.set(true);
    let r = (|| {
        loop {
            let Some(p) = ctx.after_queue.borrow_mut().pop_front() else {
                return Ok(());
            };
            fire_pending(&p, ctx)?;
        }
    })();
    ctx.after_running.set(false);
    if r.is_err() {
        ctx.after_queue.borrow_mut().clear();
    }
    r
}

fn fire_pending(p: &Pending, ctx: &ExecCtx) -> Result<()> {
    let (trigs, t, set_cols) = (&p.trigs, &*p.table, &p.set_cols[..]);
    // (A trigger with transition tables reading them closes them.)
    let trans_of = |e: u16, bt: &BoundTrigger| {
        p.trans.iter().find(|(x, _)| *x == e).map(|(_, d)| {
            d.read(bt.trig.old_table.is_some() || bt.trig.new_table.is_some(), ctx)
        })
    };
    for (i, ev) in p.rows.iter().enumerate().filter(|(_, q)| q.event & CAPTURE == 0) {
        let event = ev.event & !CROSS;
        let failed = p.when_failed.get(i);
        for bt in trigs
            .iter()
            .filter(|b| applies(b, true, false, event, ctx) && columns_hit(b, event, set_cols))
        {
            if let Some(ri) = bt.ri {
                if ev.event & CROSS != 0 && bt.trig.parent != 0 {
                    continue;
                }
                let Some(fk) = super::dml::RiCon::of(&bt.trig, ri, ctx) else {
                    continue;
                };
                if !fk.required(event, ev, ctx) {
                    continue;
                }
                let x = &bt.trig;
                if x.deferrable
                    && ctx
                        .txn
                        .borrow()
                        .is_deferred(ri.0, (x.deferrable, x.initdeferred))
                {
                    let d = crate::txn::DeferredTrigger {
                        trigger: x.oid,
                        table: t.oid,
                        constraint: ri.0,
                        key: ev.key.clone(),
                        event,
                        old: ev.old.clone(),
                        new: ev.new.clone(),
                        role: 0,
                        set_cols: set_cols.to_vec(),
                    };
                    ctx.txn.borrow_mut().deferred_triggers.push(d);
                    continue;
                }
                fk.fire(t, event, ev, ctx)?;
                continue;
            }
            if failed.is_some_and(|f| f.contains(&bt.trig.oid)) {
                continue;
            }
            let x = &bt.trig;
            if x.constraint
                && ctx
                    .txn
                    .borrow()
                    .is_deferred(x.oid, (x.deferrable, x.initdeferred))
            {
                if ctx.sess.security_restricted() {
                    return Err(Error::new(
                        crate::error::code::INSUFFICIENT_PRIVILEGE,
                        "cannot fire deferred trigger within security-restricted operation",
                    ));
                }
                let d = crate::txn::DeferredTrigger {
                    trigger: x.oid,
                    table: t.oid,
                    constraint: x.oid,
                    key: Vec::new(),
                    event,
                    old: ev.old.clone(),
                    new: ev.new.clone(),
                    role: ctx.sess.current_role(&ctx.catalog()),
                    set_cols: set_cols.to_vec(),
                };
                ctx.txn.borrow_mut().deferred_triggers.push(d);
                continue;
            }
            // (The rows' tuple slots pin their pages while the trigger runs.)
            let pins = super::sizing::trigger_pin(t.oid, &ev.key, event & tt::UPDATE != 0, ctx);
            let r = {
                let _g = crate::scoped::set_ref(&UPDATED_COLS, set_cols.to_vec(), "trigger::UPDATED_COLS");
                call_with(
                    bt,
                    t,
                    false,
                    true,
                    event,
                    ev.old.clone(),
                    ev.new.clone(),
                    trans_of(event, bt).as_ref(),
                    ctx,
                )
            };
            super::sizing::trigger_unpin(t.oid, &pins, ctx);
            r?;
        }
    }
    for &e in &p.events {
        for bt in trigs
            .iter()
            .filter(|b| applies(b, false, false, e, ctx) && columns_hit(b, e, set_cols))
        {
            call_with(bt, t, false, false, e, None, None, trans_of(e, bt).as_ref(), ctx)?;
        }
    }
    Ok(())
}

/// Fire deferred constraint triggers (all, or those of the constraints in `only`), removing
/// them from the queue.
pub fn fire_deferred(ctx: &ExecCtx, only: Option<&std::collections::HashSet<u32>>) -> Result<()> {
    let mut bound: std::collections::HashMap<u32, (Arc<Table>, Rc<Vec<BoundTrigger>>)> =
        std::collections::HashMap::new();
    loop {
        let pending: Vec<crate::txn::DeferredTrigger> = {
            let mut txn = ctx.txn.borrow_mut();
            let (now, keep) = std::mem::take(&mut txn.deferred_triggers)
                .into_iter()
                .partition(|d| only.is_none_or(|o| o.contains(&d.constraint)));
            txn.deferred_triggers = keep;
            now
        };
        if pending.is_empty() {
            return Ok(());
        }
        let cat = ctx.catalog();
        for d in pending {
            if let std::collections::hash_map::Entry::Vacant(e) = bound.entry(d.table) {
                let Some(t) = cat.tables.get(&d.table).cloned() else {
                    continue;
                };
                let trigs = crate::sql::binder::Binder::new(ctx, Vec::new()).bound_triggers(&t)?;
                e.insert((t, Rc::new(trigs)));
            }
            let (t, trigs) = &bound[&d.table];
            let Some(bt) = trigs.iter().find(|b| b.trig.oid == d.trigger) else {
                continue;
            };
            if let Some(ri) = bt.ri {
                let Some(fk) = super::dml::RiCon::of(&bt.trig, ri, ctx) else {
                    continue;
                };
                let ev = AfterRow {
                    event: d.event,
                    old: d.old,
                    new: d.new,
                    key: d.key,
                };
                fk.fire(t, d.event, &ev, ctx)?;
                continue;
            }
            // (It runs as the role that queued it.)
            ctx.sess.definer.borrow_mut().push(d.role);
            let r = {
                let _g = crate::scoped::set_ref(&UPDATED_COLS, d.set_cols, "trigger::UPDATED_COLS");
                call(bt, t, false, true, d.event, d.old, d.new, ctx)
            };
            ctx.sess.definer.borrow_mut().pop();
            r?;
        }
        // The AFTER events of rows they change.
        run_queue(ctx)?;
    }
}

/// pg_proc oids of the referential integrity trigger functions.
pub mod ri_fn {
    pub const CHECK_INS: u32 = 1644;
    pub const CHECK_UPD: u32 = 1645;
    pub const CASCADE_DEL: u32 = 1646;
    pub const CASCADE_UPD: u32 = 1647;
    pub const RESTRICT_DEL: u32 = 1648;
    pub const RESTRICT_UPD: u32 = 1649;
    pub const SETNULL_DEL: u32 = 1650;
    pub const SETNULL_UPD: u32 = 1651;
    pub const SETDEFAULT_DEL: u32 = 1652;
    pub const SETDEFAULT_UPD: u32 = 1653;
    pub const NOACTION_DEL: u32 = 1654;
    pub const NOACTION_UPD: u32 = 1655;

    /// A referencing table's check (not a referenced table's action).
    pub fn is_check(oid: u32) -> bool {
        matches!(oid, CHECK_INS | CHECK_UPD)
    }

    pub fn name(oid: u32) -> &'static str {
        match oid {
            CHECK_INS => "RI_FKey_check_ins",
            CHECK_UPD => "RI_FKey_check_upd",
            CASCADE_DEL => "RI_FKey_cascade_del",
            CASCADE_UPD => "RI_FKey_cascade_upd",
            RESTRICT_DEL => "RI_FKey_restrict_del",
            RESTRICT_UPD => "RI_FKey_restrict_upd",
            SETNULL_DEL => "RI_FKey_setnull_del",
            SETNULL_UPD => "RI_FKey_setnull_upd",
            SETDEFAULT_DEL => "RI_FKey_setdefault_del",
            SETDEFAULT_UPD => "RI_FKey_setdefault_upd",
            NOACTION_DEL => "RI_FKey_noaction_del",
            _ => "RI_FKey_noaction_upd",
        }
    }
}

/// Tables with triggers (user ones and foreign keys' internal ones); `sticky`: also those
/// whose triggers were dropped since the last VACUUM (relhastriggers).
pub fn triggered_tables(
    cat: &crate::catalog::Catalog,
    sticky: bool,
) -> std::collections::HashSet<u32> {
    let mut out: std::collections::HashSet<u32> = cat.triggers.values().map(|t| t.table).collect();
    // (Kept by the tables map as tables change: `relindex::TableIx::trigger_marks`.)
    out.extend(cat.tables.ix().trigger_marks.keys());
    debug_assert_eq!(
        internal_trigger_tables(cat).into_iter().collect::<std::collections::BTreeSet<u32>>(),
        internal_triggers(cat).into_iter().map(|r| r.trig.table).collect(),
        "internal_trigger_tables must name internal_triggers' tables"
    );
    debug_assert_eq!(
        internal_trigger_tables(cat).into_iter().collect::<std::collections::BTreeSet<u32>>(),
        cat.tables.ix().trigger_marks.keys().collect(),
        "the tables map's trigger marks must be internal_trigger_tables'"
    );
    if sticky {
        out.extend(cat.had_triggers.iter().copied());
    }
    out
}

/// The tables `internal_triggers` puts triggers on, without making the triggers: every DDL
/// statement asks (`triggered_tables`).
fn internal_trigger_tables(cat: &crate::catalog::Catalog) -> Vec<u32> {
    let mut out = Vec::new();
    for t in cat.tables.values() {
        table_internal_triggers(t, &mut out);
    }
    for t in cat.tables.values() {
        for con in &t.constraints {
            if con.kind.index().is_some() && con.deferrable {
                out.push(t.oid);
            }
        }
    }
    out
}

/// The tables table `t`'s constraints put internal triggers on (its foreign keys': itself,
/// the referenced tables; its deferrable keys': itself), as `internal_trigger_tables` finds
/// them.
pub(crate) fn table_internal_triggers(t: &crate::catalog::Table, out: &mut Vec<u32>) {
    use crate::catalog::{Catalog, ConstraintKind};
    for con in &t.constraints {
        if let ConstraintKind::ForeignKey { ref_table, .. } = &con.kind {
            if !con.enforced {
                continue;
            }
            let derived = Catalog::fk_is_derived(t, con);
            // (As `ri_triggers`: a clone's action triggers are its parent key's.)
            let clone = con.parent != 0 && !derived;
            if !clone {
                out.push(*ref_table);
            }
            if !derived {
                out.push(t.oid);
            }
        }
    }
}

/// Whether table `x` has triggers in `cat` (as `triggered_tables(cat, false)` has them).
fn has_triggers(cat: &crate::catalog::Catalog, x: u32) -> bool {
    cat.tables.ix().trigger_marks.contains(x) || cat.triggers.values().any(|t| t.table == x)
}

/// Whether `triggered_tables(cat, sticky)` has table `x`.
pub fn table_has_triggers(cat: &crate::catalog::Catalog, x: u32, sticky: bool) -> bool {
    has_triggers(cat, x) || (sticky && cat.had_triggers.contains(&x))
}

/// Tables of `before` that had triggers and have none in `after` (`triggered_tables(_,
/// false)`), when only the tables `changed` names (and the triggers) differ between them:
/// what keeps relhastriggers set until VACUUM.
pub fn lost_triggers(
    before: &crate::catalog::Catalog,
    after: &crate::catalog::Catalog,
    changed: &std::collections::BTreeSet<u32>,
) -> Vec<u32> {
    // Candidates: the changed tables, the tables their foreign keys (before and after)
    // reference, and the tables of triggers that changed.
    let mut cand: std::collections::BTreeSet<u32> = changed.clone();
    let mut marks = Vec::new();
    for o in changed {
        for c in [before, after] {
            if let Some(t) = c.tables.get(o) {
                table_internal_triggers(t, &mut marks);
            }
        }
    }
    cand.extend(marks);
    for (oid, x) in &before.triggers {
        if after.triggers.get(oid).is_none_or(|y| !Arc::ptr_eq(x, y)) {
            cand.insert(x.table);
        }
    }
    for (oid, y) in &after.triggers {
        if !before.triggers.contains_key(oid) {
            cand.insert(y.table);
        }
    }
    cand.into_iter()
        .filter(|t| after.tables.contains_key(t))
        .filter(|t| has_triggers(before, *t) && !has_triggers(after, *t))
        .collect()
}

/// A foreign key's internal trigger, as pg_trigger shows it.
pub struct RiTrigger {
    pub trig: crate::catalog::Trigger,
    /// The foreign key, and the (referencing) table that has it.
    pub constraint: u32,
    pub fk_table: u32,
    /// The table at the other end (tgconstrrelid), and the referenced unique index
    /// (tgconstrindid).
    pub other: u32,
    pub index: u32,
}

/// The internal triggers foreign keys are enforced by (of the table `only`, or all):
/// RI_ConstraintTrigger_a_<oid> on the referenced table for its DELETE and UPDATE action, and
/// RI_ConstraintTrigger_c_<oid> on the referencing table checking its INSERT and UPDATE. Their
/// oids follow their constraint's. A key referencing a partitioned table also has (derived)
/// constraints acting for its partitions, and one of a partitioned table is cloned to its
/// partitions, which check their own rows; NOT ENFORCED keys have none.
pub fn ri_triggers(cat: &crate::catalog::Catalog, only: Option<u32>) -> Vec<RiTrigger> {
    match only {
        None => ri_triggers_from(cat, cat.tables.values(), None),
        // (Made from the constraints of the table itself and of the tables referencing it:
        // the tables map's index of foreign key referencers.)
        Some(o) => {
            let mut from: std::collections::BTreeSet<u32> = (cat.tables.ix().fk_refs.get(&o))
                .into_iter()
                .flat_map(|c| c.keys())
                .collect();
            from.insert(o);
            let out = ri_triggers_from(cat, from.iter().filter_map(|t| cat.tables.get(t)), only);
            #[cfg(debug_assertions)]
            {
                let all = ri_triggers_from(cat, cat.tables.values(), only);
                debug_assert!(
                    all.iter().map(|r| (r.trig.oid, r.trig.table)).eq(out.iter().map(|r| (r.trig.oid, r.trig.table))),
                    "a table's foreign key triggers"
                );
            }
            out
        }
    }
}

/// `ri_triggers` made from the constraints of `tables` (in oid order).
fn ri_triggers_from<'a>(
    cat: &crate::catalog::Catalog,
    tables: impl Iterator<Item = &'a Arc<crate::catalog::Table>>,
    only: Option<u32>,
) -> Vec<RiTrigger> {
    use crate::catalog::{Catalog, ConstraintKind, FkAction};
    let mut out = Vec::new();
    // A constraint by oid: (ri_check of the first table's with it, whether one is a clone),
    // through the tables map's index of constraint owners.
    let by_oid = |oid: u32| -> Option<(u32, bool)> {
        let mut found: Option<(u32, bool)> = None;
        for o in cat.tables.ix().con_owner.get(&oid)?.keys() {
            let Some(t) = cat.tables.get(&o) else { continue };
            for c in t.constraints.iter().filter(|c| c.oid == oid) {
                let e = found.get_or_insert((c.ri_check, false));
                e.1 |= Catalog::fk_is_clone(t, c);
            }
        }
        found
    };
    // A key a partitioned table's key was cloned to: its check triggers come first.
    let is_clone = |oid: u32| by_oid(oid).is_some_and(|e| e.1);
    let check_base = |oid: u32| by_oid(oid).map_or(0, |e| e.0);
    for t in tables {
        for con in &t.constraints {
            let ConstraintKind::ForeignKey {
                ref_table,
                ref_columns,
                on_delete,
                on_update,
                ..
            } = &con.kind
            else {
                continue;
            };
            if !con.enforced {
                continue;
            }
            let derived = Catalog::fk_is_derived(t, con);
            let clone = con.parent != 0 && !derived;
            let deferral = con.deferral();
            let mut add = |oid: u32, table: u32, func: u32, ev: u16, parent: u32, defer: bool| {
                let kind = if ri_fn::is_check(func) { 'c' } else { 'a' };
                out.push(RiTrigger {
                    trig: crate::catalog::Trigger {
                        oid,
                        name: format!("RI_ConstraintTrigger_{kind}_{oid}"),
                        table,
                        function: func,
                        tgtype: tt::ROW | ev,
                        columns: Vec::new(),
                        when: None,
                        args: Vec::new(),
                        enabled: con
                            .ri_enabled
                            .iter()
                            .find(|e| e.0 == oid)
                            .map_or('O', |e| e.1),
                        old_table: None,
                        new_table: None,
                        constraint: true,
                        deferrable: defer && deferral.0,
                        initdeferred: defer && deferral.1,
                        parent,
                        con_oid: 0,
                        constr_rel: 0,
                    },
                    constraint: con.oid,
                    fk_table: t.oid,
                    other: if kind == 'c' { *ref_table } else { t.oid },
                    index: crate::catalog::depend::fk_index(cat, *ref_table, ref_columns)
                        .unwrap_or(0),
                });
            };
            if !clone && only.is_none_or(|o| o == *ref_table) {
                let parent = |k: u32| if derived { con.parent + k } else { 0 };
                let del = match on_delete {
                    FkAction::NoAction => ri_fn::NOACTION_DEL,
                    FkAction::Restrict => ri_fn::RESTRICT_DEL,
                    FkAction::Cascade => ri_fn::CASCADE_DEL,
                    FkAction::SetNull => ri_fn::SETNULL_DEL,
                    FkAction::SetDefault => ri_fn::SETDEFAULT_DEL,
                };
                let upd = match on_update {
                    FkAction::NoAction => ri_fn::NOACTION_UPD,
                    FkAction::Restrict => ri_fn::RESTRICT_UPD,
                    FkAction::Cascade => ri_fn::CASCADE_UPD,
                    FkAction::SetNull => ri_fn::SETNULL_UPD,
                    FkAction::SetDefault => ri_fn::SETDEFAULT_UPD,
                };
                // Only NO ACTION is deferrable.
                let (d, u) = (
                    *on_delete == FkAction::NoAction,
                    *on_update == FkAction::NoAction,
                );
                add(con.oid + 1, *ref_table, del, tt::DELETE, parent(1), d);
                add(con.oid + 2, *ref_table, upd, tt::UPDATE, parent(2), u);
            }
            if !derived && only.is_none_or(|o| o == t.oid) {
                // A clone's check triggers follow it; a key's, its action triggers (or, for
                // one referencing a partitioned table, its derived constraints).
                let at = |c: u32, clone: bool| {
                    if clone {
                        c
                    } else {
                        match check_base(c) {
                            0 => c + 2,
                            b => b,
                        }
                    }
                };
                let parent = |k: u32| {
                    if clone {
                        at(con.parent, is_clone(con.parent)) + k
                    } else {
                        0
                    }
                };
                let base = match con.ri_check {
                    0 => at(con.oid, clone),
                    b => b,
                };
                add(
                    base + 1,
                    t.oid,
                    ri_fn::CHECK_INS,
                    tt::INSERT,
                    parent(1),
                    true,
                );
                add(
                    base + 2,
                    t.oid,
                    ri_fn::CHECK_UPD,
                    tt::UPDATE,
                    parent(2),
                    true,
                );
            }
        }
    }
    out
}

/// The internal triggers of deferrable primary key, unique and exclusion constraints
/// (PK_ConstraintTrigger_<oid> / Unique_ConstraintTrigger_<oid>, running unique_key_recheck
/// after INSERT and UPDATE), whose oids follow their constraint's (index_constraint_create).
/// The index enforces the constraint here, so they're only seen in the catalogs.
pub fn recheck_triggers(cat: &crate::catalog::Catalog) -> Vec<RiTrigger> {
    recheck_triggers_from(cat.tables.values())
}

pub fn recheck_triggers_from<'a>(tables: impl Iterator<Item = &'a Arc<crate::catalog::Table>>) -> Vec<RiTrigger> {
    use crate::catalog::{ConstraintKind, trigger_type as tt};
    let mut out = Vec::new();
    for t in tables {
        for con in &t.constraints {
            let Some(index) = con.kind.index() else {
                continue;
            };
            if !con.deferrable {
                continue;
            }
            let oid = con.oid + 1;
            let kind = match con.kind {
                ConstraintKind::PrimaryKey { .. } => "PK",
                _ => "Unique",
            };
            out.push(RiTrigger {
                trig: crate::catalog::Trigger {
                    oid,
                    name: format!("{kind}_ConstraintTrigger_{oid}"),
                    table: t.oid,
                    function: 1250,
                    tgtype: tt::ROW | tt::INSERT | tt::UPDATE,
                    columns: Vec::new(),
                    when: None,
                    args: Vec::new(),
                    enabled: (con.ri_enabled.iter())
                        .find(|e| e.0 == oid)
                        .map_or('O', |e| e.1),
                    old_table: None,
                    new_table: None,
                    constraint: true,
                    deferrable: true,
                    initdeferred: con.initially_deferred,
                    parent: 0,
                    con_oid: 0,
                    constr_rel: 0,
                },
                constraint: con.oid,
                fk_table: t.oid,
                other: 0,
                index,
            });
        }
    }
    out
}

/// The internal triggers of foreign keys and deferrable unique constraints, as the catalogs
/// show them.
pub fn internal_triggers(cat: &crate::catalog::Catalog) -> Vec<RiTrigger> {
    let mut out = ri_triggers(cat, None);
    out.extend(recheck_triggers(cat));
    out
}

/// `internal_triggers` made from the constraints of `tables` alone (in `internal_triggers`'
/// order): those of the foreign keys and deferrable keys the tables have.
pub fn internal_triggers_of(
    cat: &crate::catalog::Catalog,
    tables: &std::collections::BTreeSet<u32>,
) -> Vec<RiTrigger> {
    let mut out = ri_triggers_from(cat, tables.iter().filter_map(|t| cat.tables.get(t)), None);
    out.extend(recheck_triggers_from(tables.iter().filter_map(|t| cat.tables.get(t))));
    out
}

/// A foreign key's (or deferrable unique constraint's) internal trigger by oid.
pub fn ri_trigger(cat: &crate::catalog::Catalog, oid: u32) -> Option<RiTrigger> {
    // Its oid follows its constraint's (by 1 to 4) or the key's `ri_check` (by 1 or 2): only
    // the tables with such a constraint can have it.
    let ix = cat.tables.ix();
    let mut from = std::collections::BTreeSet::new();
    for k in 1..=4u32 {
        let Some(c) = oid.checked_sub(k) else { break };
        from.extend(ix.con_owner.get(&c).into_iter().flat_map(|c| c.keys()));
        if k <= 2 {
            from.extend(ix.ri_checks.get(&c).into_iter().flat_map(|c| c.keys()));
        }
    }
    let found = internal_triggers_of(cat, &from).into_iter().find(|r| r.trig.oid == oid);
    debug_assert_eq!(
        found.as_ref().map(|r| (r.trig.oid, r.trig.table, r.constraint)),
        internal_triggers(cat).into_iter().find(|r| r.trig.oid == oid).map(|r| (r.trig.oid, r.trig.table, r.constraint)),
        "an internal trigger by oid"
    );
    found
}

/// A table's foreign key triggers, bound (`pk_only`: only those acting for it as the
/// referenced table).
pub fn ri_bound(cat: &crate::catalog::Catalog, table: u32, pk_only: bool) -> Vec<BoundTrigger> {
    ri_triggers(cat, Some(table))
        .into_iter()
        .filter(|r| !pk_only || !ri_fn::is_check(r.trig.function))
        .map(|r| BoundTrigger {
            func: ri_function(r.trig.function),
            trig: Arc::new(r.trig),
            when: None,
            ri: Some((r.constraint, r.fk_table)),
        })
        .collect()
}

/// A referential integrity trigger function (language internal).
pub fn ri_function(oid: u32) -> Arc<crate::catalog::Function> {
    let base = crate::catalog::Function::builtin_trigger(0, "suppress_redundant_updates_trigger")
        .expect("builtin trigger function");
    let mut f = (*base).clone();
    f.oid = oid;
    f.name = ri_fn::name(oid).to_string();
    f.body = f.name.clone();
    Arc::new(f)
}

/// BEFORE / AFTER STATEMENT triggers.
pub fn statement(
    trigs: &[BoundTrigger],
    t: &Table,
    before: bool,
    event: u16,
    set_cols: &[usize],
    ctx: &ExecCtx,
) -> Result<()> {
    // A statement's BEFORE STATEMENT triggers fire once per table and event, however many
    // of its parts (data-modifying CTEs, referential actions) change the table
    // (before_stmt_triggers_fired).
    if before
        && event != tt::TRUNCATE
        && !ctx.stmt_fired.borrow_mut().insert((t.oid, event))
    {
        return Ok(());
    }
    let empty = Transition::default();
    for bt in trigs
        .iter()
        .filter(|b| applies(b, false, before, event, ctx) && columns_hit(b, event, set_cols))
    {
        call_with(bt, t, before, false, event, None, None, Some(&empty), ctx)?;
    }
    Ok(())
}

/// Trigger functions written in C in Postgres.
fn builtin_trigger(
    name: &str,
    before: bool,
    row: bool,
    event: u16,
    old: Option<Row>,
    new: Option<Row>,
) -> Result<Option<Row>> {
    let violated = |what: &str| {
        Err(crate::error::Error::new(
            crate::error::code::TRIGGER_PROTOCOL_VIOLATED,
            format!("{name}: must be called {what}"),
        ))
    };
    // suppress_redundant_updates_trigger: skip an UPDATE that changes nothing.
    if event & tt::UPDATE == 0 {
        return violated("on update");
    }
    if !before {
        return violated("before update");
    }
    if !row {
        return violated("for each row");
    }
    match (old, new) {
        (Some(o), Some(n))
            if super::scan::encode_table_row(&o, 0) == super::scan::encode_table_row(&n, 0) =>
        {
            Ok(None)
        }
        (_, n) => Ok(n),
    }
}
