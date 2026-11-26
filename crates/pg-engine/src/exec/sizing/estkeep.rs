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

/// A full record's and a change record's first byte: a change record holds what changed
/// whole (`CHANGES`, a build before the compact form's: still read), or only the fields
/// that differ from the record before (`COMPACT`). A build before `COMPACT` takes a table
/// with such a record as one without (estimated afresh).
const FULL: u8 = 0xe1;
const CHANGES: u8 = 0xe2;
const COMPACT: u8 = 0xe3;

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
    pub fn varint(&mut self, v: u64) {
        crate::storage::sst::put_varint(&mut self.0, v);
    }
    /// A signed difference (zigzag).
    pub fn diff(&mut self, old: u64, new: u64) {
        let d = new.wrapping_sub(old) as i64;
        self.varint(((d << 1) ^ (d >> 63)) as u64);
    }
    /// `new` as the decoder makes it from `old`: `old` plus a whole number when that sum is
    /// exactly `new` (a count's step, an entry's bytes added), else its bits.
    pub fn fdiff(&mut self, old: f64, new: f64) {
        let d = (new - old).round();
        if d.abs() < (1u64 << 52) as f64 && (old + d).to_bits() == new.to_bits() {
            let d = d as i64;
            self.varint((((d << 1) ^ (d >> 63)) as u64) << 1);
        } else {
            self.varint(1);
            self.f64(new);
        }
    }
    pub fn vbytes(&mut self, b: &[u8]) {
        self.varint(b.len() as u64);
        self.0.extend_from_slice(b);
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
    pub fn varint(&mut self) -> Option<u64> {
        let mut p = 0;
        let v = crate::storage::sst::get_varint(self.0, &mut p).ok()?;
        self.0 = &self.0[p..];
        Some(v)
    }
    /// `Enc::diff`'s value applied to `old`.
    pub fn diff(&mut self, old: u64) -> Option<u64> {
        let z = self.varint()?;
        let d = ((z >> 1) as i64) ^ -((z & 1) as i64);
        Some(old.wrapping_add(d as u64))
    }
    /// `Enc::fdiff`'s value applied to `old`.
    pub fn fdiff(&mut self, old: f64) -> Option<f64> {
        let z = self.varint()?;
        if z & 1 != 0 {
            return self.f64();
        }
        let z = z >> 1;
        let d = ((z >> 1) as i64) ^ -((z & 1) as i64);
        Some(old + d as f64)
    }
    pub fn vbytes(&mut self) -> Option<Vec<u8>> {
        let n = self.varint()? as usize;
        let (b, rest) = (self.0.get(..n)?, &self.0[n..]);
        self.0 = rest;
        Some(b.to_vec())
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

/// An `EstHeap`'s fields as last made durable (`take`, `restore`): a change record holds
/// those that differ. `nums`: `Sent::nums`'s order.
#[derive(Clone, Default)]
pub(super) struct Sent {
    nums: [u64; 9],
    fill_key: Option<Vec<u8>>,
    tail: Option<(u32, usize, u16, Vec<u8>)>,
    last_run: Option<(Vec<u8>, u32, u16)>,
}

impl Sent {
    fn of(h: &EstHeap) -> Sent {
        Sent {
            nums: Self::nums(h),
            fill_key: h.fill_key.clone(),
            tail: h.tail.as_ref().map(|t| (t.page, t.room, t.items, t.key.clone())),
            last_run: h.runs.last().cloned(),
        }
    }

    /// pages, fill, room, items, reusable, fresh, order, writes, flags.
    fn nums(h: &EstHeap) -> [u64; 9] {
        [
            h.pages as u64,
            h.fill as u64,
            h.room as u64,
            h.items as u64,
            h.reusable as u64,
            h.fresh as u64,
            order_byte(h.order) as u64,
            h.writes,
            (h.fill_bulk as u8 | (h.in_place as u8) << 1 | (h.skip_fsm as u8) << 2) as u64,
        ]
    }
}

/// `Sent::nums` by how often an UPDATE changes them (a bit each in a change record).
const NUM_ORDER: [usize; 9] = [7, 2, 3, 1, 0, 4, 5, 6, 8];

fn order_byte(o: EstOrder) -> u8 {
    match o {
        EstOrder::InOrder => 0,
        EstOrder::Placed => 1,
        EstOrder::Unplaced => 2,
    }
}

fn order_of(b: u8) -> Option<EstOrder> {
    Some(match b {
        0 => EstOrder::InOrder,
        1 => EstOrder::Placed,
        2 => EstOrder::Unplaced,
        _ => return None,
    })
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
    e.u8(order_byte(h.order));
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
    h.order = order_of(d.u8()?)?;
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

/// The heap's changes since it was last made durable (`sent`): a bit each for the fields
/// that differ, the places dropped, the runs and the places set, then those (numbers as
/// differences, the rest as they are). An UPDATE past the model changes a few numbers and
/// places its new version: ~15 bytes where its whole fields took ~140.
fn encode_heap_changes(e: &mut Enc, h: &EstHeap, sent: &mut Sent) {
    let now = Sent::nums(h);
    let mut mask = 0u64;
    for (bit, &i) in NUM_ORDER.iter().enumerate() {
        mask |= ((now[i] != sent.nums[i]) as u64) << bit;
    }
    let tail_now = h.tail.as_ref().map(|t| (t.page, t.room, t.items, &t.key[..]));
    let tail_was = sent.tail.as_ref().map(|t| (t.0, t.1, t.2, &t.3[..]));
    // (The last run kept may have changed: its key may have come after it, `stored_run_key`.)
    let runs = h.log.reset || h.runs.len() != h.log.runs_saved || h.runs.last() != sent.last_run.as_ref();
    mask |= ((h.fill_key != sent.fill_key) as u64) << 9
        | ((tail_now != tail_was) as u64) << 10
        | (h.log.reset as u64) << 11
        | (runs as u64) << 12
        | (!h.log.moved.is_empty() as u64) << 13;
    e.varint(mask);
    for (bit, &i) in NUM_ORDER.iter().enumerate() {
        if mask & 1 << bit != 0 {
            e.diff(sent.nums[i], now[i]);
        }
    }
    if mask & 1 << 9 != 0 {
        match &h.fill_key {
            Some(k) => {
                e.varint(k.len() as u64 + 1);
                e.0.extend_from_slice(k);
            }
            None => e.varint(0),
        }
        sent.fill_key = h.fill_key.clone();
    }
    if mask & 1 << 10 != 0 {
        match &h.tail {
            Some(t) => {
                e.u8(1);
                e.varint(t.page as u64);
                e.varint(t.room as u64);
                e.varint(t.items as u64);
                e.vbytes(&t.key);
            }
            None => e.u8(0),
        }
        sent.tail = h.tail.as_ref().map(|t| (t.page, t.room, t.items, t.key.clone()));
    }
    if runs {
        let from = if h.log.reset { 0 } else { h.log.runs_saved.saturating_sub(1).min(h.runs.len()) };
        e.varint(from as u64);
        e.varint((h.runs.len() - from) as u64);
        for r in &h.runs[from..] {
            e.vbytes(&r.0);
            e.varint(r.1 as u64);
            e.varint(r.2 as u64);
        }
        sent.last_run = h.runs.last().cloned();
    }
    if !h.log.moved.is_empty() {
        e.varint(h.log.moved.len() as u64);
        for (k, t) in &h.log.moved {
            e.u64(*k);
            e.varint(t.0 as u64);
            e.varint(t.1 as u64);
        }
    }
    sent.nums = now;
}

/// `encode_heap_changes`' record applied to `h`.
fn decode_heap_changes(d: &mut Dec, h: &mut EstHeap) -> Option<()> {
    let mut n = Sent::nums(h);
    let mask = d.varint()?;
    for (bit, &i) in NUM_ORDER.iter().enumerate() {
        if mask & 1 << bit != 0 {
            n[i] = d.diff(n[i])?;
        }
    }
    h.pages = u32::try_from(n[0]).ok()?;
    h.fill = u32::try_from(n[1]).ok()?;
    h.room = n[2] as usize;
    h.items = u16::try_from(n[3]).ok()?;
    h.reusable = n[4] as usize;
    h.fresh = u32::try_from(n[5]).ok()?;
    h.order = order_of(u8::try_from(n[6]).ok()?)?;
    h.writes = n[7];
    (h.fill_bulk, h.in_place, h.skip_fsm) = (n[8] & 1 != 0, n[8] & 2 != 0, n[8] & 4 != 0);
    if mask & 1 << 9 != 0 {
        h.fill_key = match d.varint()? {
            0 => None,
            k => {
                let k = k as usize - 1;
                let (b, rest) = (d.0.get(..k)?, &d.0[k..]);
                d.0 = rest;
                Some(b.to_vec())
            }
        };
    }
    if mask & 1 << 10 != 0 {
        h.tail = match d.u8()? {
            0 => None,
            _ => Some(Tail {
                page: u32::try_from(d.varint()?).ok()?,
                room: d.varint()? as usize,
                items: u16::try_from(d.varint()?).ok()?,
                key: d.vbytes()?,
            }),
        };
    }
    if mask & 1 << 11 != 0 {
        h.runs.clear();
        h.moved.clear();
    }
    if mask & 1 << 12 != 0 {
        let from = d.varint()? as usize;
        if from > h.runs.len() {
            return None;
        }
        h.runs.truncate(from);
        for _ in 0..d.varint()? {
            let k = d.vbytes()?;
            h.runs.push((k, u32::try_from(d.varint()?).ok()?, u16::try_from(d.varint()?).ok()?));
        }
    }
    if mask & 1 << 13 != 0 {
        for _ in 0..d.varint()? {
            let k = d.u64()?;
            h.moved.insert(k, (u32::try_from(d.varint()?).ok()?, u16::try_from(d.varint()?).ok()?));
        }
    }
    Some(())
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
            e.sent = None;
            for i in &ixs {
                if let Some(b) = est_btrees.get_mut(i) {
                    b.forget_changes();
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
        let full = all
            || saved.full_next
            || saved.seq.is_none()
            || e.sent.is_none()
            || saved.changes_bytes > saved.full_bytes.max(1 << 16);
        let parts: Vec<u32> = ixs.iter().copied().filter(|i| est_btrees.get(i).is_some_and(|b| full || b.changed())).collect();
        let mut enc = Enc(Vec::with_capacity(if full { 256 } else { 64 }));
        if full {
            enc.u8(FULL);
            enc.u32(node);
            enc.u32(e.limit);
            encode_heap_full(&mut enc, e);
            enc.u32(parts.len() as u32);
            e.sent = Some(Sent::of(e));
        } else {
            enc.u8(COMPACT);
            enc.varint(node as u64);
            enc.varint(e.limit as u64);
            let mut sent = e.sent.take().expect("checked");
            encode_heap_changes(&mut enc, e, &mut sent);
            e.sent = Some(sent);
            enc.varint(parts.len() as u64);
        }
        for i in parts {
            let b = est_btrees.get_mut(&i).expect("listed");
            if full {
                enc.u32(i);
            } else {
                enc.varint(i as u64);
            }
            b.encode(&mut enc, full);
        }
        // (Its bytes alone: `Bytes::from` keeps a vector's spare capacity, up to half of a
        // full record's megabytes, for as long as the memtable holds it.)
        enc.0.shrink_to_fit();
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
        let compact = v[0] == COMPACT;
        let mut d = Dec(&v[1..]);
        let (n, l) = if compact { (u32::try_from(d.varint()?).ok()?, u32::try_from(d.varint()?).ok()?) } else { (d.u32()?, d.u32()?) };
        if n != node || max_versions() > l as usize {
            return None;
        }
        h.limit = l;
        if compact {
            decode_heap_changes(&mut d, &mut h)?;
        } else {
            decode_heap(&mut d, full, &mut h)?;
        }
        let parts = if compact { d.varint()? } else { d.u32()? as u64 };
        for _ in 0..parts {
            let i = if compact { u32::try_from(d.varint()?).ok()? } else { d.u32()? };
            let b = btest::BtEst::decode(&mut d, btrees.remove(&i))?;
            btrees.insert(i, b);
        }
        Some(())
    };
    apply(&v, true, &mut btrees)?;
    for r in it {
        let (k, v) = r.ok()?;
        let seq = u64::from_be_bytes(k.get(start.len()..)?.try_into().ok()?);
        if !matches!(v.first(), Some(&CHANGES | &COMPACT)) || seq != saved.seq? + 1 {
            return None;
        }
        apply(&v, false, &mut btrees)?;
        saved.seq = Some(seq);
        saved.changes_bytes += v.len();
    }
    h.log = Log { moved: Vec::new(), runs_saved: h.runs.len(), reset: false };
    h.sent = Some(Sent::of(&h));
    let btrees = btrees.into_iter().filter(|(i, _)| ixs.contains(i)).collect();
    Some((h, btrees, saved))
}
