//! pg_stat_statements' hooks into statement execution (`pgss_post_parse_analyze`,
//! `pgss_planner`, `pgss_ExecutorEnd`, `pgss_ProcessUtility`), for a cluster that preloads
//! the library. Nothing runs otherwise.

use super::{Outcome, Prepared, Session};
use crate::ext::pg_stat_statements::{self as pgss, Kind, Stmt, Usage, jumble};
use pg_query::NodeEnum;
use std::sync::Arc;

/// A statement being tracked: its entry and what to count when it ends.
pub(super) struct Tracked {
    id: i64,
    jumble: jumble::Jumble,
    text: Arc<str>,
    location: i32,
    len: i32,
    utility: bool,
    /// A query nested in a utility statement (EXPLAIN's, CREATE TABLE AS's, ...), recorded
    /// one level down with the whole query string as its text.
    nested: Option<(NodeEnum, Arc<str>)>,
    started: std::time::Instant,
    /// Its planning was counted (once per execution).
    planned: bool,
    /// The table it writes (oid, temporary) and that table's blocks before it ran, then the
    /// blocks it added.
    target: Option<(u32, bool)>,
    blocks_before: Option<i64>,
    blocks_added: i64,
}

/// The table an INSERT / UPDATE / DELETE / MERGE writes.
fn target_rel(node: &NodeEnum) -> Option<&pg_query::protobuf::RangeVar> {
    match node {
        NodeEnum::InsertStmt(s) => s.relation.as_ref(),
        NodeEnum::UpdateStmt(s) => s.relation.as_ref(),
        NodeEnum::DeleteStmt(s) => s.relation.as_ref(),
        NodeEnum::MergeStmt(s) => s.relation.as_ref(),
        _ => None,
    }
}

/// `pgss_ProcessUtility`'s row count: COPY, FETCH, SELECT (CREATE TABLE AS) and REFRESH
/// MATERIALIZED VIEW report theirs.
fn utility_rows(tag: &str) -> u64 {
    let counted = ["COPY", "FETCH", "SELECT", "REFRESH"].iter().any(|t| tag.starts_with(t));
    if counted { tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0) } else { 0 }
}

/// A query's processed rows (es_total_processed): its command tag's count.
fn query_rows(tag: &str) -> u64 {
    tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

/// The query a utility statement runs one level down, as the executor sees it.
fn nested_query(node: &NodeEnum) -> Option<NodeEnum> {
    let q = match node {
        NodeEnum::ExplainStmt(e) => e.query.as_deref(),
        NodeEnum::CreateTableAsStmt(c) if c.objtype != pg_query::protobuf::ObjectType::ObjectMatview as i32 => {
            c.query.as_deref()
        }
        NodeEnum::DeclareCursorStmt(d) => d.query.as_deref(),
        NodeEnum::CopyStmt(c) => c.query.as_deref(),
        _ => None,
    }?;
    let n = q.node.clone()?;
    match &n {
        NodeEnum::ExplainStmt(_) | NodeEnum::CreateTableAsStmt(_) => nested_query(&n),
        n if jumble::is_query(n) => Some(n.clone()),
        _ => None,
    }
}

use crate::ext::pg_stat_statements::jumble_in;

impl Session {
    /// The query string statement `p` came in, when this session's source is it.
    fn pgss_source(&self, p: &Prepared) -> Arc<str> {
        if self.source.get(p.offset..).is_some_and(|rest| rest.starts_with(p.sql.as_str())) {
            Arc::from(self.source.as_str())
        } else {
            Arc::from(p.placed_sql().as_ref())
        }
    }

    /// Before statement `p` runs (at the top level): parse analysis's entry of a statement
    /// with constants, planning, and what its end will count.
    pub(super) fn pgss_begin(&mut self, p: &Prepared) -> Option<Tracked> {
        self.state.cluster.statements()?;
        let node = p.stmt.as_ref()?.node.as_ref()?;
        let level = self.state.pgss.borrow().level;
        if level > 0 {
            return None;
        }
        let source = self.pgss_source(p);
        let (location, mut len) = (p.offset as i32, p.sql.len() as i32);
        // A Parse message's whole string: the statement ends before its `;`.
        if p.sql.trim_end().ends_with(';')
            && let Ok(parts) = pg_query::split_with_scanner(&p.sql)
            && let Some(first) = parts.first()
        {
            len = (first.as_ptr() as usize - p.sql.as_ptr() as usize + first.len()) as i32;
        }
        let st = |text: &Arc<str>, location: i32, len: i32| (text.clone(), location, len);
        let enabled = pgss::enabled(&self.state, level);
        let started = std::time::Instant::now();
        match node {
            // EXECUTE: the prepared query runs, under its PREPARE's entry.
            NodeEnum::ExecuteStmt(e) => {
                let (id, text) = self.state.pgss.borrow().prepared.get(&e.name).cloned()?;
                if !enabled {
                    return None;
                }
                let len = text.len() as i32;
                Some(Tracked { id, jumble: jumble::Jumble::default(), text, location: 0, len, utility: false, nested: None, started, planned: false, target: None, blocks_before: None, blocks_added: 0 })
            }
            // PREPARE: its query's entry (the PREPARE's text), made when it has constants.
            NodeEnum::PrepareStmt(ps) => {
                let q = ps.query.as_deref()?.node.as_ref()?;
                let (id, j) = jumble_in(&self.state, q);
                let (text, loc, l) = st(&source, location, len);
                let stmt = Stmt { source: &text, location: loc, len: l };
                let (clean, _) = pgss::clean(&stmt);
                if enabled && !j.locs.is_empty() {
                    pgss::store(&self.state, &stmt, id, &j, level, None, Usage::default());
                }
                let shown: Arc<str> = match self.state.cluster.statements().and_then(|s| {
                    s.entries().into_iter().find(|(k, _)| k.queryid == id).map(|(_, e)| e.query)
                }) {
                    Some(q) => q,
                    None => Arc::from(clean),
                };
                self.state.pgss.borrow_mut().prepared.insert(ps.name.clone(), (id, shown));
                None
            }
            n => {
                let utility = !jumble::is_query(n);
                let (id, j) = jumble_in(&self.state, n);
                let stmt = Stmt { source: &source, location, len };
                if enabled && !j.locs.is_empty() {
                    pgss::store(&self.state, &stmt, id, &j, level, None, Usage::default());
                }
                if !enabled || (utility && !pgss::track_utility(&self.state)) {
                    return None;
                }
                let nested = if utility { nested_query(n).map(|q| (q, source.clone())) } else { None };
                let target = target_rel(n).and_then(|rv| {
                    let cat = self.state.db.catalog();
                    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
                    match cat.lookup(schema, &rv.relname, &self.state.search_path()).ok()?? {
                        crate::catalog::Relation::Table(t) => Some((t.oid, t.temporary)),
                        _ => None,
                    }
                });
                Some(Tracked { id, jumble: j, text: source, location, len, utility, nested, started, planned: false, target, blocks_before: None, blocks_added: 0 })
            }
        }
    }

    /// The running top-level query was planned (`pgss_planner`): counted with
    /// track_planning, before it runs.
    pub(super) fn pgss_planned(&mut self, took: std::time::Duration) {
        let Some(t) = self.pgss_tracked.as_mut() else { return };
        if t.utility || t.planned || !pgss::track_planning(&self.state) {
            return;
        }
        t.planned = true;
        let t = self.pgss_tracked.take().expect("tracked");
        let usage = Usage { millis: took.as_secs_f64() * 1000.0, ..Default::default() };
        self.pgss_store(&t, 0, Some(Kind::Plan), usage);
        self.pgss_tracked = Some(t);
    }

    /// The written table's size in blocks, in the open transaction.
    fn pgss_blocks(&self, oid: u32) -> Option<i64> {
        let txn = self.txn.as_ref()?;
        let ctx = crate::exec::ExecCtx::with_search_path(txn, &self.state, &[], self.txn_time, Self::now(), self.state.search_path());
        let bytes = crate::exec::sizing::relation_size(oid, crate::exec::sizing::Fork::Main, &ctx).ok()??;
        Some(bytes / 8192)
    }

    /// The running query's table, measured before it writes (`after`: and after).
    pub(super) fn pgss_measure(&mut self, after: bool) {
        let Some((oid, _)) = self.pgss_tracked.as_ref().and_then(|t| t.target) else { return };
        let n = self.pgss_blocks(oid);
        let Some(t) = self.pgss_tracked.as_mut() else { return };
        if !after {
            if t.blocks_before.is_none() {
                t.blocks_before = n;
            }
        } else if let (Some(b), Some(a)) = (t.blocks_before, n) {
            t.blocks_added += (a - b).max(0);
            t.blocks_before = Some(a);
        }
    }

    fn pgss_store(&self, t: &Tracked, level: u32, kind: Option<Kind>, usage: Usage) {
        let stmt = Stmt { source: &t.text, location: t.location, len: t.len };
        pgss::store(&self.state, &stmt, t.id, &t.jumble, level, kind, usage);
    }

    /// After a tracked statement ran: its execution counted (a failed one isn't).
    pub(super) fn pgss_end(&mut self, t: Tracked, r: &crate::Result<Outcome>) {
        let Ok(out) = r else { return };
        let tag = match out {
            Outcome::Done(q) => q.tag.clone(),
            Outcome::CopyOut(c) => c.tag.clone(),
            // (COPY FROM STDIN ends when its data has come: `pgss_copy_done`.)
            Outcome::CopyIn(_) => {
                self.pgss_copy = Some(t);
                return;
            }
        };
        self.pgss_finish(t, &tag);
    }

    /// COPY FROM STDIN finished: the statement counted with its rows.
    pub(super) fn pgss_copy_done(&mut self, r: &crate::Result<super::QueryResult>) {
        if let (Some(t), Ok(q)) = (self.pgss_copy.take(), r) {
            let tag = q.tag.clone();
            self.pgss_finish(t, &tag);
        }
    }

    fn pgss_finish(&mut self, t: Tracked, tag: &str) {
        let millis = t.started.elapsed().as_secs_f64() * 1000.0;
        let shown = self.state.pgss.borrow_mut().utility_rows.take();
        let rows = if t.utility {
            if tag.starts_with("REFRESH") { shown.unwrap_or(0) } else { utility_rows(tag) }
        } else {
            query_rows(tag)
        };
        let writes = !t.utility && ["INSERT", "UPDATE", "DELETE", "MERGE"].iter().any(|v| tag.starts_with(v)) && rows > 0;
        let usage = Usage {
            millis,
            rows,
            wal_records: if writes { rows as i64 } else { 0 },
            wal_bytes: if writes { rows * 64 } else { 0 },
            shared_blks: if t.target.is_some_and(|x| !x.1) { t.blocks_added } else { 0 },
            local_blks: if t.target.is_some_and(|x| x.1) { t.blocks_added } else { 0 },
            workers_to_launch: self.pgss_workers * 2,
            workers_launched: self.pgss_workers * 2,
        };
        // The query a utility statement ran, one level down (track = all).
        if let Some((q, source)) = &t.nested
            && pgss::enabled(&self.state, 1)
        {
            let (id, j) = jumble_in(&self.state, q);
            let stmt = Stmt { source, location: -1, len: 0 };
            if !j.locs.is_empty() {
                pgss::store(&self.state, &stmt, id, &j, 1, None, Usage::default());
            }
            let nested_rows = match q {
                _ if tag.starts_with("EXPLAIN") => 0,
                _ => rows,
            };
            pgss::store(&self.state, &stmt, id, &j, 1, Some(Kind::Exec), Usage { millis, rows: nested_rows, ..Default::default() });
        }
        self.pgss_store(&t, 0, Some(Kind::Exec), usage);
    }
}
