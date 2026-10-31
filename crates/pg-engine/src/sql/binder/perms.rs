//! Permissions a statement needs (Postgres's RTEPermissionInfo): collected while binding,
//! checked before the plan runs (and again whenever a cached plan runs).

use super::Binder;
use crate::catalog::acl::{self, ObjKind};
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::sql::plan::Plan;
use crate::txn::locks::{LockMode, WaitPolicy};
use std::collections::BTreeSet;

/// What one relation reference needs.
#[derive(Clone, Debug, Default)]
pub struct RelPerm {
    pub oid: u32,
    /// The role to check as (a view's owner for the relations its query reads).
    pub as_user: Option<u32>,
    pub privs: u32,
    pub select_cols: BTreeSet<i16>,
    pub insert_cols: BTreeSet<i16>,
    pub update_cols: BTreeSet<i16>,
}

#[derive(Clone, Debug, Default)]
pub struct Perms {
    pub rels: Vec<RelPerm>,
    /// User functions called (EXECUTE).
    pub funcs: Vec<u32>,
    /// Where each call being bound began noting `funcs`: EXECUTE is checked in the plan's
    /// expression order, a call before its arguments (ExecInitFunc), so a call's own
    /// function goes in ahead of what its arguments noted.
    pub call_marks: Vec<usize>,
    /// While binding a table's rules: the functions its generated columns call.
    pub gen_funcs: Vec<u32>,
    /// The relations the statement locks before it runs, in order, with their modes
    /// (rellockmode: AccessShare to read, RowShare to lock rows, RowExclusive to change),
    /// then the members its plan scans and their indexes (the planner's and executor's).
    pub locks: Vec<(u32, LockMode)>,
    /// How many of `locks` the statement names (taken before it is analyzed; a wait for
    /// them can change what it means); the rest the planner and executor take.
    pub named_locks: usize,
    /// The statement's command (CreateCommandName: a SELECT with a data-modifying WITH is a
    /// SELECT), for ExecCheckXactReadOnly; None for what isn't a planned query.
    pub command: Option<&'static str>,
}

impl Perms {
    pub fn is_empty(&self) -> bool {
        self.rels.is_empty() && self.funcs.is_empty() && self.locks.is_empty()
    }

    /// Note relation `oid` is locked in `mode` (the stronger, when it already is).
    pub fn note_lock(&mut self, oid: u32, mode: LockMode) {
        match self.locks.iter_mut().find(|(o, _)| *o == oid) {
            Some((_, m)) => *m = (*m).max(mode),
            None => self.locks.push((oid, mode)),
        }
    }

    /// The tables `plan` scans or changes that the binding didn't note (inheritance members,
    /// partitions), then the indexes of every table locked, in the table's mode.
    pub fn add_plan_locks(&mut self, plan: &Plan, cat: &crate::catalog::Catalog) {
        self.named_locks = self.locks.len();
        plan_locks(plan, &mut |oid, mode| self.note_lock(oid, mode));
        let tables: Vec<(u32, LockMode)> = self.locks.clone();
        for (oid, mode) in tables {
            if let Some(t) = cat.table(oid) {
                for ix in t.pkey_index.iter().chain(t.indexes.iter()) {
                    self.note_lock(*ix, mode);
                }
            }
        }
    }
}

/// Relations a plan scans (AccessShare, RowShare under a row mark) and changes (RowExclusive).
fn plan_locks(plan: &Plan, note: &mut dyn FnMut(u32, LockMode)) {
    match plan {
        Plan::Scan(s) => note(
            s.table.oid,
            if s.mark.is_some() { LockMode::RowShare } else { LockMode::AccessShare },
        ),
        Plan::Insert(i) => {
            note(i.rules.table.oid, LockMode::RowExclusive);
            plan_locks(&i.source, note);
        }
        Plan::Update(u) => {
            note(u.rules.table.oid, LockMode::RowExclusive);
            for m in &u.members {
                note(m.rules.table.oid, LockMode::RowExclusive);
            }
            plan_locks(&u.source, note);
        }
        Plan::Delete(d) => {
            note(d.table.oid, LockMode::RowExclusive);
            for m in &d.members {
                note(m.table.oid, LockMode::RowExclusive);
            }
            plan_locks(&d.source, note);
        }
        Plan::Merge(m) => {
            note(m.rules.table.oid, LockMode::RowExclusive);
            plan_locks(&m.source, note);
        }
        Plan::Filter { input, .. }
        | Plan::Project { input, .. }
        | Plan::ProjectSet { input, .. }
        | Plan::Agg { input, .. }
        | Plan::Window { input, .. }
        | Plan::Sort { input, .. }
        | Plan::Limit { input, .. }
        | Plan::Distinct { input, .. }
        | Plan::LockRows { input, .. }
        | Plan::Barrier { input, .. }
        | Plan::Context { input, .. } => plan_locks(input, note),
        Plan::SetOp { left, right, .. } | Plan::Join { left, right, .. } => {
            plan_locks(left, note);
            plan_locks(right, note);
        }
        Plan::Append { inputs, .. } => {
            for p in inputs.iter() {
                plan_locks(p, note);
            }
        }
        Plan::With { ctes, body, .. } => {
            for (_, p) in ctes.iter() {
                plan_locks(p, note);
            }
            plan_locks(body, note);
        }
        Plan::Recursive { base, recursive, .. } => {
            plan_locks(base, note);
            plan_locks(recursive, note);
        }
        _ => {}
    }
}

impl Binder<'_, '_> {
    /// Note that the statement needs `privs` on relation `oid` (as the current check-as role).
    pub(crate) fn require_rel(&mut self, oid: u32, privs: u32) -> usize {
        // Its rellockmode: a target of INSERT / UPDATE / DELETE changes it, a FROM item
        // locked FOR UPDATE / SHARE (UPDATE with SELECT) locks rows of it.
        let mode = if privs & (acl::INSERT | acl::DELETE) != 0
            || (privs & acl::UPDATE != 0 && privs & acl::SELECT == 0)
        {
            LockMode::RowExclusive
        } else if privs & acl::UPDATE != 0 {
            LockMode::RowShare
        } else {
            LockMode::AccessShare
        };
        self.perms.note_lock(oid, mode);
        if self.cat.table(oid).is_some_and(|t| t.temporary) {
            self.ctx.sess.temp_used.set(true);
            self.ctx.sess.temp_ever.set(true);
        }
        let as_user = self.check_as;
        match self
            .perms
            .rels
            .iter()
            .position(|p| p.oid == oid && p.as_user == as_user)
        {
            Some(i) => {
                self.perms.rels[i].privs |= privs;
                i
            }
            None => {
                self.perms.rels.push(RelPerm {
                    oid,
                    as_user,
                    privs,
                    ..Default::default()
                });
                self.perms.rels.len() - 1
            }
        }
    }

    /// A column of `table` is read.
    pub(crate) fn note_col(&mut self, table: u32, attnum: i16) {
        if table == 0 {
            return;
        }
        let as_user = self.check_as;
        if let Some(p) = self
            .perms
            .rels
            .iter_mut()
            .rev()
            .find(|p| p.oid == table && p.as_user == as_user)
        {
            p.select_cols.insert(attnum);
        }
    }

    /// `f`, with what it notes for dependencies dropped: expressions the rewriter or
    /// planner adds (generated columns, defaults), not part of the statement's query tree.
    pub(crate) fn unrecorded<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let at = (self.deps.as_ref().map(Vec::len), self.rowtype_deps.len());
        let r = f(self);
        if let (Some(n), Some(d)) = (at.0, self.deps.as_mut()) {
            d.truncate(n);
        }
        self.rowtype_deps.truncate(at.1);
        r
    }

    /// A column of a function's composite result is read: what's collecting dependencies
    /// notes the composite type's relation's column.
    pub(crate) fn note_rowtype_col(&mut self, col: Option<(u32, i16)>) {
        if let Some(c) = col
            && self.deps.is_some()
            && !self.rowtype_deps.contains(&c)
        {
            self.rowtype_deps.push(c);
        }
    }

    pub(crate) fn note_target_cols(&mut self, idx: usize, cols: &[usize], insert: bool) {
        let p = &mut self.perms.rels[idx];
        for c in cols {
            let n = *c as i16 + 1;
            if insert {
                p.insert_cols.insert(n);
            } else {
                p.update_cols.insert(n);
            }
        }
    }

    /// A built-in function is called: its EXECUTE privilege is checked when it isn't PUBLIC's.
    pub(crate) fn note_builtin_call(&mut self, oid: u32) {
        if acl::builtin_proc_acl(oid).is_some() || self.cat.acl(ObjKind::Function, oid, 0).is_some()
        {
            self.require_function(oid);
        }
    }

    /// `note_builtin_call` for the call being bound (ahead of its arguments' functions).
    pub(crate) fn note_builtin_call_first(&mut self, oid: u32) {
        if acl::builtin_proc_acl(oid).is_some() || self.cat.acl(ObjKind::Function, oid, 0).is_some()
        {
            self.require_call(oid);
        }
    }

    pub(crate) fn require_function(&mut self, oid: u32) {
        if !self.perms.funcs.contains(&oid) {
            self.perms.funcs.push(oid);
        }
    }

    /// `require_function` for the call being bound.
    pub(crate) fn require_call(&mut self, oid: u32) {
        if !self.perms.funcs.contains(&oid) {
            let at = self.perms.call_marks.last().copied().unwrap_or(usize::MAX).min(self.perms.funcs.len());
            self.perms.funcs.insert(at, oid);
        }
    }

    /// USAGE on an explicitly named schema (name lookups through it fail otherwise).
    pub(crate) fn check_schema_usage(&self, schema: &str, loc: i32) -> Result<()> {
        let Some(s) = self.cat.schema_by_name(schema) else {
            return Ok(());
        };
        let me = self.ctx.current_user();
        if self.cat.is_superuser(me) {
            return Ok(());
        }
        // pg_namespace_aclcheck: the session's own temporary schema (`pg_temp`) is usable.
        if schema == "pg_temp" || s.name == self.ctx.sess.temp_schema() {
            return Ok(());
        }
        let (p, _) = self
            .cat
            .obj_mask(ObjKind::Schema, s.oid, me, self.ctx.db_owner());
        if p & acl::USAGE == 0 {
            return Err(super::ast::with_pos(
                Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for schema {schema}"),
                ),
                loc,
            ));
        }
        Ok(())
    }

    /// Check what the bound statement needs, as the current user.
    pub fn check_perms(&self) -> Result<()> {
        check(&self.perms, self.ctx)
    }
}

/// "table", "view", … for "permission denied for …".
pub fn rel_word(cat: &crate::catalog::Catalog, oid: u32) -> (&'static str, String) {
    if let Some(t) = cat.tables.get(&oid) {
        return (if t.is_foreign() { "foreign table" } else { "table" }, t.name.clone());
    }
    if let Some(v) = cat.views.get(&oid) {
        return (
            if v.materialized {
                "materialized view"
            } else {
                "view"
            },
            v.name.clone(),
        );
    }
    if let Some(s) = cat.sequences.get(&oid) {
        return ("sequence", s.name.clone());
    }
    // (An index was "relation <oid>": `Ddl::note_lock` took that for one dropped while its
    // lock was waited for whenever any DDL committed meanwhile, and failed pg_restore -j's
    // foreign keys, REINDEX and ALTER INDEX with "relation <oid> does not exist".)
    if let Some(i) = cat.indexes.get(&oid) {
        return ("index", i.name.clone());
    }
    if let Some((t, index)) = cat.toast_owner(oid) {
        return (if index { "index" } else { "table" }, t.toast_relname(index));
    }
    match crate::catalog::system::SysTable::by_oid(oid) {
        Some(t) => (
            if t.is_view() { "view" } else { "table" },
            t.name().to_string(),
        ),
        None => ("relation", oid.to_string()),
    }
}

/// A SQL function's statement about to run again after `row_security` was turned off
/// (by an earlier call: `set_config('row_security', 'false', true)`): Postgres re-plans it
/// (the plan depends on the setting), and rewriting it fails for a table whose policies
/// apply (check_enable_rls). With the setting on, the plan made at bind time stands.
pub fn check_row_security(perms: &Perms, ctx: &ExecCtx) -> Result<()> {
    if perms.rels.is_empty()
        || ctx.sess.settings.borrow().get_raw("row_security").as_deref() != Some("off")
    {
        return Ok(());
    }
    let cat = ctx.catalog();
    let me = ctx.current_user();
    let db_owner = ctx.db_owner();
    for p in &perms.rels {
        let Some(t) = cat.tables.get(&p.oid) else {
            continue;
        };
        if !t.rls {
            continue;
        }
        let user = p.as_user.unwrap_or(me);
        if cat.is_superuser(user) || cat.roles.get(&user).is_some_and(|r| r.bypass_rls) {
            continue;
        }
        let owner = cat.has_privs_of_role(user, t.owner, db_owner);
        if owner && !t.rls_force {
            continue;
        }
        let e = Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!(
                "query would be affected by row-level security policy for table \"{}\"",
                t.name
            ),
        );
        return Err(if owner {
            e.hint("To disable the policy for the table's owner, use ALTER TABLE NO FORCE ROW LEVEL SECURITY.")
        } else {
            e
        });
    }
    Ok(())
}

/// ExecCheckPermissions.
pub fn check(perms: &Perms, ctx: &ExecCtx) -> Result<()> {
    if perms.is_empty() {
        return Ok(());
    }
    lock_relations(perms, ctx)?;
    // The system views' columns read (see `ExecCtx::sys_cols`).
    for p in &perms.rels {
        if !p.select_cols.is_empty() && crate::catalog::system::SysTable::by_oid(p.oid).is_some() {
            ctx.sys_cols
                .borrow_mut()
                .entry(p.oid)
                .or_default()
                .extend(p.select_cols.iter().copied());
        }
    }
    let cat = ctx.catalog();
    let me = ctx.current_user();
    let db_owner = ctx.db_owner();
    for p in &perms.rels {
        let user = p.as_user.unwrap_or(me);
        if cat.is_superuser(user) {
            continue;
        }
        let Some((held, _)) = cat.rel_mask(p.oid, user, db_owner) else {
            continue;
        };
        let mut need = p.privs;
        if !p.select_cols.is_empty() {
            need |= acl::SELECT;
        }
        let remaining = need & !held;
        if remaining == 0 {
            continue;
        }
        let denied = || {
            let (word, name) = rel_word(&cat, p.oid);
            Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for {word} {name}"),
            )
        };
        if remaining & !(acl::SELECT | acl::INSERT | acl::UPDATE) != 0 {
            return Err(denied());
        }
        // Column privileges can stand in for missing table privileges.
        for (bit, cols) in [
            (acl::SELECT, &p.select_cols),
            (acl::INSERT, &p.insert_cols),
            (acl::UPDATE, &p.update_cols),
        ] {
            if remaining & bit == 0 {
                continue;
            }
            let ok = if cols.is_empty() || cols.contains(&0) {
                let all_cols = cols.contains(&0);
                let col_entries: Vec<i32> = cat
                    .acls
                    .iter()
                    .filter(|a| a.class == 1259 && a.oid == p.oid && a.sub > 0)
                    .map(|a| a.sub)
                    .collect();
                if all_cols {
                    let n = cat
                        .tables
                        .get(&p.oid)
                        .map_or(0, |t| t.columns.iter().filter(|c| !c.dropped).count());
                    n > 0
                        && (1..=n as i32)
                            .all(|c| cat.col_mask(p.oid, c, user, db_owner).0 & bit != 0)
                } else {
                    col_entries
                        .into_iter()
                        .any(|c| cat.col_mask(p.oid, c, user, db_owner).0 & bit != 0)
                }
            } else {
                cols.iter()
                    .all(|c| cat.col_mask(p.oid, *c as i32, user, db_owner).0 & bit != 0)
            };
            if !ok {
                return Err(denied());
            }
        }
    }
    for f in &perms.funcs {
        if cat.is_superuser(me) {
            break;
        }
        let (held, _) = cat.obj_mask(ObjKind::Function, *f, me, db_owner);
        if held & acl::EXECUTE == 0 {
            let (word, name) = match cat.functions.get(f) {
                Some(f) => (
                    if f.procedure {
                        "procedure"
                    } else if f.agg.is_some() {
                        "aggregate"
                    } else {
                        "function"
                    },
                    f.name.clone(),
                ),
                None => (
                    "function",
                    crate::sql::func::registry()
                        .funcs
                        .values()
                        .flatten()
                        .find(|d| d.oid == *f)
                        .map(|d| d.name.to_string())
                        .or_else(|| {
                            crate::catalog::system::builtin_proc_sigs()
                                .iter()
                                .find(|p| p.oid == *f)
                                .map(|p| p.name.to_string())
                        })
                        .unwrap_or_else(|| f.to_string()),
                ),
            };
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for {word} {name}"),
            ));
        }
    }
    check_xact_read_only(perms, ctx)
}

/// ExecCheckXactReadOnly: in a read-only transaction, a statement needing more than SELECT
/// on a relation outside the temporary schema fails before it runs, whether or not it would
/// change a row, in a function's statement too.
fn check_xact_read_only(perms: &Perms, ctx: &ExecCtx) -> Result<()> {
    let Some(cmd) = perms.command else { return Ok(()) };
    if !ctx.txn.borrow().read_only {
        return Ok(());
    }
    let cat = ctx.catalog();
    let writes = perms.rels.iter().any(|p| {
        p.privs & !acl::SELECT != 0
            && !cat.table(p.oid).is_some_and(|t| cat.schema_name(t.schema).starts_with("pg_temp"))
    });
    if writes {
        return Err(Error::new(
            code::READ_ONLY_SQL_TRANSACTION,
            format!("cannot execute {cmd} in a read-only transaction"),
        ));
    }
    Ok(())
}

/// Take the locks the statement's relations need (AcquireExecutorLocks), until the
/// transaction ends.
pub fn lock_relations(perms: &Perms, ctx: &ExecCtx) -> Result<()> {
    lock_some(perms, ctx, perms.locks.len())
}

/// Take the locks of the relations the statement names (what parse analysis locks).
pub fn lock_named_relations(perms: &Perms, ctx: &ExecCtx) -> Result<()> {
    lock_some(perms, ctx, perms.named_locks)
}

fn lock_some(perms: &Perms, ctx: &ExecCtx, n: usize) -> Result<()> {
    if perms.locks.is_empty() {
        return Ok(());
    }
    // (The catalog the statement was planned over.)
    let cat = ctx.catalog();
    for (i, (oid, mode)) in perms.locks[..n.min(perms.locks.len())].iter().enumerate() {
        let name = || rel_word(&cat, *oid).1;
        let before = ctx.catalog().version;
        ctx.txn
            .borrow_mut()
            .lock_relation(*oid, *mode, WaitPolicy::Block, &name)?;
        // An inheritance child the plan found, locked after a wait (find_all_inheritors):
        // looked at again as other transactions left it.
        if i >= perms.named_locks && ctx.catalog().version != before {
            recheck_child(&cat, *oid, perms, ctx)?;
        }
    }
    Ok(())
}

/// An inheritance child after waiting for its lock: one dropped meanwhile is left out (it
/// reads as empty); one whose columns no longer match a parent the statement reads fails as
/// make_inh_translation_list does.
fn recheck_child(planned: &crate::catalog::Catalog, oid: u32, perms: &Perms, ctx: &ExecCtx) -> Result<()> {
    let Some(old) = planned.tables.get(&oid) else {
        return Ok(());
    };
    if old.inherits.is_empty() || old.partbound.is_some() {
        return Ok(());
    }
    let now = ctx.catalog();
    let Some(child) = now.tables.get(&oid) else {
        ctx.gone_rels.borrow_mut().insert(oid);
        return Ok(());
    };
    for p in &old.inherits {
        if !perms.locks.iter().any(|(o, _)| o == p) {
            continue;
        }
        let Some(parent) = now.tables.get(p) else { continue };
        for pc in parent.columns.iter().filter(|c| !c.dropped) {
            let Some(cc) = child.columns.iter().find(|c| !c.dropped && c.name == pc.name) else {
                return Err(crate::error::internal(format!(
                    "could not find inherited attribute \"{}\" of relation \"{}\"",
                    pc.name, child.name
                )));
            };
            if cc.ty != pc.ty {
                return Err(Error::new(
                    code::INVALID_COLUMN_DEFINITION,
                    format!(
                        "attribute \"{}\" of relation \"{}\" does not match parent's type",
                        pc.name, child.name
                    ),
                ));
            }
            if cc.collation != pc.collation {
                return Err(Error::new(
                    code::INVALID_COLUMN_DEFINITION,
                    format!(
                        "attribute \"{}\" of relation \"{}\" does not match parent's collation",
                        pc.name, child.name
                    ),
                ));
            }
        }
    }
    Ok(())
}
