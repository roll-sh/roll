//! A nested loop's inner index scans run in a parallel scan's workers (PostgreSQL's
//! parallel nested loop: the outer side is a partial path, each worker scans the inner
//! index for its own outer rows): a pipeline stage (`pipe::Stage::Lookup`) that, for each
//! outer row, reads the index entries equal to the row's parameters and fetches their rows
//! at the statement's snapshot, from the store directly (the transaction has written
//! nothing: `Txn::store_reader`). The rows, their order and the join's output are the
//! serial nested loop's; a row whose parameters, filter or join condition a kernel leaves
//! to `eval` goes to the executor's thread, where the nested loop runs it itself.
//!
//! Only for scans with nothing done on the executor's thread per row: plain btree index
//! scans by equality, no locks, samples or system columns, not under EXPLAIN ANALYZE or
//! predicate locking, of a table whose heap and index models (`sizing`) a fetch can't
//! change (not modelled, or every page all-visible); the scans' statistics are counted on
//! the executor's thread as the stage ends (`flush`).

use super::Row;
use super::kernel::{K, StoredCmps, compile};
use super::parscan::Decode;
use super::pipe::{Joined, truncate_row};
use crate::sql::expr::Expr;
use crate::sql::plan::{Access, JoinKind, Plan, ScanPlan};
use crate::storage::Store;
use crate::storage::keys::{KeyColl, encode_index_value, index_prefix, prefix_end};
use crate::types::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Nested loops whose inner scans ran in a parallel scan's workers (tests check they did).
static STARTED: AtomicU64 = AtomicU64::new(0);

/// How many nested loops have run their inner scans in the workers.
pub fn started() -> u64 {
    STARTED.load(Ordering::Relaxed)
}

/// A nested loop's inner scans went into a pipeline.
pub fn note_started() {
    STARTED.fetch_add(1, Ordering::Relaxed);
}

/// Keys a worker keeps the rows of (a small table's: `Lookup::cache`).
const CACHE_KEYS: usize = 1 << 16;

/// A worker's scans of one stage: the scans made and rows fetched since its counts were
/// last taken, and the rows of the keys it looked up (a small table's), read at the same
/// snapshot (a cache only saves reading them again: the counts are the scans').
#[derive(Default)]
pub struct LookupScratch {
    cache: std::collections::HashMap<Vec<u8>, std::rc::Rc<Vec<Value>>>,
    loops: u64,
    fetched: u64,
    /// The worker's reader of the rows' keys, kept from lookup to lookup: the blocks the
    /// last one read stay at hand for the next (an outer side in key order reads its inner
    /// rows' blocks once).
    reader: Option<crate::storage::point::PointReader>,
    /// The index entries' row keys of a lookup, reused.
    dkeys: Vec<Vec<u8>>,
    /// The scan of the last lookup's entries (a key not unique), sought to the next one's
    /// (`Scan::reseek`): its sources aren't made again, and a lookup landing in the block
    /// the last one read reads on in it.
    entries: Option<crate::storage::Scan>,
    /// `Lookup::batch`'s buffers.
    batch: BatchBufs,
    /// A lookup's index key and the end of its range (`rows`), reused.
    start: Vec<u8>,
    end: Vec<u8>,
}

/// A batch's outer row, as `Lookup::batch` found it: left to the executor's thread, or its
/// inner rows (a range of the batch's values: `Lookup::width` a row).
#[derive(Clone, Copy)]
pub enum Got {
    Left,
    Rows(u32, u32),
}

/// `Lookup::batch`'s buffers, kept from batch to batch.
#[derive(Default)]
struct BatchBufs {
    /// The outer rows' index keys, end to end (`Want::key`: spans).
    starts: Vec<u8>,
    wants: Vec<Want>,
    /// The outer rows to read, by key.
    order: Vec<u32>,
    /// The entries' row keys, end to end (`espans`: spans; `Want::ents`: a range of them).
    ekeys: Vec<u8>,
    espans: Vec<(u32, u32)>,
    /// The batch's distinct row keys (spans of `ekeys`), ascending: the multi-get's.
    sorted: Vec<(u32, u32)>,
    /// Per entry (of `espans`), its row key's place in `sorted`.
    slot: Vec<u32>,
}

/// One outer row of a batch: its index key, how it is read, and its entries.
#[derive(Clone, Copy)]
struct Want {
    key: (u32, u32),
    how: How,
    ents: (u32, u32),
    /// A unique index's probed entry: its row's visibility not checked yet (the multi-get
    /// checks it).
    probed: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum How {
    /// A kernel left a key to `eval`.
    Left,
    /// A NULL key: nothing found.
    Null,
    /// Its entries, then their rows with the batch's.
    Batch,
    /// On its own (`rows`): one the batch couldn't read or decide.
    Alone,
}

/// What `Lookup::add` did with a stored row.
enum Add {
    /// Appended: it passed.
    Kept,
    /// Turned away by the filter or the join condition.
    Out,
    /// A kernel left something to `eval`.
    Left,
    /// It didn't decode.
    Bad,
}

/// One nested loop's inner index scans.
pub struct Lookup {
    store: Arc<Store>,
    ts: u64,
    /// The index's key prefix.
    prefix: Vec<u8>,
    /// Per equality column: its value over the outer row, how it is encoded (DESC, NULLS
    /// FIRST, its key collation), and the char(n) width a bpchar value is padded to.
    keys: Vec<(K, bool, bool, Option<KeyColl>, Option<usize>)>,
    decode: Decode,
    /// The inner rows' width (the table's columns), and the columns decoded: up to `last`,
    /// those `need` marks (`decode`'s).
    width: usize,
    last: usize,
    /// The inner scan's filter's comparisons with constants over the stored values
    /// (`StoredCmps`): a row they turn away isn't decoded; one they pass skips `filter` when
    /// they are the whole of it (true).
    stored: Option<(StoredCmps, bool)>,
    /// The inner scan's filter and the join condition, over the joined row's two sides.
    filter: Option<K>,
    cond: Option<K>,
    pub kind: JoinKind,
    pub nright: usize,
    /// The joined rows' columns anything reads (None: all).
    pub keep: Option<Box<[bool]>>,
    /// Scans made and rows fetched, for the statistics (`flush`).
    loops: AtomicU64,
    fetched: AtomicU64,
    index: u32,
    table: u32,
    /// Keep looked-up keys' rows (the table is small: modelled).
    cache: bool,
    /// A unique index's every key column equated: its one visible entry read without a
    /// merging scan (`Store::first_with`), as the serial point path reads it.
    unique: bool,
}

/// `e` with the nested loop's parameters (`Outer(1, k)`: `params[k]`, over the outer
/// row) in place and its own columns after the outer row's `nleft` (None: it reads
/// something else, or columns when `cols` is false).
fn bind(e: &Expr, params: &[Expr], nleft: usize, cols: bool) -> Option<Expr> {
    let mut ok = true;
    let out = e
        .clone()
        .transform(&mut |x| {
            Ok(match x {
                Expr::Col(i, t) if cols => Expr::Col(i + nleft, t),
                Expr::Col(i, t) => {
                    ok = false;
                    Expr::Col(i, t)
                }
                Expr::Outer(1, k, t) => match params.get(k) {
                    Some(p) => p.clone(),
                    None => {
                        ok = false;
                        Expr::Outer(1, k, t)
                    }
                },
                Expr::Outer(l, k, t) => {
                    ok = false;
                    Expr::Outer(l, k, t)
                }
                x => x,
            })
        })
        .ok()?;
    ok.then_some(out)
}

impl Lookup {
    /// The stage for a nested loop of `kind` over `right` with parameters `params`, its
    /// condition `cond` (over the joined row), when its inner side can run in the workers.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: JoinKind,
        right: &Plan,
        params: &[Expr],
        cond: Option<&Expr>,
        nleft: usize,
        nright: usize,
        keep: Option<Box<[bool]>>,
        ctx: &super::ExecCtx,
    ) -> super::Result<Option<Lookup>> {
        let Plan::Scan(s) = right else { return Ok(None) };
        let Some(snap) = ctx.snapshot() else { return Ok(None) };
        if !matches!(kind, JoinKind::Inner | JoinKind::Left | JoinKind::Semi | JoinKind::Anti)
            || params.is_empty()
            || !plain_index_scan(s, ctx)
        {
            return Ok(None);
        }
        let Some((ix, b)) = &s.index else { return Ok(None) };
        let mut keys = Vec::with_capacity(b.eq.len());
        for (i, e) in b.eq.iter().enumerate() {
            let Some(k) = bind(e, params, nleft, false).as_ref().and_then(compile) else {
                return Ok(None);
            };
            let c = &ix.columns[i];
            keys.push((k, c.desc, c.nulls_first, b.coll(i).cloned(), b.pad(i)));
        }
        let joined = |e: Option<&Expr>, shift: bool| -> Option<Option<K>> {
            match e {
                None => Some(None),
                Some(e) => {
                    let e = if shift { bind(e, params, nleft, true)? } else { e.clone() };
                    compile(&e).map(Some)
                }
            }
        };
        let (Some(filter), Some(cond_k)) = (joined(s.filter.as_ref(), true), joined(cond, false)) else {
            return Ok(None);
        };
        let Some((store, ts)) = ctx.txn.borrow().store_reader(&snap) else {
            return Ok(None);
        };
        let width = s.table.columns.len();
        if width == 0 {
            return Ok(None);
        }
        // (The filter over the inner row alone: constants and its own columns.)
        // (The filter's first conjuncts over the inner row alone, its columns and constants:
        // an inner scan's filter holds the join's equalities over the outer row too, `^1#k`.
        // Only the first ones: a row they turn away is one whose filter stops there, as
        // ExecQual stops at its first false qual, before a later one that may raise an error.)
        let stored = s.filter.as_ref().and_then(|f| {
            let parts: Vec<&Expr> = match f {
                Expr::And(a) => a.iter().collect(),
                e => vec![e],
            };
            let mut ks: Vec<K> = parts.iter().map_while(|e| compile(e)).collect();
            let whole = ks.len() == parts.len();
            let k = match ks.len() {
                0 => return None,
                1 => ks.pop()?,
                _ => K::And(ks),
            };
            StoredCmps::of(&k).map(|c| (c, whole))
        });
        // (Equal index keys' rows in the model's heap order: only a unique index's single
        // row, or a table not modelled, whose entries are in storage order.)
        let modelled = super::sizing::heap_modelled_or_kept(&s.table, ctx);
        if modelled && !(ix.unique && b.eq.len() == ix.columns.len()) {
            return Ok(None);
        }
        if !super::sizing::fetches_change_nothing(s.table.oid, &ctx.sess.db) {
            return Ok(None);
        }
        // The inner columns read: the joined row's, the filter's and the condition's.
        let need = (|| {
            let mut n = super::need::slice(keep.as_deref(), nleft, nright)?;
            if let Some(f) = &s.filter
                && !super::need::mark(f, &mut n)
            {
                return None;
            }
            if let Some(c) = cond {
                let mut all = vec![false; nleft];
                if !super::need::mark(c, &mut all) {
                    return None;
                }
                for (i, b) in all.into_iter().enumerate().skip(nleft) {
                    if b && i - nleft < n.len() {
                        n[i - nleft] = true;
                    }
                }
            }
            n.resize(s.table.columns.len(), false);
            Some(Arc::from(n))
        })();
        let last = need.as_deref().map_or(width, |n: &[bool]| n.iter().rposition(|&x| x).map_or(0, |i| i + 1));
        Ok(Some(Lookup {
            store,
            ts,
            prefix: index_prefix(s.table.oid, ix.id),
            keys,
            width,
            last,
            stored,
            decode: Decode {
                need,
                late: None,
                missing: s.table.columns.iter().map(|c| c.missing.clone()).collect(),
                types: s.table.columns.iter().map(|c| c.ty.clone()).collect(),
                with_key: false,
                filter: None,
                filtered: false,
                spec: None,
                stages: Vec::new(),
                ios: None,
                extra: Vec::new(),
                drop: Vec::new(),
                raw: Vec::new(),
                need_raw: None,
                topk: None,
                toast: None,
                vplan: Default::default(),
            },
            filter,
            cond: cond_k,
            kind,
            nright,
            keep,
            loops: AtomicU64::new(0),
            fetched: AtomicU64::new(0),
            index: ix.oid,
            table: s.table.oid,
            cache: modelled,
            unique: (ix.unique || ix.primary) && b.eq.len() == ix.columns.len(),
        }))
    }

    /// The inner rows outer row `l` joins (its index entries' rows passing the scan's
    /// filter and the join condition), in the serial join's order, into `out` (`width`
    /// values a row). False: something is left to `eval` or failed (the row goes to the
    /// executor's thread, which runs it as the serial join does). `sc`: the worker's counts
    /// and cache.
    pub fn rows(&self, l: &[Value], out: &mut Vec<Value>, sc: &mut LookupScratch) -> bool {
        let mut start = std::mem::take(&mut sc.start);
        start.clear();
        start.extend_from_slice(&self.prefix);
        let r = self.rows_under(l, &mut start, out, sc);
        sc.start = start;
        r
    }

    /// `rows` with the index key `start` (the prefix) made in place.
    fn rows_under(&self, l: &[Value], start: &mut Vec<u8>, out: &mut Vec<Value>, sc: &mut LookupScratch) -> bool {
        truncate_row(out, 0);
        for (k, desc, nulls_first, coll, pad) in &self.keys {
            let Some(v) = k.run(l) else { return false };
            if v.is_null() {
                // (An equality with NULL finds nothing: the scan still counts.)
                sc.loops += 1;
                return true;
            }
            // (A bpchar probe of a char(n) key, padded as the key is stored.)
            let v = super::scan::padded(v, *pad);
            encode_index_value(start, &v, *desc, *nulls_first, coll.as_ref());
        }
        // (A semi or anti join's inner scan stops at its first match, as ExecNestLoop's
        // does: the rows after it aren't fetched.)
        let first = matches!(self.kind, JoinKind::Semi | JoinKind::Anti);
        if first && !self.cache {
            return self.first_row(l, start, out, sc);
        }
        let found = match sc.cache.get(start.as_slice()) {
            Some(rows) => rows.clone(),
            None => {
                let Some(rows) = self.fetch(start, sc) else { return false };
                let rows = std::rc::Rc::new(rows);
                if self.cache {
                    if sc.cache.len() >= CACHE_KEYS {
                        sc.cache.clear();
                    }
                    sc.cache.insert(start.clone(), rows.clone());
                }
                rows
            }
        };
        sc.loops += 1;
        let mut read = 0u64;
        for r in found.chunks_exact(self.width) {
            read += 1;
            let view = Joined::Pair { l, r: super::pipe::Inner::plain(r), keep: None };
            let mut pass = true;
            for k in [&self.filter, &self.cond].into_iter().flatten() {
                match k.passes(&view) {
                    Some(true) => {}
                    Some(false) => {
                        pass = false;
                        break;
                    }
                    None => {
                        sc.fetched += (found.len() / self.width) as u64;
                        return false;
                    }
                }
            }
            if pass {
                out.extend_from_slice(r);
                if first {
                    break;
                }
            }
        }
        sc.fetched += read;
        true
    }

    /// The row keys of the index entries under key `start`, in index order, into
    /// `sc.dkeys` (None: a read failed).
    fn entries(&self, start: &[u8], sc: &mut LookupScratch) -> Option<()> {
        entry_keys(&self.store, self.ts, self.unique, start, sc)
    }

    /// Row `d` at the snapshot, through the worker's reader (None inside: not visible).
    fn get(&self, d: &[u8], reader: &mut Option<crate::storage::point::PointReader>) -> Option<Option<bytes::Bytes>> {
        row_at(&self.store, self.ts, d, reader)
    }

    /// Stored row `bytes` appended to `out` as `Decode::row` makes it: the table's columns,
    /// those not needed NULL (Err: it didn't decode; nothing appended).
    fn decode_into(&self, bytes: &[u8], out: &mut Vec<Value>) -> crate::error::Result<()> {
        use crate::storage::row::{decode_value, skip_value_fast};
        let data = super::scan::row_data(bytes)?;
        let base = out.len();
        out.reserve(self.width);
        let r = (|| {
            let mut pos = 0;
            let n = crate::storage::sst::get_varint(data, &mut pos)? as usize;
            let walk = self.last.min(n);
            match self.decode.need.as_deref() {
                Some(need) => {
                    for &want in &need[..walk] {
                        if want {
                            out.push(decode_value(data, &mut pos)?);
                        } else {
                            skip_value_fast(data, &mut pos)?;
                            out.push(Value::Null);
                        }
                    }
                }
                None => {
                    for _ in 0..walk {
                        out.push(decode_value(data, &mut pos)?);
                    }
                }
            }
            // (The columns past the walk NULL; one added since the row was stored, and
            // needed, its value.)
            let stored = n.min(self.width);
            for _ in walk..stored {
                out.push(Value::Null);
            }
            for i in stored..self.width {
                let needed = self.decode.need.as_deref().is_none_or(|nd| nd[i]);
                out.push(match &self.decode.missing[i] {
                    Some(v) if needed => v.clone(),
                    _ => Value::Null,
                });
            }
            Ok(())
        })();
        if r.is_err() {
            truncate_row(out, base);
        }
        r
    }

    /// Stored row `bytes` appended to `out` (`decode_into`) when it passes the inner scan's
    /// filter and the join condition with outer row `l`: over the stored values first
    /// (`stored`: a row turned away there isn't decoded; a row stored without a header too,
    /// as a bulk load writes them), else over the row.
    fn add(&self, l: &[Value], bytes: &[u8], out: &mut Vec<Value>) -> Add {
        let mut checked = false;
        if let Some((c, whole)) = &self.stored
            && !crate::storage::toast::has_external(bytes)
            && let Some(pass) = super::scan::row_data(bytes).ok().and_then(|d| c.passes(d))
        {
            if !pass {
                return Add::Out;
            }
            checked = *whole;
        }
        let base = out.len();
        if self.decode_into(bytes, out).is_err() {
            return Add::Bad;
        }
        let got = {
            let view = Joined::Pair { l, r: super::pipe::Inner::plain(&out[base..]), keep: None };
            let filter = if checked { None } else { self.filter.as_ref() };
            let mut got = Add::Kept;
            for k in [filter, self.cond.as_ref()].into_iter().flatten() {
                match k.passes(&view) {
                    Some(true) => {}
                    Some(false) => {
                        got = Add::Out;
                        break;
                    }
                    None => {
                        got = Add::Left;
                        break;
                    }
                }
            }
            got
        };
        if !matches!(got, Add::Kept) {
            truncate_row(out, base);
        }
        got
    }

    /// The rows of the index entries under key `start`, in index order, `width` values a
    /// row (None: a read failed, or a row didn't decode).
    fn fetch(&self, start: &[u8], sc: &mut LookupScratch) -> Option<Vec<Value>> {
        self.entries(start, sc)?;
        let dkeys = std::mem::take(&mut sc.dkeys);
        let out = (|| {
            let mut rows = Vec::new();
            if dkeys.len() <= 1 {
                if let Some(d) = dkeys.first()
                    && let Some(b) = self.get(d, &mut sc.reader)?
                {
                    self.decode_into(&b, &mut rows).ok()?;
                }
                return Some(rows);
            }
            // (Fetched in key order; the rows then taken in the entries' order.)
            let mut sorted: Vec<&[u8]> = dkeys.iter().map(Vec::as_slice).collect();
            sorted.sort_unstable();
            sorted.dedup();
            let mut found: Vec<(&[u8], bytes::Bytes)> = Vec::with_capacity(sorted.len());
            for k in sorted {
                if let Some(b) = self.get(k, &mut sc.reader)? {
                    found.push((k, b));
                }
            }
            rows.reserve(dkeys.len() * self.width);
            for d in &dkeys {
                if let Ok(i) = found.binary_search_by(|(k, _)| (*k).cmp(d.as_slice())) {
                    self.decode_into(&found[i].1, &mut rows).ok()?;
                }
            }
            Some(rows)
        })();
        sc.dkeys = dkeys;
        out
    }

    /// `rows` of a semi or anti join: the index entries' rows fetched one at a time in
    /// index order up to the first that passes the filter and the join condition.
    fn first_row(&self, l: &[Value], start: &[u8], out: &mut Vec<Value>, sc: &mut LookupScratch) -> bool {
        if !self.unique {
            return self.first_row_scanned(l, start, out, sc);
        }
        if self.entries(start, sc).is_none() {
            return false;
        }
        let dkeys = std::mem::take(&mut sc.dkeys);
        let r = (|| {
            let mut read = 0u64;
            for (n, d) in dkeys.iter().enumerate() {
                let Some(b) = self.get(d, &mut sc.reader)? else { continue };
                match self.add(l, &b, out) {
                    Add::Kept => {
                        read += 1;
                        break;
                    }
                    Add::Out => read += 1,
                    Add::Bad => return None,
                    Add::Left => {
                        // (Left to the executor's thread: every visible row counted, as a
                        // whole fetch counts them.)
                        read += 1;
                        for d in &dkeys[n + 1..] {
                            if let Some(Some(_)) = self.get(d, &mut sc.reader) {
                                read += 1;
                            }
                        }
                        sc.fetched += read;
                        return Some(false);
                    }
                }
            }
            sc.fetched += read;
            Some(true)
        })();
        sc.dkeys = dkeys;
        match r {
            Some(done) => {
                sc.loops += 1;
                done
            }
            None => false,
        }
    }

    /// `first_row` of a key not unique: its entries read from the index scan one at a time,
    /// none past the first whose row passes (no list of the entries' row keys made first).
    fn first_row_scanned(&self, l: &[Value], start: &[u8], out: &mut Vec<Value>, sc: &mut LookupScratch) -> bool {
        let LookupScratch { reader, entries, end, fetched, loops, .. } = sc;
        prefix_end_into(start, end);
        if !entries.as_mut().is_some_and(|s| s.reseek(start, end, self.ts)) {
            *entries = Some(self.store.scan(start, end, self.ts));
        }
        let it = entries.as_mut().expect("made above");
        let mut read = 0u64;
        while let Some(e) = it.next() {
            let Ok((_, d)) = e else { return false };
            let Some(b) = self.get(&d, reader) else { return false };
            let Some(b) = b else { continue };
            match self.add(l, &b, out) {
                Add::Kept => {
                    read += 1;
                    break;
                }
                Add::Out => read += 1,
                Add::Bad => return false,
                Add::Left => {
                    // (Left to the executor's thread: every visible row counted, as a
                    // whole fetch counts them.)
                    read += 1;
                    for e in it.by_ref() {
                        let Ok((_, d)) = e else { return false };
                        match self.get(&d, reader) {
                            Some(Some(_)) => read += 1,
                            Some(None) => {}
                            None => return false,
                        }
                    }
                    *fetched += read;
                    *loops += 1;
                    return false;
                }
            }
        }
        *fetched += read;
        *loops += 1;
        true
    }

    /// Whether outer rows are looked up a batch at a time (`batch`): an inner or left join's
    /// of a table not kept (`LookupScratch::cache`). A semi or anti join's stops at its first
    /// match, mostly in its first entry's block, which the reader holds from the lookup before:
    /// a batch's sorts and searches cost it more than the misses it overlaps.
    pub fn batches(&self) -> bool {
        !self.cache && matches!(self.kind, JoinKind::Inner | JoinKind::Left)
    }

    /// `rows` of each of the outer rows `ls`, into `got` (each one's) and `out`: the index
    /// keys made, the entries read in key order (the entries' scan sought on from one key to
    /// the next), then every entry's row read by one sorted multi-get
    /// (`PointReader::get_sorted`: the cache misses of different outer rows' rows overlap,
    /// where one outer row's rows, adjacent, have few to overlap), and each outer row's rows
    /// taken in its entries' order, filtered and counted as `rows` does.
    pub fn batch(&self, ls: &[Row], got: &mut Vec<Got>, out: &mut Vec<Value>, sc: &mut LookupScratch) {
        got.clear();
        truncate_row(out, 0);
        let mut b = std::mem::take(&mut sc.batch);
        let BatchBufs { starts, wants, order, ekeys, espans, sorted, slot } = &mut b;
        starts.clear();
        wants.clear();
        order.clear();
        ekeys.clear();
        espans.clear();
        sorted.clear();
        // The index keys.
        for l in ls {
            let at = starts.len() as u32;
            starts.extend_from_slice(&self.prefix);
            let mut how = How::Batch;
            for (k, desc, nulls_first, coll, pad) in &self.keys {
                let Some(v) = k.run(l) else {
                    how = How::Left;
                    break;
                };
                if v.is_null() {
                    how = How::Null;
                    break;
                }
                let v = super::scan::padded(v, *pad);
                encode_index_value(starts, &v, *desc, *nulls_first, coll.as_ref());
            }
            wants.push(Want { key: (at, starts.len() as u32), how, ents: (0, 0), probed: false });
        }
        // The entries, in key order.
        let span = |s: &(u32, u32)| s.0 as usize..s.1 as usize;
        order.extend((0..wants.len() as u32).filter(|&i| wants[i as usize].how == How::Batch));
        order.sort_by(|&x, &y| starts[span(&wants[x as usize].key)].cmp(&starts[span(&wants[y as usize].key)]));
        let mut ok = true;
        for &i in order.iter() {
            let w = &mut wants[i as usize];
            let e0 = espans.len() as u32;
            match entry_spans(&self.store, self.ts, self.unique, &starts[span(&w.key)], sc, ekeys, espans) {
                Some(probed) => {
                    w.ents = (e0, espans.len() as u32);
                    w.probed = probed;
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        // Every entry's row, by one multi-get.
        let mut rows: Vec<Option<bytes::Bytes>> = Vec::new();
        if ok && !espans.is_empty() {
            ok = self.multi_get(ekeys, espans, order, sorted, slot, &mut rows, sc);
        }
        // Each outer row's rows, in turn.
        let mut one = Vec::new();
        for (l, w) in ls.iter().zip(wants.iter()) {
            let n = out.len();
            let mut how = w.how;
            if how == How::Batch {
                match ok.then(|| self.take(l, &rows, slot, w, out, sc)).flatten() {
                    Some(true) => got.push(Got::Rows(n as u32, out.len() as u32)),
                    Some(false) => {
                        truncate_row(out, n);
                        got.push(Got::Left);
                    }
                    // (A unique key's entry whose row the snapshot doesn't see, or a read
                    // that failed: read as `rows` reads it.)
                    None => {
                        truncate_row(out, n);
                        how = How::Alone;
                    }
                }
            }
            match how {
                How::Batch => {}
                How::Left => got.push(Got::Left),
                How::Null => {
                    // (An equality with NULL finds nothing: the scan still counts.)
                    sc.loops += 1;
                    got.push(Got::Rows(n as u32, n as u32));
                }
                How::Alone => match self.rows(l, &mut one, sc) {
                    true => {
                        out.append(&mut one);
                        got.push(Got::Rows(n as u32, out.len() as u32));
                    }
                    false => got.push(Got::Left),
                },
            }
        }
        sc.batch = b;
    }

    /// The rows of the entries' row keys (`espans`, spans of `ekeys`) into `rows`, by one
    /// multi-get through the worker's reader of the distinct keys ascending (`sorted`; each
    /// entry's place there into `slot`; `order` a buffer). False: a read failed.
    #[allow(clippy::too_many_arguments)]
    fn multi_get(
        &self,
        ekeys: &[u8],
        espans: &[(u32, u32)],
        order: &mut Vec<u32>,
        sorted: &mut Vec<(u32, u32)>,
        slot: &mut Vec<u32>,
        rows: &mut Vec<Option<bytes::Bytes>>,
        sc: &mut LookupScratch,
    ) -> bool {
        let span = |s: &(u32, u32)| &ekeys[s.0 as usize..s.1 as usize];
        order.clear();
        order.extend(0..espans.len() as u32);
        order.sort_unstable_by(|&x, &y| span(&espans[x as usize]).cmp(span(&espans[y as usize])));
        slot.clear();
        slot.resize(espans.len(), 0);
        for &e in order.iter() {
            let s = espans[e as usize];
            if sorted.last().is_none_or(|l| span(l) != span(&s)) {
                sorted.push(s);
            }
            slot[e as usize] = sorted.len() as u32 - 1;
        }
        let keys: Vec<&[u8]> = sorted.iter().map(span).collect();
        let r = sc.reader.get_or_insert_with(|| self.store.point_reader(self.ts, false));
        match r.get_sorted(&keys, true) {
            Ok(r) => {
                *rows = r;
                true
            }
            Err(_) => false,
        }
    }

    /// `batch`'s rows for outer row `l` (`w`) from the multi-get's `rows` (each entry's at
    /// its `slot`), filtered and counted as `rows` does, into `out`. Some(false): left to the
    /// executor's thread; None (nothing counted): a unique key's entry whose row the snapshot
    /// doesn't see.
    fn take(&self, l: &[Value], rows: &[Option<bytes::Bytes>], slot: &[u32], w: &Want, out: &mut Vec<Value>, sc: &mut LookupScratch) -> Option<bool> {
        let ents = w.ents.0 as usize..w.ents.1 as usize;
        let row_of = |e: usize| rows[slot[e] as usize].as_ref();
        let mut read = 0u64;
        for e in ents.clone() {
            let Some(v) = row_of(e) else {
                if w.probed {
                    return None;
                }
                continue;
            };
            match self.add(l, v, out) {
                Add::Kept | Add::Out => read += 1,
                Add::Bad => return Some(false),
                Add::Left => {
                    // (Left to the executor's thread: every visible row counted, as
                    // `rows` counts them.)
                    sc.loops += 1;
                    sc.fetched += ents.filter(|&e| row_of(e).is_some()).count() as u64;
                    return Some(false);
                }
            }
        }
        sc.loops += 1;
        sc.fetched += read;
        Some(true)
    }

    /// A worker's counts, into the stage's (`flush` counts them).
    pub fn take_counts(&self, sc: &mut LookupScratch) {
        self.loops.fetch_add(std::mem::take(&mut sc.loops), Ordering::Relaxed);
        self.fetched.fetch_add(std::mem::take(&mut sc.fetched), Ordering::Relaxed);
    }

    /// The join's rows for outer row `l` and its inner `rows` (`width` values a row), in
    /// order, to `f` (`own_last`: `Probe::emit`'s; the last row's values are moved into the
    /// joined row, not copied).
    pub fn emit(&self, l: Row, rows: &mut [Value], own_last: bool, f: &mut dyn FnMut(Joined)) {
        match self.kind {
            JoinKind::Semi if !rows.is_empty() => f(Joined::Row(l)),
            JoinKind::Anti if rows.is_empty() => f(Joined::Row(l)),
            JoinKind::Semi | JoinKind::Anti => {}
            JoinKind::Left if rows.is_empty() => {
                let mut o = l;
                o.extend(std::iter::repeat_n(Value::Null, self.nright));
                f(Joined::Row(o));
            }
            _ => {
                let keep = self.keep.as_deref();
                let last = rows.len().saturating_sub(self.width);
                if !own_last || rows.is_empty() {
                    for r in rows.chunks_exact(self.width) {
                        f(Joined::Pair { l: &l, r: super::pipe::Inner::plain(r), keep });
                    }
                    super::pipe::drop_row(l);
                    return;
                }
                let (rest, last) = rows.split_at_mut(last);
                for r in rest.chunks_exact(self.width) {
                    f(Joined::Pair { l: &l, r: super::pipe::Inner::plain(r), keep });
                }
                f(Joined::Row(super::pipe::joined_from_taken(l, last, keep)));
            }
        }
    }

    /// The scans' statistics, counted on the executor's thread (once: the counts start
    /// again).
    pub fn flush(&self, ctx: &super::ExecCtx) {
        let loops = self.loops.swap(0, Ordering::Relaxed);
        let fetched = self.fetched.swap(0, Ordering::Relaxed) as i64;
        if !super::pgstat::scan_counts(ctx) {
            return;
        }
        for _ in 0..loops {
            super::pgstat::count_index_scan(ctx, self.index);
        }
        if fetched > 0 {
            super::pgstat::count_returned(ctx, self.index, fetched);
            super::pgstat::count_fetched(ctx, self.index, fetched);
            super::pgstat::count_blocks(ctx, self.index, fetched);
        }
        let _ = self.table;
    }
}

/// `Lookup::entries`: the row keys of the index entries under key `start` visible at `ts`,
/// in index order, into `sc.dkeys`. A unique key's first visible entry is probed
/// (`Store::first_with`) and taken when its row is visible too; otherwise (no answer in a
/// few tries, or an entry whose row the snapshot doesn't see) the range is read as a scan,
/// the entries after it too.
/// `keys::prefix_end` of `prefix` into `end`.
fn prefix_end_into(prefix: &[u8], end: &mut Vec<u8>) {
    end.clear();
    end.extend_from_slice(prefix);
    while let Some(last) = end.last_mut() {
        if *last < 0xFF {
            *last += 1;
            return;
        }
        end.pop();
    }
    end.resize(prefix.len() + 1, 0xFF);
}

fn entry_keys(store: &Arc<Store>, ts: u64, unique: bool, start: &[u8], sc: &mut LookupScratch) -> Option<()> {
    let end = prefix_end(start);
    sc.dkeys.clear();
    if unique {
        use crate::storage::point::First;
        let r = sc.reader.get_or_insert_with(|| store.point_reader(ts, false));
        match store.first_with(r, start, &end, false).ok()? {
            First::Row(_, v) => {
                if row_at(store, ts, &v, &mut sc.reader)?.is_some() {
                    sc.dkeys.push(v.to_vec());
                    return Some(());
                }
            }
            First::None => return Some(()),
            First::Unknown => {}
        }
    }
    if !sc.entries.as_mut().is_some_and(|s| s.reseek(start, &end, ts)) {
        sc.entries = Some(store.scan(start, &end, ts));
    }
    for e in sc.entries.iter_mut().flatten() {
        sc.dkeys.push(e.ok()?.1.to_vec());
    }
    Some(())
}

/// `entry_keys` for `Lookup::batch`: the row keys appended to `ekeys` (each one's span to
/// `espans`). A unique key's probed entry is taken without its row's visibility checked
/// (Some(true): the multi-get checks it). None: a read failed.
fn entry_spans(
    store: &Arc<Store>,
    ts: u64,
    unique: bool,
    start: &[u8],
    sc: &mut LookupScratch,
    ekeys: &mut Vec<u8>,
    espans: &mut Vec<(u32, u32)>,
) -> Option<bool> {
    let mut push = |k: &[u8]| {
        let at = ekeys.len() as u32;
        ekeys.extend_from_slice(k);
        espans.push((at, ekeys.len() as u32));
    };
    let end = prefix_end(start);
    if unique {
        use crate::storage::point::First;
        let r = sc.reader.get_or_insert_with(|| store.point_reader(ts, false));
        match store.first_with(r, start, &end, false).ok()? {
            First::Row(_, v) => {
                push(&v);
                return Some(true);
            }
            First::None => return Some(false),
            First::Unknown => {}
        }
    }
    if !sc.entries.as_mut().is_some_and(|s| s.reseek(start, &end, ts)) {
        sc.entries = Some(store.scan(start, &end, ts));
    }
    for e in sc.entries.iter_mut().flatten() {
        push(&e.ok()?.1);
    }
    Some(false)
}

/// Row `d` visible at `ts`, through `reader` (None inside: not visible; None: a read failed).
fn row_at(store: &Store, ts: u64, d: &[u8], reader: &mut Option<crate::storage::point::PointReader>) -> Option<Option<bytes::Bytes>> {
    let r = reader.get_or_insert_with(|| store.point_reader(ts, false));
    match store.get_with(r, d, true).ok()? {
        Some((_, Some(b))) => Some(Some(b)),
        _ => Some(None),
    }
}

/// A plain btree index scan by equality that does nothing per row on the executor's
/// thread (`Lookup`'s conditions).
fn plain_index_scan(s: &ScanPlan, ctx: &super::ExecCtx) -> bool {
    let Some((ix, b)) = &s.index else { return false };
    // (An index-only scan's rows are its entries' values, the heap rows' at the snapshot.)
    matches!(s.access, Access::Index { .. })
        && ix.method == "btree"
        && super::scan::reads_entries(ix)
        && b.any.is_none()
        && b.lower.is_none()
        && b.upper.is_none()
        && b.order.is_empty()
        && !b.eq.is_empty()
        && !s.reverse
        && s.mark.is_none()
        && !s.with_key
        && !s.sys
        && s.limit.is_none()
        && s.current_of.is_none()
        && s.sample.is_none()
        && s.excl.is_none()
        && s.part.is_none()
        && s.table.foreign.is_none()
        && !(s.table.storage_pkey().is_some() && s.table.pk_clustered())
        && !ctx.instrumented()
        && !ctx.gone_rels.borrow().contains(&s.table.oid)
        && !ctx.catalog().views.contains_key(&s.table.oid)
        && !super::scan::predicate_locked(&s.table, ctx)
}

#[cfg(test)]
mod tests {
    use crate::storage::{Store, StoreOptions};
    use bytes::Bytes;

    /// A unique key's probe whose first visible entry names a row the snapshot doesn't see
    /// reads the range as a scan: a later entry under the key, whose row the snapshot sees,
    /// is found (the probe alone would stop at the first).
    #[test]
    fn unique_probe_past_an_entry_without_its_row() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions { sync: false, ..Default::default() }).unwrap();
        let key = b"ix\x00\x00\x00\x01\x00\x02k5".to_vec();
        let entry = |row: &[u8]| [key.as_slice(), row].concat();
        let (gone, live) = (b"row-1".to_vec(), b"row-2".to_vec());
        let rows = vec![
            (entry(&gone), Some(Bytes::from(gone.clone()))),
            (entry(&live), Some(Bytes::from(live.clone()))),
            (live.clone(), Some(Bytes::from_static(b"v"))),
        ];
        store.commit(rows, |_| Ok(())).unwrap();
        let mut sc = super::LookupScratch::default();
        super::entry_keys(&store, store.visible_ts(), true, &key, &mut sc).unwrap();
        assert!(sc.dkeys.contains(&live), "the visible row's entry is read: {:?}", sc.dkeys);
    }
}
