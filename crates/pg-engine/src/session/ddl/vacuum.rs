//! VACUUM, ANALYZE and CLUSTER. Storage needs no vacuuming (old versions go with their
//! transactions), so these validate like Postgres and refresh what they would have
//! measured: pg_class.relpages / reltuples / relallvisible of tables and their indexes.
//! CLUSTER also rewrites a table in index order.

use super::Ddl;
use crate::catalog::{Index, RelStats, Relation, Table, relsize};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{const_str, def_arg_str, inner, ival, sval, with_pos};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// A VACUUM or ANALYZE runs in the session (vacuum()'s `in_vacuum`) until this goes. (The
/// session's: a nested statement may run on another thread.)
struct InVacuum<'s>(&'s std::cell::Cell<bool>, bool);

impl Drop for InVacuum<'_> {
    fn drop(&mut self) {
        self.0.set(self.1);
    }
}

impl Ddl<'_, '_> {
    pub(super) fn vacuum(&mut self, s: &pb::VacuumStmt) -> Result<String> {
        let what = if s.is_vacuumcmd { "VACUUM" } else { "ANALYZE" };
        // No VACUUM or ANALYZE inside one (an index expression's function, say).
        if self.ctx.sess.in_vacuum.get() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("{what} cannot be executed from VACUUM or ANALYZE"),
            ));
        }
        let _in = InVacuum(&self.ctx.sess.in_vacuum, self.ctx.sess.in_vacuum.replace(true));
        let mut vacuum = s.is_vacuumcmd;
        let mut analyze = !s.is_vacuumcmd;
        let (mut full, mut skip_pages, mut only_db, mut verbose) = (false, false, false, false);
        let mut freeze = false;
        let mut skip_locked = false;
        let (mut process_main, mut process_toast) = (true, true);
        let mut truncate: Option<bool> = None;
        let (mut ring_size, mut nworkers) = (None::<i64>, 0i64);
        for d in super::def_elems(&s.options) {
            let flag = || bool_option(d);
            match (s.is_vacuumcmd, d.defname.as_str()) {
                (_, "verbose") => verbose = flag()?,
                (_, "skip_locked") => skip_locked = flag()?,
                (_, "buffer_usage_limit") => {
                    let Some(v) = def_arg_str(d) else {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "buffer_usage_limit requires a parameter",
                        ));
                    };
                    // 0, or between 128 kB and 16 GB.
                    let bad = |hint: Option<&'static str>| {
                        let e = Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            "BUFFER_USAGE_LIMIT option must be 0 or between 128 kB and 16777216 kB",
                        );
                        match hint {
                            Some(h) => e.hint(h),
                            None => e,
                        }
                    };
                    match crate::session::settings::parse_int_kb(&v) {
                        Ok(n) if n == 0 || (128..=16_777_216).contains(&n) => ring_size = Some(n),
                        Ok(_) => return Err(bad(None)),
                        Err(h) => return Err(bad(h)),
                    }
                }
                (true, "analyze") => analyze = flag()?,
                (true, "full") => full = flag()?,
                (true, "freeze") => freeze = flag()?,
                (true, "disable_page_skipping") => skip_pages = flag()?,
                (true, "only_database_stats") => only_db = flag()?,
                (true, "process_main") => process_main = flag()?,
                (true, "process_toast") => process_toast = flag()?,
                (true, "truncate") => truncate = Some(flag()?),
                (true, "skip_database_stats") => {
                    flag()?;
                }
                (true, "index_cleanup") => {
                    if def_arg_str(d).is_some_and(|v| v.eq_ignore_ascii_case("auto")) {
                        continue;
                    }
                    flag()?;
                }
                (true, "parallel") => match d.arg.as_deref().and_then(ival) {
                    Some(n) if (0..=1024).contains(&n) => nworkers = n,
                    Some(_) => {
                        return Err(with_pos(
                            Error::new(
                                code::SYNTAX_ERROR,
                                "parallel workers for vacuum must be between 0 and 1024",
                            ),
                            d.location,
                        ));
                    }
                    _ => {
                        return Err(with_pos(
                            Error::new(
                                code::SYNTAX_ERROR,
                                "parallel option requires a value between 0 and 1024",
                            ),
                            d.location,
                        ));
                    }
                },
                (_, o) => {
                    return Err(with_pos(
                        Error::new(
                            code::SYNTAX_ERROR,
                            format!("unrecognized {what} option \"{o}\""),
                        ),
                        d.location,
                    ));
                }
            }
        }
        vacuum &= s.is_vacuumcmd;
        crate::exec::sizing::set_vacuum_truncate(truncate);
        if full && nworkers > 0 {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "VACUUM FULL cannot be performed in parallel",
            ));
        }
        if ring_size.is_some() && full && !analyze {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "BUFFER_USAGE_LIMIT cannot be specified for VACUUM FULL",
            ));
        }
        if full && skip_pages {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "VACUUM option DISABLE_PAGE_SKIPPING cannot be used with FULL",
            ));
        }
        if full && !process_toast {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "PROCESS_TOAST required with VACUUM FULL",
            ));
        }
        let rels: Vec<&pb::VacuumRelation> = s
            .rels
            .iter()
            .filter_map(|n| match inner(n) {
                Some(NodeEnum::VacuumRelation(r)) => Some(r),
                _ => None,
            })
            .collect();
        if only_db {
            if !rels.is_empty() {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "ONLY_DATABASE_STATS cannot be specified with a list of tables",
                ));
            }
            return Ok(what.into());
        }
        if !analyze && rels.iter().any(|r| !r.va_cols.is_empty()) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "ANALYZE option must be specified when a column list is provided",
            ));
        }
        // The relations, each resolved and checked before any is processed.
        let mut targets: Vec<(Arc<Table>, Vec<String>, bool)> = Vec::new();
        for r in &rels {
            let Some(rv) = &r.relation else { continue };
            let t = match self.lookup(rv)? {
                // A foreign table isn't vacuumed; its wrapper would analyze it.
                Some(Relation::Table(t)) if t.is_foreign() => {
                    if vacuum {
                        let msg = format!(
                            "skipping \"{}\" --- cannot vacuum non-tables or special system tables",
                            rv.relname
                        );
                        self.ctx.notice(crate::session::warning(code::WARNING, msg));
                    }
                    // (A skipped VACUUM skips its ANALYZE too.)
                    if analyze
                        && !vacuum
                        && let Some(e) = self.cat.fdw_routine_error(&t)
                    {
                        return Err(e);
                    }
                    continue;
                }
                Some(Relation::Table(t)) => t,
                Some(Relation::View(v)) if v.materialized => match &v.storage {
                    Some(st) => st.clone(),
                    None => continue,
                },
                Some(_) => {
                    let verb = if vacuum { "vacuum" } else { "analyze" };
                    let msg = format!(
                        "skipping \"{}\" --- cannot {verb} non-tables or special system tables",
                        rv.relname
                    );
                    self.ctx.notice(crate::session::warning(code::WARNING, msg));
                    continue;
                }
                // A system catalog: nothing to do here, but for its owner's (a
                // superuser's) say.
                None if crate::catalog::system::SysTable::lookup(
                    Self::rv_schema(rv),
                    &rv.relname,
                    self.search_path(),
                )
                .is_some() =>
                {
                    if !self.is_superuser()
                        && !self.cat.has_privs_of_role(
                            self.me(),
                            crate::catalog::acl::PG_MAINTAIN,
                            self.db_owner(),
                        )
                    {
                        let verb = if vacuum { "vacuum" } else { "analyze" };
                        self.ctx.notice(crate::session::warning(
                            code::INSUFFICIENT_PRIVILEGE,
                            format!("permission denied to {verb} \"{}\", skipping it", rv.relname),
                        ));
                    }
                    continue;
                }
                None => {
                    return Err(crate::catalog::no_such_relation(
                        Self::rv_schema(rv),
                        &rv.relname,
                    ));
                }
            };
            let cols: Vec<&str> = r.va_cols.iter().filter_map(sval).collect();
            for (i, c) in cols.iter().enumerate() {
                if t.column_index(c).is_none_or(|j| t.columns[j].dropped) {
                    return Err(Error::new(
                        code::UNDEFINED_COLUMN,
                        format!("column \"{c}\" of relation \"{}\" does not exist", t.name),
                    ));
                }
                if cols[..i].contains(c) {
                    return Err(Error::new(
                        code::DUPLICATE_COLUMN,
                        format!(
                            "column \"{c}\" of relation \"{}\" appears more than once",
                            t.name
                        ),
                    ));
                }
            }
            if !self.may_maintain(t.oid) {
                let verb = if vacuum { "vacuum" } else { "analyze" };
                self.ctx.notice(crate::session::warning(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to {verb} \"{}\", skipping it", t.name),
                ));
                // (expand_vacuum_rel: a partitioned table's partitions are each taken
                // on their own all the same.)
                if t.partkey.is_some() && rv.inh {
                    for o in self.cat.inheritors(t.oid).into_iter().skip(1) {
                        if let Some(c) = self.cat.tables.get(&o).cloned() {
                            if !self.may_maintain(c.oid) {
                                self.ctx.notice(crate::session::warning(
                                    code::INSUFFICIENT_PRIVILEGE,
                                    format!("permission denied to {verb} \"{}\", skipping it", c.name),
                                ));
                                continue;
                            }
                            targets.push((c, cols.iter().map(|c| c.to_string()).collect(), false));
                        }
                    }
                }
                continue;
            }
            targets.push((t, cols.iter().map(|c| c.to_string()).collect(), rv.inh));
        }
        if rels.is_empty() {
            let temp = self.ctx.sess.temp_schema();
            let mine = self.cat.schema_by_name(&temp).map(|s| s.oid);
            let mut all: Vec<Arc<Table>> = self
                .cat
                .tables
                .values()
                .filter(|t| (!t.temporary || Some(t.schema) == mine) && !t.is_foreign())
                .cloned()
                .collect();
            all.extend(self.cat.views.values().filter_map(|v| v.storage.clone()));
            targets = all
                .into_iter()
                .filter(|t| self.may_maintain(t.oid))
                .map(|t| (t, Vec::new(), false))
                .collect();
        }
        let _ = verbose;
        let mut analyzed = Vec::new();
        let own_xacts =
            vacuum || super::TOP_LEVEL.with(|t| t.get()) == Some(false);
        let mut mark: Option<usize> = None;
        for (t, cols, recurse) in targets {
            if vacuum && !recurse && t.partkey.is_some() && !rels.is_empty() {
                self.ctx.notice(crate::session::warning(
                    code::WARNING,
                    format!(
                        "VACUUM ONLY of partitioned table \"{}\" has no effect",
                        t.name
                    ),
                ));
            }
            // Every table in the inheritance tree, unless ONLY.
            let tree: Vec<Arc<Table>> = if recurse && self.cat.tables.contains_key(&t.oid) {
                self.cat
                    .inheritors(t.oid)
                    .iter()
                    .filter_map(|o| self.cat.tables.get(o).cloned())
                    .collect()
            } else {
                vec![t.clone()]
            };
            for r in tree {
                // Each relation in a transaction of its own (vacuum's use_own_xacts: VACUUM,
                // and ANALYZE outside a transaction block): the locks one took are gone
                // before the next is processed.
                if own_xacts && let Some(m) = mark.take() {
                    self.ctx.txn.borrow_mut().release_locks_since(m);
                }
                mark = Some(self.ctx.txn.borrow().lock_mark());
                // A foreign child is sampled by its wrapper.
                if r.is_foreign() {
                    if analyze && let Some(e) = self.cat.fdw_routine_error(&r) {
                        return Err(e);
                    }
                    continue;
                }
                if r.oid != t.oid && !self.may_maintain(r.oid) {
                    let verb = if vacuum { "vacuum" } else { "analyze" };
                    self.ctx.notice(crate::session::warning(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied to {verb} \"{}\", skipping it", r.name),
                    ));
                    continue;
                }
                // vacuum_open_relation: SHARE UPDATE EXCLUSIVE (VACUUM FULL: ACCESS
                // EXCLUSIVE), or skipped when SKIP_LOCKED finds it taken; a relation named in
                // the command and dropped while we waited is skipped with a warning.
                let named = r.oid == t.oid && !rels.is_empty();
                let verb = if vacuum { "vacuum" } else { "analyze" };
                let mode = if vacuum && full {
                    super::table::lockmode::ACCESS_EXCLUSIVE
                } else {
                    super::table::lockmode::SHARE_UPDATE_EXCLUSIVE
                };
                if skip_locked {
                    if !self.try_lock(r.oid, mode)? {
                        if named {
                            self.ctx.notice(crate::session::warning(
                                code::LOCK_NOT_AVAILABLE,
                                format!("skipping {verb} of \"{}\" --- lock not available", r.name),
                            ));
                        }
                        continue;
                    }
                } else {
                    self.note_lock(r.oid, mode)?;
                }
                // ANALYZE of a parent samples its children (acquire_inherited_sample_rows,
                // their locks taken without SKIP_LOCKED).
                if analyze && (r.partkey.is_some() || !self.cat.children(r.oid).is_empty()) {
                    for c in self.cat.inheritors(r.oid).into_iter().skip(1) {
                        self.note_lock(c, super::table::lockmode::ACCESS_SHARE)?;
                    }
                }
                self.refresh_catalog();
                let r = match self.cat.tables.get(&r.oid).cloned() {
                    Some(r) => r,
                    None if self.cat.views.values().any(|v| v.storage.as_ref().is_some_and(|s| s.oid == r.oid)) => r,
                    None => {
                        if named {
                            self.ctx.notice(crate::session::warning(
                                code::UNDEFINED_TABLE,
                                format!("skipping {verb} of \"{}\" --- relation no longer exists", r.name),
                            ));
                        }
                        continue;
                    }
                };
                if r.partkey.is_none() {
                    // (dead_items_alloc: a temporary table with two indexes or more isn't
                    // vacuumed in parallel.)
                    if vacuum
                        && !full
                        && nworkers > 0
                        && r.temporary
                        && r.indexes.len() + usize::from(r.pkey_index.is_some()) > 1
                    {
                        self.ctx.notice(crate::session::warning(
                            code::WARNING,
                            format!(
                                "disabling parallel option of vacuum on \"{}\" --- cannot vacuum temporary tables in parallel",
                                r.name
                            ),
                        ));
                    }
                    // VACUUM FULL rewrites the table and rebuilds its indexes.
                    let started = std::time::Instant::now();
                    let ms = |s: std::time::Instant| s.elapsed().as_secs_f64() * 1000.0;
                    if vacuum && full {
                        // PROCESS_MAIN false: only the TOAST table gets new files.
                        if process_main {
                            self.measure_rewritten(&r, None)?;
                            // (It rebuilds the indexes: their expressions are computed.)
                            self.evaluate_index_exprs(&r, None)?;
                            crate::exec::pgstat::io_rewrite(self.ctx, r.oid);
                        } else if process_toast && r.toast != 0 {
                            self.cat.new_filenode(r.toast);
                            self.cat.new_filenode(r.toast + 1);
                        }
                    } else {
                        // PROCESS_MAIN false: only the TOAST table is vacuumed.
                        let mut live = None;
                        if process_main || !vacuum {
                            live = Some(self.measure(&r, vacuum)?);
                        }
                        if vacuum {
                            self.advance_frozen_xid(&r, freeze, process_main, process_toast)?;
                            // pgstat_report_vacuum, of the table and of its TOAST table.
                            if let Some(live) = live {
                                let pages = crate::exec::sizing::modelled_blocks(r.oid, self.ctx)
                                    .unwrap_or(0) as i64;
                                crate::exec::pgstat::count_io(self.ctx, "relation", "vacuum", "reads", pages.max(1));
                                crate::exec::pgstat::count_io(self.ctx, "relation", "vacuum", "reuses", 1);
                                crate::exec::pgstat::report_vacuum(self.ctx, r.oid, live as i64, 0, ms(started));
                            }
                            if process_toast && r.toast != 0 {
                                crate::exec::pgstat::report_vacuum(self.ctx, r.toast, 0, 0, 0.0);
                            }
                        }
                    }
                    if analyze {
                        let started = std::time::Instant::now();
                        self.analyze_columns(&r, &cols, false)?;
                        let live = self.table_rows(&r)?.len() as i64;
                        crate::exec::pgstat::report_analyze(
                            self.ctx,
                            r.oid,
                            live,
                            0,
                            cols.is_empty(),
                            false,
                            ms(started),
                        );
                    }
                }
                if !analyze {
                    continue;
                }
                analyzed.push(r.oid);
                // An inheritance parent without children any more: relhassubclass goes.
                if self.cat.children(r.oid).is_empty() {
                    self.cat.class_has_subclass.remove(&r.oid);
                }
                // Statistics over the whole tree below a partitioned table or a parent.
                if r.partkey.is_some() || !self.cat.children(r.oid).is_empty() {
                    self.analyze_columns(&r, &cols, true)?;
                }
                if r.partkey.is_some() {
                    crate::exec::pgstat::report_analyze(self.ctx, r.oid, 0, 0, cols.is_empty(), true, 0.0);
                    let mut tuples = 0.0;
                    for leaf in self.cat.leaves(r.oid) {
                        tuples += self.table_rows(&leaf)?.len() as f64;
                    }
                    self.cat.relstats.insert(
                        r.oid,
                        RelStats {
                            pages: -1,
                            tuples,
                            allvisible: 0,
                            allfrozen: 0,
                        },
                    );
                }
            }
        }
        // VACUUM updates pg_class in place, taking no transaction id; so does ANALYZE unless
        // it stores statistics (it found rows).
        let stored = analyzed
            .iter()
            .any(|o| self.cat.colstats.get(o).is_some_and(|s| !s.is_empty()));
        if !full && !stored {
            self.save_inplace()?;
        } else {
            self.save()?;
        }
        Ok(what.into())
    }

    /// VACUUM's new relfrozenxid: OldestXmin, held back by the oldest xmin of a row not yet
    /// frozen (those older than the current relfrozenxid are; FREEZE freezes them all). It
    /// never goes back. The TOAST table's is held back the same way by the rows with values
    /// stored out of line (their chunks have the row's xmin). PROCESS_MAIN / PROCESS_TOAST
    /// false leave the heap's / the TOAST table's alone.
    fn advance_frozen_xid(
        &mut self,
        t: &Table,
        freeze: bool,
        main: bool,
        toast: bool,
    ) -> Result<()> {
        let oldest = self.recent_xmin();
        let first = crate::txn::xids::FIRST_NORMAL_XID;
        let old = self.cat.frozen_xids.get(&t.oid).copied().unwrap_or(first);
        let toast = toast && t.toast != 0;
        let old_toast = self.cat.frozen_xids.get(&t.toast).copied().unwrap_or(first);
        let (mut new, mut new_toast) = (oldest, oldest);
        if !freeze {
            let start = crate::storage::keys::index_prefix(t.oid, 0);
            let end = crate::storage::keys::prefix_end(&start);
            let entries: Vec<(Vec<u8>, bytes::Bytes)> = (self.ctx.txn.borrow())
                .scan(&start, &end)
                .collect::<Result<_>>()?;
            let none = std::collections::HashSet::new();
            for (_, v) in entries {
                let x = crate::exec::scan::row_xmin(&v)?;
                if main && x >= old && x < new {
                    new = x;
                }
                if toast && x >= old_toast && x < new_toast {
                    let row = crate::exec::scan::decode_table_row(t, &v)?;
                    if !crate::exec::sizing::layout(t, &row, self.ctx, &none).1.is_empty() {
                        new_toast = x;
                    }
                }
            }
        }
        if main {
            self.cat.frozen_xids.insert(t.oid, new.max(old));
        }
        if toast {
            self.cat.frozen_xids.insert(t.toast, new_toast.max(old_toast));
        }
        Ok(())
    }

    /// TRUNCATE (and a matview's refill) leave a table and its indexes unmeasured.
    /// heap_truncate_one_rel's statistics: its indexes rebuilt over the empty heap
    /// (index_update_stats: the heap's reltuples 0, unless unknown, or left alone with
    /// autovacuum off for it; nothing changes without indexes).
    pub(super) fn truncated_in_place(&mut self, t: &Table) {
        let indexes: Vec<u32> = t.indexes.iter().chain(&t.pkey_index).copied().collect();
        if indexes.is_empty() {
            return;
        }
        let autovacuum_off = t.options.iter().any(|o| {
            o.split_once('=').is_some_and(|(k, v)| {
                k == "autovacuum_enabled"
                    && crate::types::text::parse_bool(v.trim_matches('\'')).is_ok_and(|b| !b)
            })
        });
        if !autovacuum_off {
            let tuples = match self.cat.relstats.get(&t.oid) {
                Some(s) if s.tuples >= 0.0 => 0.0,
                _ => -1.0,
            };
            self.cat.relstats.insert(
                t.oid,
                RelStats {
                    pages: 0,
                    tuples,
                    allvisible: 0,
                    allfrozen: 0,
                },
            );
        }
        for i in indexes {
            self.cat.relstats.remove(&i);
        }
    }

    pub(super) fn reset_stats(&mut self, t: &Table) {
        self.cat.relstats.remove(&t.oid);
        for i in t.indexes.iter().chain(&t.pkey_index) {
            self.cat.relstats.insert(
                *i,
                RelStats {
                    pages: 0,
                    tuples: -1.0,
                    allvisible: 0,
                    allfrozen: 0,
                },
            );
        }
    }

    /// Whether the current role may VACUUM / ANALYZE a relation (owner or MAINTAIN).
    fn may_maintain(&self, oid: u32) -> bool {
        let me = self.me();
        self.cat.is_superuser(me)
            || self
                .cat
                .rel_mask(oid, me, self.db_owner())
                .is_none_or(|(p, _)| p & crate::catalog::acl::MAINTAIN != 0)
    }

    /// Measure a table into pg_class as VACUUM (every page is then all-visible; btree
    /// indexes with nothing to clean up keep their stats) or ANALYZE (its indexes too)
    /// does; returns the table's tuple count.
    /// `measure_quiet`: what the modelling's evaluations of index expressions say isn't
    /// Postgres's (it doesn't compute them there).
    fn measure(&mut self, t: &Table, vacuum: bool) -> Result<f64> {
        let quiet = self.ctx.notices.borrow().len();
        let r = self.measure_quiet(t, vacuum);
        self.ctx.notices.borrow_mut().truncate(quiet);
        r
    }

    /// Rebuilding the indexes of `t` (those in `only`, else all): their expressions are
    /// computed for each row, as the table's owner under a safe search_path.
    pub(super) fn evaluate_index_exprs(&mut self, t: &Table, only: Option<&[u32]>) -> Result<()> {
        let ta = Arc::new(t.clone());
        let bixs = self.binder().bound_indexes(&ta)?;
        let with_exprs: Vec<_> = bixs
            .into_iter()
            .filter(|b| only.is_none_or(|o| o.contains(&b.index.oid)))
            .filter(|b| {
                b.predicate.is_some()
                    || b.keys.iter().any(|k| !matches!(k, crate::sql::expr::Expr::Col(..)))
            })
            .collect();
        if with_exprs.is_empty() {
            return Ok(());
        }
        let rows = self.table_rows(t)?;
        for b in &with_exprs {
            for (_, r) in &rows {
                super::table::index_values(b, r, self)?;
            }
        }
        Ok(())
    }

    fn measure_quiet(&mut self, t: &Table, vacuum: bool) -> Result<f64> {
        let rows: Vec<crate::exec::Row> = self.table_rows(t)?.into_iter().map(|(_, r)| r).collect();
        // (A lazy VACUUM doesn't compute index expressions; when modelling the indexes
        // would, and that fails, the heap isn't modelled.)
        if vacuum && let Some(v) = crate::exec::sizing::vacuum(t, self.ctx).ok().flatten() {
            // The modelled heap, vacuumed: indexes it cleaned up are measured too.
            self.cat.had_triggers.remove(&t.oid);
            self.cat.relstats.insert(
                t.oid,
                RelStats {
                    pages: v.pages,
                    tuples: v.tuples,
                    allvisible: v.allvisible,
                    allfrozen: 0,
                },
            );
            for (oid, pages, tuples) in v.indexes {
                self.cat.relstats.insert(
                    oid,
                    RelStats {
                        pages,
                        tuples,
                        allvisible: 0,
                        allfrozen: 0,
                    },
                );
            }
            return Ok(v.tuples);
        }
        let pages = self.heap_pages(t, &rows);
        let tuples = rows.len() as f64;
        let before = self.cat.relstats.get(&t.oid).map_or(0, |s| s.allvisible);
        let modelled = if vacuum {
            crate::exec::sizing::measured(t, self.ctx).ok().flatten()
        } else {
            crate::exec::sizing::measured(t, self.ctx)?
        };
        let allvisible = match &modelled {
            Some((_, av, _)) if !vacuum => *av,
            _ if vacuum => pages,
            _ => before.min(pages),
        };
        crate::exec::sizing::invalidated(t.oid, &self.ctx.sess.db);
        if vacuum {
            // relhastriggers is cleared once no triggers are left.
            self.cat.had_triggers.remove(&t.oid);
        }
        // ANALYZE keeps the all-frozen count VACUUM (or a restore) last set.
        let allfrozen = if vacuum {
            0
        } else {
            self.cat.relstats.get(&t.oid).map_or(0, |s| s.allfrozen)
        };
        self.cat.relstats.insert(
            t.oid,
            RelStats {
                pages,
                tuples,
                allvisible,
                allfrozen,
            },
        );
        if !vacuum {
            self.measure_indexes(t, &rows, None)?;
        }
        Ok(tuples)
    }

    /// A rewritten table (VACUUM FULL, CLUSTER): heap and every index measured afresh,
    /// nothing all-visible yet.
    fn measure_rewritten(&mut self, t: &Table, order: Option<&[Vec<u8>]>) -> Result<f64> {
        let quiet = self.ctx.notices.borrow().len();
        let r = self.measure_rewritten_quiet(t, order);
        self.ctx.notices.borrow_mut().truncate(quiet);
        r
    }

    fn measure_rewritten_quiet(&mut self, t: &Table, order: Option<&[Vec<u8>]>) -> Result<f64> {
        // The new heap and index files take oids (the TOAST table's contents are swapped,
        // so it keeps its oid).
        if let Some(st) = self.cat.index_table(t.oid) {
            let mut st = (*st).clone();
            self.cat.rewrite_heap(&mut st, true);
            self.ctx.txn.borrow_mut().mark_new_storage(st.oid);
            // copy_table_data freezes everything older than OldestXmin.
            let xmin = self.recent_xmin();
            self.set_frozen_xid(&st, xmin);
            self.cat.put_storage(st);
        }
        crate::exec::sizing::rewritten(t, order, self.ctx)?;
        let rows: Vec<crate::exec::Row> = self.table_rows(t)?.into_iter().map(|(_, r)| r).collect();
        let pages = self.heap_pages(t, &rows);
        let tuples = rows.len() as f64;
        self.cat.relstats.insert(
            t.oid,
            RelStats {
                pages,
                tuples,
                allvisible: 0,
                allfrozen: 0,
            },
        );
        self.measure_indexes(t, &rows, None)?;
        Ok(tuples)
    }

    pub(super) fn cluster(&mut self, s: &pb::ClusterStmt) -> Result<String> {
        let mut verbose = false;
        for d in super::def_elems(&s.params) {
            match d.defname.as_str() {
                "verbose" => verbose = bool_option(d)?,
                o => {
                    return Err(with_pos(
                        Error::new(
                            code::SYNTAX_ERROR,
                            format!("unrecognized CLUSTER option \"{o}\""),
                        ),
                        d.location,
                    ));
                }
            }
        }
        let Some(rv) = &s.relation else {
            // Every table with a clustered index the current role may maintain.
            let mut all: Vec<Arc<Table>> = self.cat.tables.values().cloned().collect();
            all.extend(self.cat.views.values().filter_map(|v| v.storage.clone()));
            for t in all {
                if let Some(ix) = self.clustered_index(&t)
                    && self.may_maintain(t.oid)
                {
                    self.cluster_rel(&t, &ix, verbose)?;
                }
            }
            self.save()?;
            return Ok("CLUSTER".into());
        };
        let t = match self.lookup(rv)? {
            Some(Relation::Table(t)) => t,
            Some(Relation::View(v)) if v.materialized => v
                .storage
                .clone()
                .ok_or_else(|| crate::error::internal("materialized view without storage"))?,
            Some(_) => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a table or materialized view", rv.relname),
                ));
            }
            // A built-in catalog's TOAST table, with its index: built in, so already in order.
            None if self.is_superuser() && builtin_toast_index(rv, &s.indexname) => {
                return Ok("CLUSTER".into());
            }
            None => {
                return Err(crate::catalog::no_such_relation(
                    Self::rv_schema(rv),
                    &rv.relname,
                ));
            }
        };
        self.require_rel_priv(t.oid, crate::catalog::acl::MAINTAIN, false)?;
        self.note_lock(t.oid, super::table::lockmode::ACCESS_EXCLUSIVE)?;
        let ix = if s.indexname.is_empty() {
            self.clustered_index(&t).ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!(
                        "there is no previously clustered index for table \"{}\"",
                        t.name
                    ),
                )
            })?
        } else {
            let ix = self.index_of(&t, &s.indexname)?;
            check_clusterable(&ix)?;
            ix
        };
        if t.partkey.is_some() {
            for leaf in self.cat.leaves(t.oid) {
                let Some(lix) = self
                    .cat
                    .table_indexes(&leaf)
                    .into_iter()
                    .find(|i| self.index_descends(i, ix.oid))
                else {
                    continue;
                };
                // (Partitions the user may not maintain are skipped.)
                if !self.may_maintain(leaf.oid) {
                    self.ctx.notice(crate::session::warning(
                        code::WARNING,
                        format!("permission denied to cluster \"{}\", skipping it", leaf.name),
                    ));
                    continue;
                }
                self.note_lock(leaf.oid, super::table::lockmode::ACCESS_EXCLUSIVE)?;
                self.cluster_rel(&leaf, &lix, verbose)?;
            }
        } else {
            self.cluster_rel(&t, &ix, verbose)?;
        }
        self.save()?;
        Ok("CLUSTER".into())
    }

    /// Whether `ix` is `root` or attached (through partitioned indexes) to it.
    fn index_descends(&self, ix: &Index, root: u32) -> bool {
        let mut cur = Some(ix.oid);
        while let Some(o) = cur {
            if o == root {
                return true;
            }
            cur = self
                .cat
                .indexes
                .get(&o)
                .map(|i| i.parent)
                .filter(|&p| p != 0);
        }
        false
    }

    fn clustered_index(&self, t: &Table) -> Option<Arc<Index>> {
        t.indexes
            .iter()
            .chain(&t.pkey_index)
            .filter_map(|i| self.cat.indexes.get(i))
            .find(|i| i.clustered)
            .cloned()
    }

    /// The index `name` of table `t` (in its schema).
    fn index_of(&self, t: &Table, name: &str) -> Result<Arc<Index>> {
        match self.cat.relation_in(t.schema, name) {
            Some(Relation::Index(i)) if i.table == t.oid => Ok(i),
            Some(Relation::Index(_)) => Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not an index for table \"{}\"", t.name),
            )),
            _ => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("index \"{name}\" for table \"{}\" does not exist", t.name),
            )),
        }
    }

    /// ALTER TABLE ... CLUSTER ON (`Some`) / SET WITHOUT CLUSTER (`None`).
    pub(super) fn cluster_on(&mut self, t: &Table, name: Option<&str>) -> Result<()> {
        let ix = match name {
            Some(n) => {
                let ix = self.index_of(t, n)?;
                check_clusterable(&ix)?;
                Some(ix.oid)
            }
            None => None,
        };
        if t.partkey.is_some() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot mark index clustered in partitioned table",
            ));
        }
        self.mark_clustered(t, ix);
        Ok(())
    }

    fn mark_clustered(&mut self, t: &Table, ix: Option<u32>) {
        for i in t.indexes.iter().chain(&t.pkey_index) {
            if let Some(x) = self.cat.indexes.get(i)
                && x.clustered != (Some(*i) == ix)
            {
                let x = Index {
                    clustered: Some(*i) == ix,
                    ..(**x).clone()
                };
                self.cat.put_index(x);
            }
        }
    }

    /// Rewrite a table in the order of one of its indexes, which becomes its clustered one.
    fn cluster_rel(&mut self, t: &Arc<Table>, ix: &Index, verbose: bool) -> Result<()> {
        // As the table's owner (its index functions run as that user).
        let pushed = t.owner != self.ctx.current_user();
        if pushed {
            self.ctx.sess.definer.borrow_mut().push(t.owner);
            crate::scoped::checkpoint("session::definer");
        }
        let level = self.ctx.sess.settings.borrow_mut().restrict_search_path();
        let r = self.cluster_rel_inner(t, ix, verbose);
        self.ctx.sess.settings.borrow_mut().pop_nest(level);
        if pushed {
            self.ctx.sess.definer.borrow_mut().pop();
        }
        r
    }

    fn cluster_rel_inner(&mut self, t: &Arc<Table>, ix: &Index, verbose: bool) -> Result<()> {
        self.mark_clustered(t, Some(ix.oid));
        if verbose {
            let name = format!(
                "{}.{}",
                crate::catalog::quote_ident(self.cat.schema_name(t.schema)),
                crate::catalog::quote_ident(&t.name)
            );
            let mut e = Error::new(
                code::SUCCESSFUL_COMPLETION,
                format!("clustering \"{name}\" using sequential scan and sort"),
            );
            e.severity = crate::error::Severity::Info;
            self.ctx.notice(e);
        }
        let bixs = self.binder().bound_indexes(t)?;
        let bix = bixs
            .iter()
            .find(|b| b.index.oid == ix.oid)
            .ok_or_else(|| crate::error::internal("index not found"))?;
        // (data key, (index entry key, the index key alone, row)).
        let mut items: Vec<(Vec<u8>, (Vec<u8>, Vec<u8>, crate::exec::Row))> = Vec::new();
        for (k, r) in self.table_rows(t)? {
            let vals = super::table::index_values(bix, &r, self)?.unwrap_or_default();
            let ik = crate::exec::scan::index_entry_key(t, &bix.index, &vals, &k);
            let key = crate::exec::scan::index_value_prefix(t, &bix.index, &vals);
            items.push((k, (ik, key, r)));
        }
        // plan_cluster_use_sort: with index scans disabled, a sequential scan feeds a sort
        // on the index key (tuplesort_begin_cluster), whose quicksort leaves equal keys in
        // the order it does; else the rows come in index order.
        let sort = self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("enable_indexscan")
            .is_some_and(|v| crate::types::text::parse_bool(&v).is_ok_and(|b| !b));
        if sort {
            crate::exec::sizing::in_heap_order(t, &mut items, self.ctx);
            crate::sort::pg_qsort(&mut items, |a, b| a.1.1.cmp(&b.1.1));
        } else {
            items.sort_by(|a, b| a.1.0.cmp(&b.1.0));
        }
        let order: Vec<Vec<u8>> = items.iter().map(|(k, _)| k.clone()).collect();
        if t.has_rowid() {
            // Rows are stored by rowid: new rowids in index order put them in that order.
            crate::exec::dml::truncate(t, self.ctx)?;
            let rows: Vec<crate::exec::Row> = items.into_iter().map(|(_, (_, _, r))| r).collect();
            self.write_new_rows(t, rows)?;
            let built: Vec<Index> = bixs.iter().map(|b| (*b.index).clone()).collect();
            for i in &built {
                self.build_index(t, i)?;
            }
        }
        // Rows re-stored under new keys are in index order already.
        let order = t.storage_pkey().is_some().then_some(order.as_slice());
        let n = self.measure_rewritten(t, order)?;
        if verbose {
            let name = format!(
                "{}.{}",
                crate::catalog::quote_ident(self.cat.schema_name(t.schema)),
                crate::catalog::quote_ident(&t.name)
            );
            let pages = self.cat.relstats.get(&t.oid).map_or(0, |s| s.pages);
            let mut e = Error::new(code::SUCCESSFUL_COMPLETION, format!("\"{name}\": found 0 removable, {n} nonremovable row versions in {pages} pages"))
                .detail("0 dead row versions cannot be removed yet.\nCPU: user: 0.00 s, system: 0.00 s, elapsed: 0.00 s.");
            e.severity = crate::error::Severity::Info;
            self.ctx.notice(e);
        }
        Ok(())
    }

    /// CREATE INDEX measures the new index and its table (an empty table that was never
    /// measured keeps its unknown tuple count).
    pub(super) fn index_built(
        &mut self,
        t: &Table,
        index: u32,
        rows: Vec<crate::exec::Row>,
    ) -> Result<()> {
        // (Modelling the index's pages evaluates its expressions again: what they say then
        // isn't Postgres's.)
        let quiet = self.ctx.notices.borrow().len();
        let r = self.index_built_measured(t, index, rows);
        self.ctx.notices.borrow_mut().truncate(quiet);
        r
    }

    fn index_built_measured(
        &mut self,
        t: &Table,
        index: u32,
        rows: Vec<crate::exec::Row>,
    ) -> Result<()> {
        crate::exec::sizing::index_built(t, index, self.ctx)?;
        // index_update_stats leaves the table's statistics alone when autovacuum is off
        // for it (a dump restoring statistics before its indexes keeps them).
        let autovacuum_off = t.options.iter().any(|o| {
            o.split_once('=').is_some_and(|(k, v)| {
                k == "autovacuum_enabled"
                    && crate::types::text::parse_bool(v.trim_matches('\'')).is_ok_and(|b| !b)
            })
        });
        let before = self.cat.relstats.get(&t.oid).copied();
        if !autovacuum_off && (!rows.is_empty() || before.is_some_and(|s| s.tuples >= 0.0)) {
            let pages = self.heap_pages(t, &rows);
            self.cat.relstats.insert(
                t.oid,
                RelStats {
                    pages,
                    tuples: rows.len() as f64,
                    allvisible: before.map_or(0, |s| s.allvisible).min(pages),
                    allfrozen: before.map_or(0, |s| s.allfrozen).min(pages),
                },
            );
        }
        self.measure_indexes(t, &rows, Some(index))
    }

    fn heap_pages(&self, t: &Table, rows: &[crate::exec::Row]) -> i32 {
        let blocks = crate::exec::sizing::heap_blocks(t, self.ctx);
        let db = &self.ctx.sess.db;
        if !crate::exec::sizing::is_estimated(t.oid, db)
            && let Ok(Some(n)) = blocks
        {
            return n as i32;
        }
        let types: Vec<DataType> = t.columns.iter().map(|c| c.ty.clone()).collect();
        let fmt = self.ctx.fmt().clone();
        let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, &fmt);
        // Virtual generated columns are stored as nulls.
        let virt: Vec<usize> = t
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.virtual_gen)
            .map(|(i, _)| i)
            .collect();
        let live = relsize::heap_pages(rows.iter().map(|r| {
            if virt.is_empty() {
                return relsize::heap_tuple(&types, r, &text);
            }
            let mut r = r.clone();
            for &i in &virt {
                if i < r.len() {
                    r[i] = Value::Null;
                }
            }
            relsize::heap_tuple(&types, &r, &text)
        }));
        // A table too big to model: its file keeps its estimated pages.
        crate::exec::sizing::measured_live(t.oid, live.max(0) as u32, db).map_or(live, |p| p as i32)
    }

    /// pg_class stats of a table's indexes (or just `only`) over its rows.
    fn measure_indexes(
        &mut self,
        t: &Table,
        rows: &[crate::exec::Row],
        only: Option<u32>,
    ) -> Result<()> {
        let bixs = self.binder().bound_indexes(&Arc::new(t.clone()))?;
        let fmt = self.ctx.fmt().clone();
        let text = |v: &Value, ty: &DataType| crate::types::text::format_value(v, ty, &fmt);
        for bix in bixs
            .into_iter()
            .filter(|b| only.is_none_or(|o| o == b.index.oid))
        {
            let mut types: Vec<DataType> = bix.keys.iter().map(|k| k.ty()).collect();
            types.extend(bix.index.include.iter().map(|&i| t.columns[i].ty.clone()));
            let mut keys: Vec<(Vec<u8>, usize)> = Vec::new();
            for row in rows {
                let Some(mut vals) = super::table::index_values(&bix, row, self)? else {
                    continue;
                };
                let sort = super::table::key_bytes(&vals);
                vals.extend(bix.index.include.iter().map(|&i| row[i].clone()));
                keys.push((sort, relsize::index_tuple(&types, &vals, &text)));
            }
            keys.sort();
            let dedup = !bix.index.unique
                && bix.index.include.is_empty()
                && types.iter().all(relsize::equal_image);
            let mut sizes = Vec::with_capacity(keys.len());
            for group in keys.chunk_by(|a, b| dedup && a.0 == b.0) {
                relsize::posting_tuples(group[0].1, group.len(), &mut sizes);
            }
            let tuples = keys.len() as f64;
            let modelled = crate::exec::sizing::btree_blocks(bix.index.oid, &self.ctx.sess.db);
            self.cat.relstats.insert(
                bix.index.oid,
                RelStats {
                    pages: modelled.map_or_else(|| relsize::btree_pages(&sizes), |n| n as i32),
                    tuples,
                    allvisible: 0,
                    allfrozen: 0,
                },
            );
        }
        Ok(())
    }
}

/// A boolean VACUUM option's value (no argument: true).
fn bool_option(d: &pb::DefElem) -> Result<bool> {
    let Some(arg) = d.arg.as_deref() else {
        return Ok(true);
    };
    let v = match inner(arg) {
        Some(NodeEnum::Integer(i)) => match i.ival {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        },
        _ => const_str(arg).and_then(|s| match s.to_ascii_lowercase().as_str() {
            "true" | "on" => Some(true),
            "false" | "off" => Some(false),
            _ => None,
        }),
    };
    v.ok_or_else(|| {
        Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires a Boolean value", d.defname),
        )
    })
}

/// CLUSTER needs a valid, complete btree index.
fn check_clusterable(ix: &Index) -> Result<()> {
    if ix.method != "btree" {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!(
                "cannot cluster on index \"{}\" because access method does not support clustering",
                ix.name
            ),
        ));
    }
    if ix.predicate.is_some() {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("cannot cluster on partial index \"{}\"", ix.name),
        ));
    }
    if !ix.valid {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("cannot cluster on invalid index \"{}\"", ix.name),
        ));
    }
    Ok(())
}

/// `pg_toast.pg_toast_N` of a built-in catalog, with `index` its TOAST index (or none given).
fn builtin_toast_index(rv: &pb::RangeVar, index: &str) -> bool {
    use crate::catalog::static_rows::{oid_at, rows};
    use crate::catalog::system::SysTable;
    const PG_TOAST_NAMESPACE: u32 = 99;
    if rv.schemaname != "pg_toast" || !rv.relname.starts_with("pg_toast_") {
        return false;
    }
    let class = rows(SysTable::PgClass);
    let named = |name: &str, kind: u8| {
        class.iter().any(|r| {
            matches!(&r[1], Value::Text(n) if &**n == name)
                && oid_at(r, 2) == PG_TOAST_NAMESPACE
                && match &r[17] {
                    Value::Char(k) => *k == kind,
                    Value::Text(k) => k.as_bytes() == [kind],
                    _ => false,
                }
        })
    };
    named(&rv.relname, b't')
        && (index.is_empty() || (index == format!("{}_index", rv.relname) && named(index, b'i')))
}
