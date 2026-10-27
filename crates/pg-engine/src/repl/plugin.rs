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
use std::collections::HashMap;
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
    fn stream_abort(&mut self, c: &mut Ctx, t: &TxnInfo, sub_xid: u64, abort_lsn: u64) -> Result<()>;
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

/// ReorderBufferChangeSize: what a change counts against logical_decoding_work_mem.
fn change_size(rec: &Rec, rels: &HashMap<u32, Arc<RelDesc>>, fmt: &crate::types::text::FormatCtx) -> usize {
    let tuple = |rel: u32, row: &[Value]| -> usize {
        let Some(d) = rels.get(&rel) else { return TUPLE_SIZE + 24 };
        let types: Vec<crate::types::DataType> = d.cols.iter().map(|c| c.ty.clone()).collect();
        let n = types.len().min(row.len());
        let text = |v: &Value, ty: &crate::types::DataType| crate::types::text::format_value(v, ty, fmt);
        TUPLE_SIZE + crate::catalog::relsize::heap_tuple_len(&types[..n], &row[..n], &text)
    };
    CHANGE_SIZE
        + match rec {
            Rec::Insert { rel, new } => tuple(*rel, new),
            Rec::Update { rel, old, new, .. } => tuple(*rel, new) + old.as_ref().map_or(0, |o| tuple(*rel, &o.0)),
            Rec::Delete { rel, old } => old.as_ref().map_or(0, |o| tuple(*rel, &o.0)),
            Rec::Message { prefix, content, .. } => prefix.len() + 1 + content.len(),
            Rec::Truncate { rels, .. } => 4 * rels.len(),
            Rec::Rel(_) => 0,
        }
}

#[derive(Clone, Copy)]
enum Ending {
    Commit,
    Prepare,
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
            Kind::Commit => self.replay(m, &t, Ending::Commit, sink),
            Kind::Prepare => {
                if !self.ctx.twophase || m.end_lsn <= self.two_phase_at || self.plugin.filter_prepare(m.xid, &m.gid) {
                    return Ok(());
                }
                self.replay(m, &t, Ending::Prepare, sink)
            }
            Kind::CommitPrepared => {
                if !self.ctx.twophase || self.plugin.filter_prepare(m.xid, &m.gid) {
                    return self.replay(m, &t, Ending::Commit, sink);
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
                    self.replay(m, &pt, Ending::Prepare, sink)?;
                }
                self.ctx.write_xid = m.xid;
                self.ctx.write_lsn = m.end_lsn;
                self.plugin.commit_prepared(&mut self.ctx, &t, m.commit_lsn)?;
                self.emit(sink)
            }
            Kind::RollbackPrepared => {
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

    /// ReorderBufferReplay / ReorderBufferStreamTXN: the transaction's changes, streamed in
    /// blocks if it outgrows logical_decoding_work_mem while streaming is on.
    fn replay(&mut self, m: &Marker, t: &TxnInfo, end: Ending, sink: &mut dyn FnMut(Output) -> Result<()>) -> Result<()> {
        if self.plugin.filter_by_origin(m.origin) {
            return Ok(());
        }
        let store = self.ctx.db.store.clone();
        let mut size = 0usize;
        let mut total = 0usize;
        let mut pending: Vec<Change> = Vec::new();
        let mut streamed = false;
        let mut began = false;
        let mut spilled = false;
        for c in decode::changes(&store, m) {
            let c = c?;
            if let Rec::Rel(d) = &c.rec {
                self.ctx.rels.insert(d.oid, d.clone());
                continue;
            }
            let sz = change_size(&c.rec, &self.ctx.rels, &self.ctx.fmt);
            total += sz;
            size += sz;
            if !self.ctx.streaming {
                // Spilled to disk past the limit; replayed in order at commit either way.
                if size >= self.work_mem {
                    if !spilled {
                        self.stats.spill_txns += 1;
                        spilled = true;
                    }
                    self.stats.spill_count += 1;
                    self.stats.spill_bytes += size as i64;
                    size = 0;
                }
                if !began {
                    self.begin(t, end)?;
                    began = true;
                }
                self.change(t, &c, false)?;
                self.emit(sink)?;
                continue;
            }
            pending.push(c);
            if size >= self.work_mem {
                self.stream_block(t, &mut pending, &mut streamed, size, sink)?;
                size = 0;
            }
        }
        self.ctx.write_xid = t.xid;
        if streamed {
            if !pending.is_empty() {
                self.stream_block(t, &mut pending, &mut streamed, size, sink)?;
            }
            self.ctx.write_lsn = t.end_lsn;
            match end {
                Ending::Commit => self.plugin.stream_commit(&mut self.ctx, t, t.final_lsn)?,
                Ending::Prepare => self.plugin.stream_prepare(&mut self.ctx, t, t.final_lsn)?,
            }
        } else {
            if !began {
                self.begin(t, end)?;
            }
            for c in std::mem::take(&mut pending) {
                self.change(t, &c, false)?;
            }
            self.ctx.write_xid = t.xid;
            self.ctx.write_lsn = t.end_lsn;
            match end {
                Ending::Commit => self.plugin.commit(&mut self.ctx, t, t.final_lsn)?,
                Ending::Prepare => self.plugin.prepare(&mut self.ctx, t, t.final_lsn)?,
            }
        }
        self.stats.total_txns += 1;
        self.stats.total_bytes += total as i64;
        self.emit(sink)
    }

    fn begin(&mut self, t: &TxnInfo, end: Ending) -> Result<()> {
        self.ctx.write_xid = t.xid;
        self.ctx.write_lsn = t.first_lsn;
        match end {
            Ending::Commit => self.plugin.begin(&mut self.ctx, t),
            Ending::Prepare => self.plugin.begin_prepare(&mut self.ctx, t),
        }
    }

    fn stream_block(
        &mut self,
        t: &TxnInfo,
        pending: &mut Vec<Change>,
        streamed: &mut bool,
        size: usize,
        sink: &mut dyn FnMut(Output) -> Result<()>,
    ) -> Result<()> {
        if !*streamed {
            self.stats.stream_txns += 1;
        }
        *streamed = true;
        self.stats.stream_count += 1;
        self.stats.stream_bytes += size as i64;
        self.ctx.write_xid = t.xid;
        self.ctx.write_lsn = pending.first().map_or(t.first_lsn, |c| c.lsn);
        self.plugin.stream_start(&mut self.ctx, t)?;
        let last = pending.last().map_or(t.first_lsn, |c| c.lsn);
        for c in std::mem::take(pending) {
            self.change(t, &c, true)?;
        }
        self.ctx.write_xid = t.xid;
        self.ctx.write_lsn = last;
        self.plugin.stream_stop(&mut self.ctx, t)?;
        self.emit(sink)
    }

    fn change(&mut self, t: &TxnInfo, c: &Change, stream: bool) -> Result<()> {
        self.ctx.write_xid = t.xid;
        self.ctx.write_lsn = c.lsn;
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
