//! VACUUM (VERBOSE) and ANALYZE (VERBOSE): heap_vacuum_rel's, do_analyze_rel's and
//! acquire_sample_rows' INFO reports. Pages, tuples, the visibility map bits and the
//! indexes' pages are the heap and index models' (`sizing::VacuumReport`); the buffer, WAL
//! and I/O figures of PostgreSQL's buffer manager the engine doesn't have are 0, as in
//! log_autovacuum_min_duration's report (`autovacuum::log_report`).

use super::Ddl;
use crate::catalog::Table;
use crate::error::{Error, Result, code};
use crate::exec::sizing::VacuumReport;

/// When a report's work began: the clock and the thread's CPU times.
#[derive(Clone, Copy)]
pub(super) struct Began {
    at: std::time::Instant,
    cpu: (i64, i64),
}

impl Began {
    pub(super) fn now() -> Began {
        Began { at: std::time::Instant::now(), cpu: crate::autovacuum::thread_cpu() }
    }

    /// pg_rusage_show's "CPU: user: … s, system: … s, elapsed: … s".
    fn usage(&self) -> String {
        let (u, s) = crate::autovacuum::thread_cpu();
        let cs = |us: i64| format!("{}.{:02}", us / 1_000_000, us % 1_000_000 / 10_000);
        format!(
            "CPU: user: {} s, system: {} s, elapsed: {} s",
            cs(u - self.cpu.0),
            cs(s - self.cpu.1),
            cs(self.at.elapsed().as_micros() as i64)
        )
    }
}

/// The relfrozenxid a VACUUM found and left, when it advanced it.
pub(super) type FrozenXid = Option<(u64, u64)>;

/// An INFO message (SQLSTATE 00000) from `routine`.
fn info(msg: String, routine: &'static str) -> Error {
    let mut e = Error::new(code::SUCCESSFUL_COMPLETION, msg).routine(routine);
    e.severity = crate::error::Severity::Info;
    e
}

impl Ddl<'_, '_> {
    fn send_info(&self, msg: String, routine: &'static str) {
        self.ctx.notice(info(msg, routine));
    }

    /// The I/O lines that end both reports: the timing settings' lines, then the rates,
    /// buffers and WAL (none of PostgreSQL's buffer manager here), then the CPU used.
    fn io_lines(&self, buf: &mut String, delay_ns: i64, began: &Began) {
        let st = self.ctx.sess.settings.borrow();
        if st.bool("track_cost_delay_timing") {
            buf.push_str(&format!("delay time: {:.3} ms\n", delay_ns as f64 / 1_000_000.0));
        }
        if st.bool("track_io_timing") {
            buf.push_str("I/O timings: read: 0.000 ms, write: 0.000 ms\n");
        }
        buf.push_str(
            "avg read rate: 0.000 MB/s, avg write rate: 0.000 MB/s\n\
             buffer usage: 0 hits, 0 reads, 0 dirtied\n\
             WAL usage: 0 records, 0 full page images, 0 bytes, 0 buffers full\n",
        );
        buf.push_str("system usage: ");
        buf.push_str(&began.usage());
    }

    /// heap_vacuum_rel's first message.
    pub(super) fn vacuuming_info(&self, schema: &str, rel: &str, aggressive: bool) {
        let db = self.ctx.sess.database.as_str();
        let msg = if aggressive {
            format!("aggressively vacuuming \"{db}.{schema}.{rel}\"")
        } else {
            format!("vacuuming \"{db}.{schema}.{rel}\"")
        };
        self.send_info(msg, "heap_vacuum_rel");
    }

    /// heap_vacuum_rel's report of a heap `rel` (the table's or its TOAST table's) once
    /// VACUUM is done with it: lazy_truncate_heap's message, then the report. `r` None: the
    /// heap isn't modelled (too big): its pages and tuples as pg_class has them now.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn vacuumed_info(
        &self,
        oid: u32,
        schema: &str,
        rel: &str,
        r: Option<&VacuumReport>,
        cutoff: u64,
        frozen: FrozenXid,
        delay_ns: i64,
        began: &Began,
    ) {
        let db = self.ctx.sess.database.as_str();
        let fallback;
        let r = match r {
            Some(r) => r,
            None => {
                let s = self.cat.rel_stats(oid, 'r');
                let mut v = VacuumReport { remain: s.tuples.max(0.0), ..Default::default() };
                v.heap.rel_pages = s.pages.max(0) as u32;
                v.heap.pages = v.heap.rel_pages;
                v.heap.scanned = v.heap.rel_pages;
                fallback = v;
                &fallback
            }
        };
        let h = &r.heap;
        if h.pages < h.rel_pages {
            // (vacuum_error_callback's context, phase truncate.)
            let mut e = info(format!("table \"{rel}\": truncated {} to {} pages", h.rel_pages, h.pages), "lazy_truncate_heap");
            e.context = Some(format!("while truncating relation \"{schema}.{rel}\" to {} blocks", h.pages).into_boxed_str());
            self.ctx.notice(e);
        }
        let pct = |n: u64| if h.rel_pages == 0 { 100.0 } else { 100.0 * n as f64 / h.rel_pages as f64 };
        let mut buf = format!("finished vacuuming \"{db}.{schema}.{rel}\": index scans: {}\n", h.index_scans);
        buf.push_str(&format!(
            "pages: {} removed, {} remain, {} scanned ({:.2}% of total), 0 eagerly scanned\n",
            h.rel_pages - h.pages,
            h.pages,
            h.scanned,
            pct(h.scanned as u64)
        ));
        buf.push_str(&format!(
            "tuples: {} removed, {} remain, {} are dead but not yet removable\n",
            h.tuples_deleted, r.remain as i64, h.recently_dead
        ));
        let next = self.ctx.sess.db.xids().next();
        buf.push_str(&format!(
            "removable cutoff: {}, which was {} XIDs old when operation ended\n",
            cutoff as u32,
            next.saturating_sub(cutoff) as i32
        ));
        if let Some((old, new)) = frozen {
            buf.push_str(&format!(
                "new relfrozenxid: {}, which is {} XIDs ahead of previous value\n",
                new as u32,
                new.wrapping_sub(old) as i32
            ));
        }
        buf.push_str(&format!(
            "frozen: {} pages from table ({:.2}% of total) had {} tuples frozen\n",
            h.frozen_pages,
            pct(h.frozen_pages as u64),
            h.tuples_frozen
        ));
        buf.push_str(&format!(
            "visibility map: {} pages set all-visible, {} pages set all-frozen ({} were all-visible)\n",
            h.vm_visible,
            h.vm_visible_frozen + h.vm_frozen,
            h.vm_frozen
        ));
        let (lead, verb) = if h.bypassed {
            ("index scan bypassed: ", "have")
        } else if h.index_scans == 0 {
            ("index scan not needed: ", "had")
        } else {
            ("index scan needed: ", "had")
        };
        let removed = if verb == "had" { " removed" } else { "" };
        buf.push_str(&format!(
            "{lead}{} pages from table ({:.2}% of total) {verb} {} dead item identifiers{removed}\n",
            h.lpdead_pages,
            pct(h.lpdead_pages as u64),
            h.lpdead_items
        ));
        for &(ix, pages, newly, deleted, free) in &r.indexes {
            let name = match self.cat.indexes.get(&ix) {
                Some(i) => i.name.clone(),
                // (The TOAST table's index.)
                None => match self.cat.tables.values().find(|t| t.toast != 0 && t.toast + 1 == ix) {
                    Some(t) => t.toast_relname(true),
                    None => continue,
                },
            };
            buf.push_str(&format!(
                "index \"{name}\": pages: {pages} in total, {newly} newly deleted, {deleted} currently deleted, {free} reusable\n"
            ));
        }
        self.io_lines(&mut buf, delay_ns, began);
        self.send_info(buf, "heap_vacuum_rel");
    }

    /// do_analyze_rel's first message (`inh`: of the inheritance tree).
    pub(super) fn analyzing_info(&self, t: &Table, inh: bool) {
        let schema = self.cat.schema_name(t.schema);
        let tree = if inh { " inheritance tree" } else { "" };
        self.send_info(format!("analyzing \"{schema}.{}\"{tree}", t.name), "do_analyze_rel");
    }

    /// acquire_sample_rows' message of `t`, sampled for at most `targrows` rows: its pages
    /// (all, up to `targrows` of them, read), the live and dead rows on those, the rows the
    /// sample took and the table's estimated rows. Returns the rows the sample took.
    pub(super) fn sample_info(&mut self, t: &Table, targrows: usize, live: Option<u64>) -> Result<usize> {
        let blocks = match crate::exec::sizing::heap_blocks(t, self.ctx)? {
            Some(b) => b as u64,
            None => self.cat.relstats.get(&t.oid).map_or(0, |s| s.pages.max(0) as u64),
        };
        let stats = crate::exec::sizing::heap_tuple_stats(t.oid, self.ctx)?;
        let live = match (live, stats) {
            (Some(n), _) => n,
            (None, Some(s)) => s.0,
            (None, None) => self.count_rows(t)? as u64,
        };
        let dead = stats.map_or(0, |s| s.2);
        let scanned = blocks.min(targrows as u64);
        // (The rows on the pages read, the rest's estimated from them.)
        let (live_seen, dead_seen, total) = if scanned >= blocks {
            (live, dead, live)
        } else {
            let f = scanned as f64 / blocks as f64;
            let seen = (live as f64 * f).round();
            (seen as u64, (dead as f64 * f).round() as u64, (seen / scanned as f64 * blocks as f64 + 0.5).floor() as u64)
        };
        let rows = (live_seen as usize).min(targrows);
        self.send_info(
            format!(
                "\"{}\": scanned {scanned} of {blocks} pages, containing {live_seen} live rows and {dead_seen} dead rows; {rows} rows in sample, {total} estimated total rows",
                t.name
            ),
            "acquire_sample_rows",
        );
        Ok(rows)
    }

    /// acquire_inherited_sample_rows' messages for `t`'s inheritance tree: each member with
    /// storage and pages sampled in proportion to its pages, or why none is.
    pub(super) fn inherited_sample_info(&mut self, t: &Table) -> Result<()> {
        let members: Vec<std::sync::Arc<Table>> = self
            .cat
            .inheritors(t.oid)
            .into_iter()
            .filter_map(|o| self.cat.tables.get(&o).cloned())
            .collect();
        let schema = self.cat.schema_name(t.schema).to_string();
        if members.len() < 2 {
            self.send_info(
                format!("skipping analyze of \"{schema}.{}\" inheritance tree --- this inheritance tree contains no child tables", t.name),
                "acquire_inherited_sample_rows",
            );
            return Ok(());
        }
        let mut sized = Vec::new();
        for m in members.iter().filter(|m| m.partkey.is_none() && !m.is_foreign()) {
            let blocks = crate::exec::sizing::heap_blocks(m, self.ctx)?
                .map(|b| b as u64)
                .unwrap_or_else(|| self.cat.relstats.get(&m.oid).map_or(0, |s| s.pages.max(0) as u64));
            sized.push((m.clone(), blocks));
        }
        if sized.is_empty() {
            self.send_info(
                format!("skipping analyze of \"{schema}.{}\" inheritance tree --- this inheritance tree contains no analyzable child tables", t.name),
                "acquire_inherited_sample_rows",
            );
            return Ok(());
        }
        let targrows = self.targrows;
        let total: u64 = sized.iter().map(|s| s.1).sum();
        let mut numrows = 0usize;
        for (m, blocks) in sized {
            if blocks == 0 {
                continue;
            }
            let want = ((targrows as f64 * blocks as f64 / total as f64).round() as usize).min(targrows - numrows);
            if want > 0 {
                numrows += self.sample_info(&m, want, None)?;
            }
        }
        Ok(())
    }

    /// copy_table_data's first message: VACUUM FULL's (`how` None), or CLUSTER's with how
    /// it reads the table ("using sequential scan and sort").
    pub(super) fn send_rewrite_start(&self, t: &Table, how: Option<&str>) {
        let schema = self.cat.schema_name(t.schema);
        let msg = match how {
            None => format!("vacuuming \"{schema}.{}\"", t.name),
            Some(how) => format!("clustering \"{schema}.{}\" {how}", t.name),
        };
        self.send_info(msg, "copy_table_data");
    }

    /// copy_table_data's closing message: the old heap's `pages`, the versions it left out
    /// (`dead`: the heap model's dead and recently dead ones) and the `live` ones it kept.
    pub(super) fn send_rewrite_end(&self, t: &Table, pages: i64, dead: Option<(u64, u64)>, live: f64, began: &Began) {
        let schema = self.cat.schema_name(t.schema);
        let (removable, recently) = dead.unwrap_or((0, 0));
        let e = info(
            format!(
                "\"{schema}.{}\": found {removable} removable, {} nonremovable row versions in {pages} pages",
                t.name,
                live.max(0.0) as u64 + recently
            ),
            "copy_table_data",
        )
        .detail(format!("{recently} dead row versions cannot be removed yet.\n{}.", began.usage()));
        self.ctx.notice(e);
    }

    /// do_analyze_rel's closing report.
    pub(super) fn analyzed_info(&self, t: &Table, began: &Began) {
        let db = self.ctx.sess.database.as_str();
        let schema = self.cat.schema_name(t.schema);
        let mut buf = format!("finished analyzing table \"{db}.{schema}.{}\"\n", t.name);
        self.io_lines(&mut buf, 0, began);
        self.send_info(buf, "do_analyze_rel");
    }
}
