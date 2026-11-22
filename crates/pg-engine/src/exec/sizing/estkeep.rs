//! The estimates of tables too big to model (`EstHeap`, the btrees' `btest` summaries),
//! durable: what a restart or a crash would otherwise lose (the versions' places past the
//! model, the fill and fresh pages, the btrees' leaves), so a table's sizes, plans, serial
//! scan order and float sums are the same after it as before.
//!
//! - A table's record is its estimate in full (`est_key`, the key its heap pages were kept
//!   under before), then what changed since, one record a commit (`est_key ++ seq`): the
//!   places set since (`Log`), the runs begun, the btrees' changed leaves.
//! - A commit takes the changes of every table whose estimate changed (`take`, from the
//!   commit's `check`, under the store's commit lock): they commit with it, in commit order,
//!   so after a crash the records give the estimate as of the last commit. (What a
//!   transaction that never committed placed is in the next commit's changes, as
//!   PostgreSQL's file keeps an aborted transaction's tuples.)
//! - The changes are folded into a new full record when they outgrow it, and when the
//!   database closes (`persist`).
//! - A record is used only while it is of the table's relfilenode, under the model limit it
//!   was too big for (`restore`); temporary and unlogged tables keep none.

use super::*;
use std::sync::atomic::Ordering;

/// A full record's and a change record's first byte.
const FULL: u8 = 0xe1;
const CHANGES: u8 = 0xe2;

/// A record's bytes, written in order (little-endian).
pub(crate) struct Enc(pub Vec<u8>);

impl Enc {
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u128(&mut self, v: u128) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn f64(&mut self, v: f64) {
        self.u64(v.to_bits());
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn tid(&mut self, t: Tid) {
        self.u32(t.0);
        self.u16(t.1);
    }
}

/// A record read back (`Enc`'s order); None past its end.
pub(crate) struct Dec<'a>(pub &'a [u8]);

impl Dec<'_> {
    fn take<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (a, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*a)
    }
    pub fn u8(&mut self) -> Option<u8> {
        self.take::<1>().map(|b| b[0])
    }
    pub fn u16(&mut self) -> Option<u16> {
        self.take().map(u16::from_le_bytes)
    }
    pub fn u32(&mut self) -> Option<u32> {
        self.take().map(u32::from_le_bytes)
    }
    pub fn u64(&mut self) -> Option<u64> {
        self.take().map(u64::from_le_bytes)
    }
    pub fn u128(&mut self) -> Option<u128> {
        self.take().map(u128::from_le_bytes)
    }
    pub fn f64(&mut self) -> Option<f64> {
        self.u64().map(f64::from_bits)
    }
    pub fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        let (b, rest) = (self.0.get(..n)?, &self.0[n..]);
        self.0 = rest;
        Some(b.to_vec())
    }
    fn tid(&mut self) -> Option<Tid> {
        Some((self.u32()?, self.u16()?))
    }
}

/// What changed in an `EstHeap` since it was last made durable: the places set since, in
/// order (`EstHeap::moved`), the runs kept then, and whether the places were dropped since
/// (`EstHeap::unplace`).
#[derive(Clone, Default)]
pub(super) struct Log {
    pub moved: Vec<(u64, Tid)>,
    pub runs_saved: usize,
    pub reset: bool,
}

/// What the store holds of a table's estimate: its change records after the full one
/// (`seq`; None: not known, those there are found when the next full record replaces them),
/// their bytes and the full record's, and whether the next record must be a full one.
#[derive(Clone, Default)]
pub(crate) struct Saved {
    seq: Option<u64>,
    full_bytes: usize,
    changes_bytes: usize,
    full_next: bool,
}

impl Saved {
    pub(super) fn full_next(&mut self, v: bool) {
        self.full_next = v;
    }
}

fn change_key(oid: u32, seq: u64) -> Vec<u8> {
    let mut k = est_key(oid);
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

/// An estimate changed: the next commit makes it durable (`take`).
pub(super) fn touched(db: &Database, e: &mut EstHeap) {
    e.dirty = true;
    note(db);
}

pub(super) fn note(db: &Database) {
    if !db.relfiles.est_dirty.load(Ordering::Relaxed) {
        db.relfiles.est_dirty.store(true, Ordering::Release);
    }
}

/// Table `oid`'s estimate went (dropped, truncated, modelled again): its records go with the
/// next commit.
pub(super) fn gone(db: &Database, rf: &mut RelFiles, oid: u32) {
    rf.est_gone.push(oid);
    note(db);
}

/// The estimate in full, its btrees' (`ixs`) summaries with it.
fn encode_heap_full(e: &mut Enc, h: &EstHeap) {
    scalars(e, h);
    e.u32(h.runs.len() as u32);
    for r in &h.runs {
        e.bytes(&r.0);
        e.tid((r.1, r.2));
    }
    e.u32(h.moved.len() as u32);
    for (k, t) in &h.moved {
        e.u64(*k);
        e.tid(*t);
    }
}

fn scalars(e: &mut Enc, h: &EstHeap) {
    e.u32(h.pages);
    e.u32(h.fill);
    e.u64(h.room as u64);
    e.u16(h.items);
    e.u64(h.reusable as u64);
    e.u32(h.fresh);
    e.u8(match h.order {
        EstOrder::InOrder => 0,
        EstOrder::Placed => 1,
        EstOrder::Unplaced => 2,
    });
    e.u64(h.writes);
    e.u8(h.fill_bulk as u8 | (h.in_place as u8) << 1 | (h.skip_fsm as u8) << 2);
    match &h.fill_key {
        Some(k) => {
            e.u8(1);
            e.bytes(k);
        }
        None => e.u8(0),
    }
    match &h.tail {
        Some(t) => {
            e.u8(1);
            e.u32(t.page);
            e.u64(t.room as u64);
            e.u16(t.items);
            e.bytes(&t.key);
        }
        None => e.u8(0),
    }
}

fn decode_scalars(d: &mut Dec, h: &mut EstHeap) -> Option<()> {
    h.pages = d.u32()?;
    h.fill = d.u32()?;
    h.room = d.u64()? as usize;
    h.items = d.u16()?;
    h.reusable = d.u64()? as usize;
    h.fresh = d.u32()?;
    h.order = match d.u8()? {
        0 => EstOrder::InOrder,
        1 => EstOrder::Placed,
        2 => EstOrder::Unplaced,
        _ => return None,
    };
    h.writes = d.u64()?;
    let f = d.u8()?;
    (h.fill_bulk, h.in_place, h.skip_fsm) = (f & 1 != 0, f & 2 != 0, f & 4 != 0);
    h.fill_key = match d.u8()? {
        0 => None,
        _ => Some(d.bytes()?),
    };
    h.tail = match d.u8()? {
        0 => None,
        _ => Some(Tail { page: d.u32()?, room: d.u64()? as usize, items: d.u16()?, key: d.bytes()? }),
    };
    Some(())
}

/// The heap's changes since it was last made durable.
fn encode_heap_changes(e: &mut Enc, h: &EstHeap) {
    scalars(e, h);
    e.u8(h.log.reset as u8);
    // (The last run kept is written again: its key may have come after it, `stored_run_key`.)
    let from = if h.log.reset { 0 } else { h.log.runs_saved.saturating_sub(1).min(h.runs.len()) };
    e.u32(from as u32);
    e.u32((h.runs.len() - from) as u32);
    for r in &h.runs[from..] {
        e.bytes(&r.0);
        e.tid((r.1, r.2));
    }
    e.u32(h.log.moved.len() as u32);
    for (k, t) in &h.log.moved {
        e.u64(*k);
        e.tid(*t);
    }
}

fn decode_heap(d: &mut Dec, full: bool, h: &mut EstHeap) -> Option<()> {
    decode_scalars(d, h)?;
    // (A full record, or changes after the places were dropped: `EstHeap::unplace`.)
    if full || d.u8()? != 0 {
        h.runs.clear();
        h.moved.clear();
    }
    let from = if full { 0 } else { d.u32()? as usize };
    if from > h.runs.len() {
        return None;
    }
    h.runs.truncate(from);
    for _ in 0..d.u32()? {
        let k = d.bytes()?;
        let t = d.tid()?;
        h.runs.push((k, t.0, t.1));
    }
    for _ in 0..d.u32()? {
        let k = d.u64()?;
        h.moved.insert(k, d.tid()?);
    }
    Some(())
}

/// The table's btree indexes (its primary key's too).
fn btree_oids(t: &Table) -> Vec<u32> {
    let mut v: Vec<u32> = t.indexes.iter().chain(&t.pkey_index).copied().collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Make the estimates that changed since the last commit durable with this one: their
/// records go into `writes` (the commit's, under the store's commit lock: one commit at a
/// time takes them, in commit order). `cat`: the catalog the commit publishes, if it changes
/// it.
pub fn take(db: &Database, cat: Option<&Catalog>, writes: &mut Vec<(Vec<u8>, Option<bytes::Bytes>)>) {
    // (A load first: most commits find nothing, and write no line other sessions read.)
    if !db.relfiles.est_dirty.load(Ordering::Acquire) || !db.relfiles.est_dirty.swap(false, Ordering::AcqRel) {
        return;
    }
    let latest;
    let cat = match cat {
        Some(c) => c,
        None => {
            latest = db.latest_catalog();
            &latest
        }
    };
    let mut again = false;
    for shard in db.relfiles.all() {
        let mut rf = shard.write();
        again |= take_shard(db, cat, &mut rf, writes, false);
    }
    // (A table whose relfilenode isn't committed yet: its estimate waits for that commit.)
    if again {
        note(db);
    }
}

/// `take` for one shard; `all`: every estimate, in full (the database is closing). True
/// when one is left for a later commit.
fn take_shard(db: &Database, cat: &Catalog, rf: &mut RelFiles, writes: &mut Vec<(Vec<u8>, Option<bytes::Bytes>)>, all: bool) -> bool {
    let RelFiles { estimated, est_btrees, est_saved, est_gone, .. } = rf;
    for oid in est_gone.drain(..) {
        if estimated.contains_key(&oid) {
            continue;
        }
        let saved = est_saved.remove(&oid).unwrap_or_default();
        drop_records(db, oid, saved.seq, writes);
    }
    let mut left = false;
    for (&oid, e) in estimated.iter_mut() {
        let saved = est_saved.entry(oid).or_insert_with(|| Saved { full_next: true, ..Default::default() });
        if !(e.dirty || saved.full_next || all) {
            continue;
        }
        // (Not committed yet, by another transaction: its commit takes it.)
        let Some(t) = cat.tables.get(&oid) else {
            left = true;
            continue;
        };
        let ixs = btree_oids(t);
        // (Gone with the session or emptied by a crash: nothing to keep.)
        if t.temporary || t.unlogged {
            e.dirty = false;
            e.log = Log::default();
            for i in &ixs {
                if let Some(b) = est_btrees.get_mut(i) {
                    b.encode(&mut Enc(Vec::new()), false);
                }
            }
            continue;
        }
        let Some(node) = cat.relfilenode(oid).filter(|n| *n != 0) else {
            left = true;
            continue;
        };
        // (Folded by bytes alone: the change records cost at most the full record's bytes
        // again. A fold after a fixed count of them wrote a big table's whole estimate, a
        // megabyte and more with its places, every 256 small commits: to the log, the
        // memtable and every compaction after it.)
        let full = all || saved.full_next || saved.seq.is_none() || saved.changes_bytes > saved.full_bytes.max(1 << 16);
        let mut enc = Enc(Vec::with_capacity(256));
        enc.u8(if full { FULL } else { CHANGES });
        enc.u32(node);
        enc.u32(e.limit);
        if full {
            encode_heap_full(&mut enc, e);
        } else {
            encode_heap_changes(&mut enc, e);
        }
        let mut n = 0u32;
        let at = enc.0.len();
        enc.u32(0);
        for i in &ixs {
            if let Some(b) = est_btrees.get_mut(i)
                && (full || b.changed())
            {
                enc.u32(*i);
                b.encode(&mut enc, full);
                n += 1;
            }
        }
        enc.0[at..at + 4].copy_from_slice(&n.to_le_bytes());
        let len = enc.0.len();
        if full {
            drop_records(db, oid, saved.seq, writes);
            writes.push((est_key(oid), Some(bytes::Bytes::from(enc.0))));
            *saved = Saved { seq: Some(0), full_bytes: len, changes_bytes: 0, full_next: false };
        } else {
            let seq = saved.seq.unwrap_or(0) + 1;
            writes.push((change_key(oid, seq), Some(bytes::Bytes::from(enc.0))));
            saved.seq = Some(seq);
            saved.changes_bytes += len;
        }
        e.dirty = false;
        e.log = Log { moved: Vec::new(), runs_saved: e.runs.len(), reset: false };
    }
    left
}

/// Delete table `oid`'s records: its full one and the changes after it (`seq` of them; None:
/// those the store has).
fn drop_records(db: &Database, oid: u32, seq: Option<u64>, writes: &mut Vec<(Vec<u8>, Option<bytes::Bytes>)>) {
    match seq {
        Some(n) => {
            writes.push((est_key(oid), None));
            writes.extend((1..=n).map(|s| (change_key(oid, s), None)));
        }
        None => {
            let start = est_key(oid);
            let end = prefix_end(&start);
            for (k, _) in db.store.scan(&start, &end, db.store.visible_ts()).flatten() {
                writes.push((k, None));
            }
        }
    }
}

/// The database is closing: every estimate's changes folded into a full record.
pub fn persist(db: &Database) {
    let cat = db.latest_catalog();
    let mut writes = Vec::new();
    for shard in db.relfiles.all() {
        let mut rf = shard.write();
        let all = rf.estimated.values().any(|e| e.dirty) || rf.est_saved.values().any(|s| s.seq != Some(0)) || !rf.est_gone.is_empty();
        if all {
            take_shard(db, &cat, &mut rf, &mut writes, true);
        }
    }
    if writes.is_empty() {
        return;
    }
    if let Err(e) = db.store.commit(writes, |_| Ok(())) {
        tracing::warn!(error = %e, "keeping the estimates of tables too big to model failed");
    }
}

/// Table `t`'s estimate as its records have it, when they are of its relfilenode and it is
/// still too big to model; with its btrees' summaries (those of its indexes now) and what
/// the store holds.
pub(super) fn restore(t: &Table, ctx: &ExecCtx) -> Option<(EstHeap, Vec<(u32, btest::BtEst)>, Saved)> {
    let store = &ctx.sess.db.store;
    let start = est_key(t.oid);
    let end = prefix_end(&start);
    let node = ctx.catalog().relfilenode(t.oid)?;
    let ixs = btree_oids(t);
    let mut it = store.scan(&start, &end, store.visible_ts());
    let (k, v) = it.next()?.ok()?;
    if k != start || v.first() != Some(&FULL) {
        return None;
    }
    let mut h = EstHeap::default();
    let mut btrees: HashMap<u32, btest::BtEst> = HashMap::new();
    let mut saved = Saved { seq: Some(0), full_bytes: v.len(), changes_bytes: 0, full_next: false };
    let mut apply = |v: &[u8], full: bool, btrees: &mut HashMap<u32, btest::BtEst>| -> Option<()> {
        let mut d = Dec(&v[1..]);
        let (n, l) = (d.u32()?, d.u32()?);
        if n != node || max_versions() > l as usize {
            return None;
        }
        h.limit = l;
        decode_heap(&mut d, full, &mut h)?;
        for _ in 0..d.u32()? {
            let i = d.u32()?;
            let b = btest::BtEst::decode(&mut d, btrees.remove(&i))?;
            btrees.insert(i, b);
        }
        Some(())
    };
    apply(&v, true, &mut btrees)?;
    for r in it {
        let (k, v) = r.ok()?;
        let seq = u64::from_be_bytes(k.get(start.len()..)?.try_into().ok()?);
        if v.first() != Some(&CHANGES) || seq != saved.seq? + 1 {
            return None;
        }
        apply(&v, false, &mut btrees)?;
        saved.seq = Some(seq);
        saved.changes_bytes += v.len();
    }
    h.log = Log { moved: Vec::new(), runs_saved: h.runs.len(), reset: false };
    let btrees = btrees.into_iter().filter(|(i, _)| ixs.contains(i)).collect();
    Some((h, btrees, saved))
}
