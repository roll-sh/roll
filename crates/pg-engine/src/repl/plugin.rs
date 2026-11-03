//! Driving an output plugin over the change stream, as logical.c's callback wrappers and
//! reorderbuffer.c's replay do: a committed transaction's changes between BEGIN and COMMIT,
//! streamed in blocks once it outgrows logical_decoding_work_mem (when the plugin and the
//! client stream), prepared transactions at PREPARE with two-phase decoding, origins
//! filtered by the plugin.

use super::decode::{self, Change};
use super::log::{Kind, Marker, Rec, RelDesc};
use super::slot::SlotStats;
use crate::error::{Error, Result, code};
use crate::txn::Database;
use crate::types::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// A transaction as callbacks see it (ReorderBufferTXN).
#[derive(Clone, Debug, Default)]
pub struct TxnInfo {
    pub xid: u64,
    pub first_lsn: u64,
    /// The commit (or prepare) record's position.
    pub final_lsn: u64,
    pub end_lsn: u64,
    /// Commit (or prepare) time.
    pub time: i64,
    pub origin: u16,
    pub origin_lsn: u64,
    pub gid: String,
    pub prepare_time: i64,
    pub prepare_end_lsn: u64,
}

/// One change of a row (ReorderBufferChange's tuples).
pub enum RowChange<'a> {
    Insert { new: &'a [Value] },
    Update { old: Option<(&'a [Value], bool)>, new: &'a [Value], unchanged_toast: &'a [usize] },
    Delete { old: Option<(&'a [Value], bool)> },
}

/// What a plugin writes (OutputPluginWrite): data at a position, for a transaction.
pub struct Output {
    pub lsn: u64,
    pub xid: u64,
    pub data: Vec<u8>,
}

/// LogicalDecodingContext, as plugins use it.
pub struct Ctx<'a> {
    pub db: &'a Arc<Database>,
    pub fmt: crate::types::text::FormatCtx,
    /// The relations described so far.
    pub rels: HashMap<u32, Arc<RelDesc>>,
    pub write_lsn: u64,
    pub write_xid: u64,
    out: Vec<Output>,
    /// Streaming of in-progress transactions is on (ctx->streaming).
    pub streaming: bool,
    /// Prepared transactions are decoded at PREPARE (ctx->twophase).
    pub twophase: bool,
    /// The aborted subtransaction the change being output belongs to (0: none).
    pub sub_xid: u64,
    /// Evaluates a publication's row filter over a row (the walsender's session; none:
    /// every row passes).
    pub filter: Option<&'a mut RowFilterFn<'a>>,
}

/// A row filter evaluator: (relation, the filter's SQL, the row) -> whether it passes.
pub type RowFilterFn<'f> = dyn FnMut(&RelDesc, &str, &[Value]) -> Result<bool> + 'f;

impl Ctx<'_> {
    pub fn write(&mut self, data: Vec<u8>) {
        self.out.push(Output { lsn: self.write_lsn, xid: self.write_xid, data });
    }

    pub fn take(&mut self) -> Vec<Output> {
        std::mem::take(&mut self.out)
    }
}

/// What a plugin's startup tells the driver (OutputPluginOptions and the context flags).
#[derive(Default)]
pub struct Startup {
    pub binary: bool,
    /// The plugin streams in-progress transactions (it has the callbacks, and the client
    /// asked for it).
    pub streaming: bool,
    /// The client asked for two-phase decoding (pgoutput's two_phase option).
    pub twophase_opt: bool,
    /// Table rewrites' rows are decoded too (ctx->output_rewrites: test_decoding's
    /// include-rewrites).
    pub output_rewrites: bool,
}

/// An output plugin's callbacks (OutputPluginCallbacks).
#[allow(clippy::too_many_arguments)]
pub trait Plugin {
    fn startup(&mut self, opts: &[(String, Option<String>)], is_init: bool, s: &mut Startup) -> Result<()>;
    fn begin(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()>;
    fn commit(&mut self, c: &mut Ctx, t: &TxnInfo, commit_lsn: u64) -> Result<()>;
    fn change(&mut self, c: &mut Ctx, t: &TxnInfo, rel: &Arc<RelDesc>, ch: &RowChange) -> Result<()>;
    fn truncate(&mut self, c: &mut Ctx, t: &TxnInfo, rels: &[Arc<RelDesc>], cascade: bool, restart: bool) -> Result<()>;
    fn message(&mut self, c: &mut Ctx, t: Option<&TxnInfo>, lsn: u64, transactional: bool, prefix: &str, content: &[u8]) -> Result<()>;
    fn filter_by_origin(&mut self, _origin: u16) -> bool {
        false
    }
    fn filter_prepare(&mut self, _xid: u64, _gid: &str) -> bool {
        false
    }
    fn begin_prepare(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()>;
    fn prepare(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64) -> Result<()>;
    fn commit_prepared(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64) -> Result<()>;
    fn rollback_prepared(&mut self, c: &mut Ctx, t: &TxnInfo, prepare_end_lsn: u64, prepare_time: i64) -> Result<()>;
    fn stream_start(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()>;
    fn stream_stop(&mut self, c: &mut Ctx, t: &TxnInfo) -> Result<()>;
    fn stream_abort(&mut self, c: &mut Ctx, t: &TxnInfo, sub_xid: u64, abort_lsn: u64, abort_time: i64) -> Result<()>;
    fn stream_prepare(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64) -> Result<()>;
    fn stream_commit(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64) -> Result<()>;
    fn stream_change(&mut self, c: &mut Ctx, t: &TxnInfo, rel: &Arc<RelDesc>, ch: &RowChange) -> Result<()>;
    fn stream_message(&mut self, c: &mut Ctx, t: &TxnInfo, lsn: u64, transactional: bool, prefix: &str, content: &[u8]) -> Result<()>;
    fn stream_truncate(&mut self, c: &mut Ctx, t: &TxnInfo, rels: &[Arc<RelDesc>], cascade: bool, restart: bool) -> Result<()>;
}

/// LoadOutputPlugin.
pub fn load(name: &str) -> Result<Box<dyn Plugin>> {
    match name {
        "test_decoding" => Ok(Box::new(super::test_decoding::TestDecoding::default())),
        "pgoutput" => Ok(Box::new(super::pgoutput::PgOutput::default())),
        _ => Err(Error::new("58P01", format!("could not access file \"{name}\": No such file or directory"))),
    }
}

/// sizeof(ReorderBufferChange) and sizeof(HeapTupleData) on a 64-bit build.
const CHANGE_SIZE: usize = 80;
const TUPLE_SIZE: usize = 24;

/// ReorderBufferChangeSize: what a change counts against logical_decoding_work_mem. A row
/// heap_insert TOASTs counts as stored, plus a change per chunk of each value it moves out of
/// line (the chunks' inserts into the TOAST table come first in PostgreSQL's WAL).
fn change_size(
    rec: &Rec,
    rels: &HashMap<u32, Arc<RelDesc>>,
    fmt: &crate::types::text::FormatCtx,
    cat: &crate::catalog::Catalog,
) -> usize {
    use crate::exec::sizing;
    let tuple = |rel: u32, row: &[Value], unchanged: &[usize]| -> usize {
        let Some(d) = rels.get(&rel) else { return TUPLE_SIZE + 24 };
        let types: Vec<crate::types::DataType> = d.cols.iter().map(|c| c.ty.clone()).collect();
        let n = types.len().min(row.len());
        let text = |v: &Value, ty: &crate::types::DataType| crate::types::text::format_value(v, ty, fmt);
        let len = crate::catalog::relsize::heap_tuple_len(&types[..n], &row[..n], &text);
        if len <= 2032 {
            return TUPLE_SIZE + len;
        }
        let pinned = unchanged.iter().copied().collect();
        let Some((len, exts)) = cat.tables.get(&rel).and_then(|t| sizing::decoded_toast(t, &types[..n], &row[..n], &pinned, &text))
        else {
            return TUPLE_SIZE + len;
        };
        let chunks: usize = exts
            .iter()
            .map(|&b| {
                let (full, rest) = (b / sizing::TOAST_CHUNK_BYTES, b % sizing::TOAST_CHUNK_BYTES);
                let one = |c: usize| CHANGE_SIZE + TUPLE_SIZE + sizing::toast_chunk_tuple_len(c);
                full * one(sizing::TOAST_CHUNK_BYTES) + if rest > 0 { one(rest) } else { 0 }
            })
            .sum();
        TUPLE_SIZE + len + chunks
    };
    CHANGE_SIZE
        + match rec {
            Rec::Insert { rel, new } => tuple(*rel, new, &[]),
            Rec::Update { rel, old, new, unchanged_toast } => {
                tuple(*rel, new, unchanged_toast) + old.as_ref().map_or(0, |o| tuple(*rel, &o.0, &[]))
            }
            Rec::Delete { rel, old } => old.as_ref().map_or(0, |o| tuple(*rel, &o.0, &[])),
            Rec::Message { prefix, content, .. } => prefix.len() + 1 + content.len(),
            Rec::Truncate { rels, .. } => 4 * rels.len(),
            Rec::Rel(_) | Rec::Abort { .. } => 0,
        }
}

#[derive(Clone, Copy)]
enum Ending {
    Commit,
    Prepare,
}

/// A change queued in the reorder buffer (streaming), with what it counts in memory.
struct Queued<'c> {
    c: Cow<'c, Change>,
    sz: usize,
}

/// One transaction's replay: ReorderBuffer's state of it.
#[derive(Default)]
struct Replay<'c> {
    /// What it holds in memory (txn->total_size), and of that each aborted
    /// subtransaction's.
    mem: usize,
    sub_mem: HashMap<u64, usize>,
    /// Changes queued for the next block (streaming), in memory or spilled.
    queue: Vec<Queued<'c>>,
    /// Bytes of the changes replayed at the end (not streamed).
    total: usize,
    began: bool,
    spilled: bool,
    /// Streamed (RBTXN_IS_STREAMED), its streamed subtransactions, and those found aborted
    /// while streaming (their later changes aren't queued: RBTXN_IS_ABORTED).
    streamed: bool,
    streamed_subs: HashSet<u64>,
    dropped: HashSet<u64>,
    /// Counted among the slot's decoded transactions.
    counted: bool,
    /// Blocks output before (`Decoder::replay`'s `resume`).
    mute: bool,
}

/// What looking up `rec`'s relations leaves in a session's caches.
fn note_lookups(looked_up: &mut HashSet<u32>, rec: &Rec) {
    match rec {
        Rec::Insert { rel, .. } | Rec::Update { rel, .. } | Rec::Delete { rel, .. } => {
            looked_up.insert(*rel);
        }
        Rec::Truncate { rels, .. } => looked_up.extend(rels.iter().copied()),
        _ => {}
    }
}

/// The driver: a plugin, how it decodes, and what it did (the slot's statistics).
pub struct Decoder<'a> {
    pub ctx: Ctx<'a>,
    pub plugin: Box<dyn Plugin>,
    pub binary: bool,
    /// logical_decoding_work_mem, in bytes.
    pub work_mem: usize,
    /// Prepared transactions whose PREPARE ends at or before this were not decoded at
    /// PREPARE (two-phase decoding started later).
    pub two_phase_at: u64,
    pub stats: SlotStats,
    /// Rows output so far.
    pub rows: usize,
    /// Replay a transaction without its changes (`outgrows_work_mem` and `rolled_back`).
    skip_changes: bool,
    /// The slot's confirmed position when decoding started (start_decoding_at): nothing
    /// before it streams.
    pub start_at: u64,
    /// debug_logical_replication_streaming = immediate.
    pub immediate: bool,
    /// Relations looked up since the caches were reset (`needs_catalog`).
    looked_up: HashSet<u32>,
    /// Prepared transactions streamed in part at a PREPARE not decoded as one
    /// (`replay`'s `resume` at their COMMIT PREPARED).
    partly_streamed: HashSet<u64>,
    output_rewrites: bool,
    /// Counted among the database's decoders (`decode::Shared`).
    _reader: decode::Reader,
}

/// How a decoding context is set up.
pub struct Setup<'s> {
    pub plugin: &'s str,
    pub opts: &'s [(String, Option<String>)],
    pub is_init: bool,
    pub slot_two_phase: bool,
    pub two_phase_at: u64,
    pub fmt: crate::types::text::FormatCtx,
    pub work_mem: usize,
    pub start_at: u64,
    pub immediate: bool,
}

impl<'a> Decoder<'a> {
    /// CreateDecodingContext: the plugin started with its options.
    pub fn new(db: &'a Arc<Database>, slot: &str, s: Setup) -> Result<Decoder<'a>> {
        let mut plugin = load(s.plugin)?;
        let mut st = Startup::default();
        plugin.startup(s.opts, s.is_init, &mut st).map_err(|e| {
            e.context(format!("slot \"{slot}\", output plugin \"{}\", in the startup callback", s.plugin))
        })?;
        let ctx = Ctx {
            db,
            fmt: s.fmt,
            rels: HashMap::new(),
            write_lsn: 0,
            write_xid: 0,
            out: Vec::new(),
            streaming: st.streaming,
            twophase: s.slot_two_phase || st.twophase_opt,
            sub_xid: 0,
            filter: None,
        };
        Ok(Decoder {
            ctx,
            plugin,
            binary: st.binary,
            work_mem: s.work_mem,
            two_phase_at: s.two_phase_at,
            stats: SlotStats::default(),
            rows: 0,
            skip_changes: false,
            start_at: s.start_at,
            immediate: s.immediate,
            looked_up: HashSet::new(),
            partly_streamed: HashSet::new(),
            output_rewrites: st.output_rewrites,
            _reader: decode::Shared::reader(db),
        })
    }

    fn emit(&mut self, sink: &mut dyn FnMut(Output) -> Result<()>) -> Result<()> {
        for o in self.ctx.take() {
            self.rows += 1;
            sink(o)?;
        }
        Ok(())
    }

    /// Decode one commit record (and the transaction it ends) into `sink`.
    pub fn decode(&mut self, m: &Marker, sink: &mut dyn FnMut(Output) -> Result<()>) -> Result<()> {
        let t = TxnInfo {
            xid: m.xid,
            first_lsn: m.first_lsn,
            final_lsn: m.commit_lsn,
            end_lsn: m.end_lsn,
            time: m.time,
            origin: m.origin,
            origin_lsn: m.origin_lsn,
            gid: m.gid.clone(),
            prepare_time: m.prepare_time,
            prepare_end_lsn: m.prepare_end_lsn,
        };
        match m.kind {
            Kind::Message => {
                if self.plugin.filter_by_origin(m.origin) {
                    return Ok(());
                }
                let Some((prefix, content)) = decode::message_of(m) else { return Ok(()) };
                self.ctx.write_xid = 0;
                self.ctx.write_lsn = m.commit_lsn;
                self.plugin.message(&mut self.ctx, None, m.commit_lsn, false, &prefix, &content)?;
                self.emit(sink)
            }
            Kind::Commit => self.replay(m, &t, Some(Ending::Commit), false, sink),
            Kind::Prepare => {
                if !self.ctx.twophase || m.end_lsn <= self.two_phase_at || self.plugin.filter_prepare(m.xid, &m.gid) {
                    // (Streamed while it was read, as far as it outgrew the limit; its COMMIT
                    // PREPARED goes on from there.)
                    if self.ctx.streaming && m.first_lsn >= self.start_at {
                        let before = self.stats.stream_count;
                        self.replay(m, &t, None, false, sink)?;
                        if self.stats.stream_count > before {
                            self.partly_streamed.insert(m.xid);
                        }
                    }
                    return Ok(());
                }
                // ReorderBufferCheckAndTruncateAbortedTXN: a transaction that outgrows
                // logical_decoding_work_mem and has rolled back by then loses its changes
                // (only the PREPARE and, later, ROLLBACK PREPARED are decoded).
                self.skip_changes = !self.immediate && self.outgrows_work_mem(m)? && self.rolled_back(m)?;
                let r = self.replay(m, &t, Some(Ending::Prepare), false, sink);
                self.skip_changes = false;
                r
            }
            Kind::CommitPrepared => {
                if !self.ctx.twophase || self.plugin.filter_prepare(m.xid, &m.gid) {
                    let resume = self.partly_streamed.remove(&m.xid);
                    return self.replay(m, &t, Some(Ending::Commit), resume, sink);
                }
                if self.plugin.filter_by_origin(m.origin) {
                    return Ok(());
                }
                // ReorderBufferFinishPrepared: a PREPARE from before two-phase decoding
                // started is replayed now.
                if m.prepare_end_lsn <= self.two_phase_at {
                    let mut pt = t.clone();
                    pt.final_lsn = m.prepare_end_lsn.saturating_sub(super::log::COMMIT_SIZE);
                    pt.end_lsn = m.prepare_end_lsn;
                    pt.time = m.prepare_time;
                    self.replay(m, &pt, Some(Ending::Prepare), false, sink)?;
                }
                self.ctx.write_xid = m.xid;
                self.ctx.write_lsn = m.end_lsn;
                self.plugin.commit_prepared(&mut self.ctx, &t, m.commit_lsn)?;
                self.emit(sink)
            }
            Kind::RollbackPrepared => {
                if self.partly_streamed.remove(&m.xid) {
                    self.ctx.write_xid = m.xid;
                    self.ctx.write_lsn = m.end_lsn;
                    self.plugin.stream_abort(&mut self.ctx, &t, m.xid, m.commit_lsn, m.time)?;
                    return self.emit(sink);
                }
                if !self.ctx.twophase
                    || m.prepare_end_lsn <= self.two_phase_at
                    || self.plugin.filter_prepare(m.xid, &m.gid)
                    || self.plugin.filter_by_origin(m.origin)
                {
                    return Ok(());
                }
                self.ctx.write_xid = m.xid;
                self.ctx.write_lsn = m.end_lsn;
                self.plugin.rollback_prepared(&mut self.ctx, &t, m.prepare_end_lsn, m.prepare_time)?;
                self.emit(sink)
            }
        }
    }

    /// A transaction in progress (the decoding session's own: its records are its own
    /// writes, read at `lsn`), streamed as far as it outgrew logical_decoding_work_mem, as
    /// PostgreSQL streams one from the WAL it wrote so far.
    pub fn in_progress(&mut self, xid: u64, recs: &[bytes::Bytes], lsn: u64, sink: &mut dyn FnMut(Output) -> Result<()>) -> Result<()> {
        if !self.ctx.streaming || recs.is_empty() {
            return Ok(());
        }
        let t = TxnInfo { xid, first_lsn: lsn, final_lsn: lsn, end_lsn: lsn, ..TxnInfo::default() };
        let shared = &self.ctx.db.log.decoded;
        let mut at = lsn;
        let changes: Vec<Result<Change>> = recs
            .iter()
            .map(|v| {
                let c = shared.change(at, v);
                at += super::log::record_size(v.len());
                c
            })
            .collect();
        self.run(&t, None, false, changes.into_iter().map(|c| c.map(Cow::Owned)), sink)
    }

    /// Whether transaction `m`'s changes reach logical_decoding_work_mem.
    fn outgrows_work_mem(&mut self, m: &Marker) -> Result<bool> {
        let store = self.ctx.db.store.clone();
        let mut size = 0usize;
        let mut subs: HashMap<u64, usize> = HashMap::new();
        let cat = self.ctx.db.catalog();
        for c in decode::changes(&store, &self.ctx.db.log.decoded, m) {
            let c = c?;
            match &c.rec {
                Rec::Rel(d) => {
                    self.ctx.rels.insert(d.oid, d.clone());
                    continue;
                }
                Rec::Abort { xids, .. } => {
                    for x in xids {
                        size = size.saturating_sub(subs.remove(x).unwrap_or(0));
                    }
                    continue;
                }
                _ => {}
            }
            let sz = change_size(&c.rec, &self.ctx.rels, &self.ctx.fmt, &cat);
            size += sz;
            if c.sub != 0 {
                *subs.entry(c.sub).or_default() += sz;
            }
            if size >= self.work_mem {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether prepared transaction `m` was rolled back (TransactionIdDidAbort, as of now).
    fn rolled_back(&self, m: &Marker) -> Result<bool> {
        for r in decode::markers(&self.ctx.db.store, m.end_lsn, u64::MAX) {
            let r = r?;
            if r.kind == Kind::RollbackPrepared && r.xid == m.xid {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// ReorderBufferReplay / ReorderBufferStreamTXN: the transaction's changes, streamed in
    /// blocks if it outgrows logical_decoding_work_mem while streaming is on.
    ///
    /// The memory accounting is ReorderBuffer's as it reads the WAL: an aborted
    /// subtransaction's changes count until its abort record (and are streamed when the
    /// transaction outgrows the limit before it, then aborted with "stream abort"); a change
    /// of one that needs the catalog while streaming (a relation not looked up yet) finds it
    /// aborted (HandleConcurrentAbort), which ends the block, drops what was queued and the
    /// subtransaction's later changes. Non-streamed replay leaves them out.
    ///
    /// `end` None: a transaction not finished at this point (a PREPARE not decoded as one):
    /// only the blocks outgrowing the limit streamed; `resume`: those were output already
    /// (at its PREPARE), so only what follows them is.
    fn replay(
        &mut self,
        m: &Marker,
        t: &TxnInfo,
        end: Option<Ending>,
        resume: bool,
        sink: &mut dyn FnMut(Output) -> Result<()>,
    ) -> Result<()> {
        if self.plugin.filter_by_origin(m.origin) {
            return Ok(());
        }
        let store = self.ctx.db.store.clone();
        let keep = if self.skip_changes { 0 } else { usize::MAX };
        let db = self.ctx.db;
        // (Decoded once for the database's decoders when it can be kept: `decode::Shared`.)
        let shared = if keep == 0 { None } else { db.log.decoded.txn(&store, m)? };
        let changes: Box<dyn Iterator<Item = Result<Cow<Change>>>> = match &shared {
            Some(cs) => Box::new(cs.iter().map(|c| Ok(Cow::Borrowed(c)))),
            None => Box::new(decode::changes(&store, &db.log.decoded, m).take(keep).map(|c| c.map(Cow::Owned))),
        };
        self.run(t, end, resume, changes, sink)
    }

    fn run<'c>(
        &mut self,
        t: &TxnInfo,
        end: Option<Ending>,
        resume: bool,
        changes: impl Iterator<Item = Result<Cow<'c, Change>>>,
        sink: &mut dyn FnMut(Output) -> Result<()>,
    ) -> Result<()> {
        // (Nothing before the slot's confirmed position streams: SnapBuildXactNeedsSkip.)
        let stream = self.ctx.streaming
            && t.first_lsn >= self.start_at
            // COMMIT PREPARED reuses the changes from PREPARE. If a previous SQL
            // decoding call consumed that PREPARE, those changes no longer stream.
            && (t.prepare_end_lsn == 0 || t.prepare_end_lsn > self.start_at);
        if end.is_none() && !stream {
            return Ok(());
        }
        let mut st = Replay { mute: resume, ..Replay::default() };
        let cat = self.ctx.db.catalog();
        for c in changes {
            let c = c?;
            match &c.rec {
                Rec::Rel(d) => {
                    // (A changed definition is looked up again: its invalidation.)
                    if self.ctx.rels.get(&d.oid).is_some_and(|o| !Arc::ptr_eq(o, d) && **o != **d) {
                        self.looked_up.remove(&d.oid);
                    }
                    self.ctx.rels.insert(d.oid, d.clone());
                    continue;
                }
                Rec::Abort { xids, time } => {
                    for x in xids {
                        // ReorderBufferAbort: a streamed subtransaction is aborted downstream;
                        // its changes are freed either way.
                        if st.streamed_subs.remove(x) && !st.mute {
                            self.ctx.write_xid = *x;
                            self.ctx.write_lsn = c.lsn;
                            self.plugin.stream_abort(&mut self.ctx, t, *x, c.lsn, *time)?;
                            self.emit(sink)?;
                        }
                        st.mem = st.mem.saturating_sub(st.sub_mem.remove(x).unwrap_or(0));
                        st.queue.retain(|q| q.c.sub != *x);
                    }
                    continue;
                }
                _ => {}
            }
            if c.sub != 0 && st.dropped.contains(&c.sub) {
                continue;
            }
            let sz = change_size(&c.rec, &self.ctx.rels, &self.ctx.fmt, &cat);
            st.mem += sz;
            if c.sub != 0 {
                *st.sub_mem.entry(c.sub).or_default() += sz;
            }
            if !stream {
                // Spilled to disk past the limit; replayed in order at commit either way.
                if self.over(st.mem) {
                    self.spill(&mut st);
                }
                if c.sub != 0 {
                    continue;
                }
                st.total += sz;
                // (ReorderBufferProcessTXN: a rewrite's rows, queued like any, are output
                // only to plugins asking for them.)
                if c.rewrite && !self.output_rewrites {
                    continue;
                }
                if !st.began {
                    self.begin(t, end.unwrap_or(Ending::Commit))?;
                    st.began = true;
                }
                self.change(t, &c, false)?;
                self.emit(sink)?;
                continue;
            }
            st.queue.push(Queued { c, sz });
            if self.over(st.mem) {
                self.stream_block(t, &mut st, sink)?;
            }
        }
        self.ctx.write_xid = t.xid;
        let Some(end) = end else {
            // (Not finished here: only what outgrowing the limit streamed.)
            return Ok(());
        };
        st.mute = false;
        if st.streamed {
            if !st.queue.is_empty() {
                self.stream_block(t, &mut st, sink)?;
            }
            self.ctx.write_xid = t.xid;
            self.ctx.write_lsn = t.end_lsn;
            match end {
                Ending::Commit => self.plugin.stream_commit(&mut self.ctx, t, t.final_lsn)?,
                Ending::Prepare => self.plugin.stream_prepare(&mut self.ctx, t, t.final_lsn)?,
            }
        } else {
            if !st.began {
                self.begin(t, end)?;
            }
            for q in std::mem::take(&mut st.queue) {
                if q.c.sub == 0 {
                    st.total += q.sz;
                    if !q.c.rewrite || self.output_rewrites {
                        self.change(t, &q.c, false)?;
                    }
                }
            }
            self.ctx.write_xid = t.xid;
            self.ctx.write_lsn = t.end_lsn;
            match end {
                Ending::Commit => self.plugin.commit(&mut self.ctx, t, t.final_lsn)?,
                Ending::Prepare => self.plugin.prepare(&mut self.ctx, t, t.final_lsn)?,
            }
            self.stats.total_txns += 1;
        }
        self.stats.total_bytes += st.total as i64;
        self.emit(sink)
    }

    /// The memory limit is reached (debug_logical_replication_streaming = immediate: any
    /// change reaches it).
    fn over(&self, mem: usize) -> bool {
        mem >= self.work_mem || (self.immediate && mem > 0)
    }

    /// ReorderBufferSerializeTXN: what is in memory goes to disk (statistics only here).
    fn spill(&mut self, st: &mut Replay) {
        if !st.spilled {
            self.stats.spill_txns += 1;
            st.spilled = true;
        }
        self.stats.spill_count += 1;
        self.stats.spill_bytes += st.mem as i64;
        st.mem = 0;
        st.sub_mem.clear();
    }

    fn begin(&mut self, t: &TxnInfo, end: Ending) -> Result<()> {
        self.ctx.write_xid = t.xid;
        self.ctx.write_lsn = t.first_lsn;
        match end {
            Ending::Commit => self.plugin.begin(&mut self.ctx, t),
            Ending::Prepare => self.plugin.begin_prepare(&mut self.ctx, t),
        }
    }

    /// ReorderBufferStreamTXN: a block of what is queued (`Replay::mute`: only its effects
    /// on what follows).
    fn stream_block(&mut self, t: &TxnInfo, st: &mut Replay, sink: &mut dyn FnMut(Output) -> Result<()>) -> Result<()> {
        if st.mute {
            let found = st.queue.iter().position(|q| q.c.sub != 0 && self.needs_catalog(&q.c.rec));
            for (i, q) in st.queue.drain(..).enumerate() {
                if q.c.sub != 0 {
                    st.streamed_subs.insert(q.c.sub);
                }
                if found.is_none_or(|f| i < f) {
                    note_lookups(&mut self.looked_up, &q.c.rec);
                } else if Some(i) == found {
                    st.dropped.insert(q.c.sub);
                }
            }
            st.streamed = true;
            st.mem = 0;
            st.sub_mem.clear();
            return Ok(());
        }
        let bytes: usize = st.queue.iter().map(|q| q.sz).sum();
        if !st.streamed {
            self.stats.stream_txns += 1;
        }
        self.stats.stream_count += 1;
        self.stats.stream_bytes += st.mem as i64;
        self.ctx.write_xid = t.xid;
        self.ctx.write_lsn = st.queue.first().map_or(t.first_lsn, |q| q.c.lsn);
        self.plugin.stream_start(&mut self.ctx, t)?;
        let mut last = self.ctx.write_lsn;
        let mut aborted = None;
        for q in &st.queue {
            last = q.c.lsn;
            if q.c.sub != 0 && self.needs_catalog(&q.c.rec) {
                aborted = Some(q.c.sub);
                break;
            }
            if q.c.rewrite && !self.output_rewrites {
                note_lookups(&mut self.looked_up, &q.c.rec);
                continue;
            }
            self.change(t, &q.c, true)?;
        }
        // ReorderBufferTruncateTXN: subtransactions with changes in it count as streamed.
        for q in st.queue.drain(..) {
            if q.c.sub != 0 {
                st.streamed_subs.insert(q.c.sub);
            }
        }
        match aborted {
            Some(x) => {
                st.dropped.insert(x);
            }
            None => {
                self.stats.total_bytes += bytes as i64;
                if !st.counted {
                    self.stats.total_txns += 1;
                    st.counted = true;
                }
            }
        }
        st.streamed = true;
        st.mem = 0;
        st.sub_mem.clear();
        self.ctx.write_xid = t.xid;
        self.ctx.write_lsn = last;
        self.plugin.stream_stop(&mut self.ctx, t)?;
        self.emit(sink)
    }

    /// Whether replaying `rec` reads the catalog (RelidByRelfilenumber, RelationIdGetRelation
    /// of a relation this decoding session hasn't looked up since its caches were reset).
    fn needs_catalog(&self, rec: &Rec) -> bool {
        match rec {
            Rec::Insert { rel, .. } | Rec::Update { rel, .. } | Rec::Delete { rel, .. } => !self.looked_up.contains(rel),
            Rec::Truncate { rels, .. } => rels.iter().any(|r| !self.looked_up.contains(r)),
            _ => false,
        }
    }

    fn change(&mut self, t: &TxnInfo, c: &Change, stream: bool) -> Result<()> {
        self.ctx.write_xid = t.xid;
        self.ctx.write_lsn = c.lsn;
        self.ctx.sub_xid = c.sub;
        let r = self.output_change(t, c, stream);
        self.ctx.sub_xid = 0;
        r
    }

    fn output_change(&mut self, t: &TxnInfo, c: &Change, stream: bool) -> Result<()> {
        let rel = |ctx: &Ctx, oid: u32| -> Result<Arc<RelDesc>> {
            ctx.rels.get(&oid).cloned().ok_or_else(|| {
                Error::new(code::INTERNAL_ERROR, format!("could not map filenumber of relation {oid}"))
            })
        };
        let ch = match &c.rec {
            Rec::Insert { rel: r, new } => Some((*r, RowChange::Insert { new })),
            Rec::Update { rel: r, old, new, unchanged_toast } => Some((
                *r,
                RowChange::Update { old: old.as_ref().map(|(v, k)| (v.as_slice(), *k)), new, unchanged_toast },
            )),
            Rec::Delete { rel: r, old } => {
                Some((*r, RowChange::Delete { old: old.as_ref().map(|(v, k)| (v.as_slice(), *k)) }))
            }
            _ => None,
        };
        note_lookups(&mut self.looked_up, &c.rec);
        if let Some((r, ch)) = ch {
            let d = rel(&self.ctx, r)?;
            return if stream {
                self.plugin.stream_change(&mut self.ctx, t, &d, &ch)
            } else {
                self.plugin.change(&mut self.ctx, t, &d, &ch)
            };
        }
        match &c.rec {
            Rec::Truncate { cascade, restart_seqs, rels } => {
                let ds: Vec<Arc<RelDesc>> = rels.iter().map(|r| rel(&self.ctx, *r)).collect::<Result<_>>()?;
                if stream {
                    self.plugin.stream_truncate(&mut self.ctx, t, &ds, *cascade, *restart_seqs)
                } else {
                    self.plugin.truncate(&mut self.ctx, t, &ds, *cascade, *restart_seqs)
                }
            }
            Rec::Message { prefix, content, .. } => {
                if stream {
                    self.plugin.stream_message(&mut self.ctx, t, c.lsn, true, prefix, content)
                } else {
                    self.plugin.message(&mut self.ctx, Some(t), c.lsn, true, prefix, content)
                }
            }
            _ => Ok(()),
        }
    }
}
