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
use super::kernel::{K, compile};
use super::parscan::Decode;
use super::pipe::Joined;
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
    cache: std::collections::HashMap<Vec<u8>, std::rc::Rc<Vec<Row>>>,
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
        Ok(Some(Lookup {
            store,
            ts,
            prefix: index_prefix(s.table.oid, ix.id),
            keys,
            decode: Decode {
                need,
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
    /// filter and the join condition), in the serial join's order, into `out`. False:
    /// something is left to `eval` or failed (the row goes to the executor's thread, which
    /// runs it as the serial join does). `sc`: the worker's counts and cache.
    pub fn rows(&self, l: &[Value], out: &mut Vec<Row>, sc: &mut LookupScratch) -> bool {
        out.clear();
        let mut start = self.prefix.clone();
        for (k, desc, nulls_first, coll, pad) in &self.keys {
            let Some(v) = k.run(l) else { return false };
            if v.is_null() {
                // (An equality with NULL finds nothing: the scan still counts.)
                sc.loops += 1;
                return true;
            }
            // (A bpchar probe of a char(n) key, padded as the key is stored.)
            let v = super::scan::padded(v, *pad);
            encode_index_value(&mut start, &v, *desc, *nulls_first, coll.as_ref());
        }
        // (A semi or anti join's inner scan stops at its first match, as ExecNestLoop's
        // does: the rows after it aren't fetched.)
        let first = matches!(self.kind, JoinKind::Semi | JoinKind::Anti);
        if first && !self.cache {
            return self.first_row(l, &start, out, sc);
        }
        let found = match sc.cache.get(&start) {
            Some(rows) => rows.clone(),
            None => {
                let Some(rows) = self.fetch(&start, sc) else { return false };
                let rows = std::rc::Rc::new(rows);
                if self.cache {
                    if sc.cache.len() >= CACHE_KEYS {
                        sc.cache.clear();
                    }
                    sc.cache.insert(start, rows.clone());
                }
                rows
            }
        };
        sc.loops += 1;
        let mut read = 0u64;
        for r in found.iter() {
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
                        sc.fetched += found.len() as u64;
                        return false;
                    }
                }
            }
            if pass {
                out.push(r.clone());
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

    /// The rows of the index entries under key `start`, in index order (None: a read
    /// failed, or a row didn't decode).
    fn fetch(&self, start: &[u8], sc: &mut LookupScratch) -> Option<Vec<Row>> {
        self.entries(start, sc)?;
        let dkeys = std::mem::take(&mut sc.dkeys);
        let out = (|| {
            if dkeys.len() <= 1 {
                return match dkeys.first() {
                    None => Some(Vec::new()),
                    Some(d) => Some(match self.get(d, &mut sc.reader)? {
                        Some(b) => vec![self.decode.row(d, &b).ok()?],
                        None => Vec::new(),
                    }),
                };
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
            let mut rows = Vec::with_capacity(dkeys.len());
            for d in &dkeys {
                if let Ok(i) = found.binary_search_by(|(k, _)| (*k).cmp(d.as_slice())) {
                    rows.push(self.decode.row(d, &found[i].1).ok()?);
                }
            }
            Some(rows)
        })();
        sc.dkeys = dkeys;
        out
    }

    /// `rows` of a semi or anti join: the index entries' rows fetched one at a time in
    /// index order up to the first that passes the filter and the join condition.
    fn first_row(&self, l: &[Value], start: &[u8], out: &mut Vec<Row>, sc: &mut LookupScratch) -> bool {
        if self.entries(start, sc).is_none() {
            return false;
        }
        let dkeys = std::mem::take(&mut sc.dkeys);
        let r = (|| {
            let mut read = 0u64;
            for (n, d) in dkeys.iter().enumerate() {
                let Some(b) = self.get(d, &mut sc.reader)? else { continue };
                let row = self.decode.row(d, &b).ok()?;
                read += 1;
                let view = Joined::Pair { l, r: super::pipe::Inner::plain(&row), keep: None };
                let mut pass = true;
                for k in [&self.filter, &self.cond].into_iter().flatten() {
                    match k.passes(&view) {
                        Some(true) => {}
                        Some(false) => {
                            pass = false;
                            break;
                        }
                        None => {
                            // (Left to the executor's thread: every visible row counted,
                            // as a whole fetch counts them.)
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
                if pass {
                    out.push(row);
                    break;
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

    /// A worker's counts, into the stage's (`flush` counts them).
    pub fn take_counts(&self, sc: &mut LookupScratch) {
        self.loops.fetch_add(std::mem::take(&mut sc.loops), Ordering::Relaxed);
        self.fetched.fetch_add(std::mem::take(&mut sc.fetched), Ordering::Relaxed);
    }

    /// The join's rows for outer row `l` and its inner `rows`, in order, to `f`.
    pub fn emit(&self, l: Row, rows: &[Row], f: &mut dyn FnMut(Joined)) {
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
                for r in rows {
                    f(Joined::Pair { l: &l, r: super::pipe::Inner::plain(r), keep: self.keep.as_deref() });
                }
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
