//! Reading the change stream back (`log`'s records): commit records in position order, and
//! each transaction's change records with the positions they took.

use super::log::{self, Marker, Rec, RelDesc};
use crate::error::Result;
use crate::storage::Store;
use crate::storage::batch::RowBatch;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The commit records of positions `from..to`, in order, read at the store's visible state.
pub fn markers(store: &Arc<Store>, from: u64, to: u64) -> impl Iterator<Item = Result<Marker>> {
    let ts = store.visible_ts();
    store
        .scan(&log::commit_key(from.max(1)), &log::commit_key(to), ts)
        .map(|r| r.and_then(|(_, v)| Marker::decode(&v)))
}

/// A change record with its position.
#[derive(Clone)]
pub struct Change {
    pub lsn: u64,
    pub rec: Rec,
    /// A row a table rewrite wrote (`log::rewrite_insert`).
    pub rewrite: bool,
    /// The aborted subtransaction it belongs to (`log::rolled_back_to`; 0: one that
    /// commits with the transaction).
    pub sub: u64,
    /// The multi-insert batch a row belongs to (`log::multi_insert`; 0: none).
    pub batch: u32,
}

/// Transaction `m`'s change records, in order, with the positions they took after its
/// commit record's base (`Marker::first_lsn`). A relation description equal to its
/// relation's last one is that one (`Shared::rel`): parsed once, the same `Arc` for every
/// decoder (pgoutput keeps what it worked out for it).
pub fn changes<'a>(store: &Arc<Store>, shared: &'a Shared, m: &Marker) -> impl Iterator<Item = Result<Change>> + 'a {
    let ts = store.visible_ts();
    let (a, b) = log::changes_range(m.xid);
    let mut at = m.first_lsn;
    store.scan(&a, &b, ts).map(move |r| {
        let (_, v) = r?;
        let lsn = at;
        at += log::record_size(v.len());
        shared.change(lsn, &v)
    })
}

/// The change records of a run of transactions, read in one pass over their key range
/// (`Prefetched::read`): a scan of the store per transaction (a seek in every file) was
/// most of a walsender's store work. Bounded: the run's records, copied (`PREFETCH_BYTES`
/// of WAL at most, none of a transaction past `PREFETCH_ONE`).
#[derive(Default)]
pub struct Prefetched {
    rows: RowBatch,
    /// By xid: its records' places in `rows`.
    at: HashMap<u64, std::ops::Range<usize>>,
    /// A read's rows as the scan gives them.
    chunk: RowBatch,
}

/// How much WAL a run of transactions read in one pass takes at most, and a transaction
/// in it (a bigger one is read as it's decoded): hundreds of small transactions a pass.
pub const PREFETCH_BYTES: u64 = 256 << 10;
pub const PREFETCH_ONE: u64 = 32 << 10;

impl Prefetched {
    /// The records of transactions `xids` (sorted, distinct), read at the store's visible
    /// state, in place of what it held (its buffers kept: a walsender reads a run after
    /// another).
    pub fn read(&mut self, store: &Arc<Store>, xids: &[u64]) -> Result<()> {
        self.clear();
        let (Some(&lo), Some(&hi)) = (xids.first(), xids.last()) else { return Ok(()) };
        let (a, _) = log::changes_range(lo);
        let (_, b) = log::changes_range(hi);
        // (One pass, each block once: not kept in the block cache, as PostgreSQL's walsender
        // reads the WAL through the kernel's page cache rather than its buffers.)
        crate::storage::read_once(|| self.read_in(store, &a, &b, xids))
    }

    /// Empty (its buffers kept).
    pub fn clear(&mut self) {
        self.rows.clear();
        self.chunk.clear();
        self.at.clear();
    }

    fn read_in(&mut self, store: &Arc<Store>, a: &[u8], b: &[u8], xids: &[u64]) -> Result<()> {
        let mut scan = store.scan(a, b, store.visible_ts());
        let mut want = xids.iter().copied().peekable();
        loop {
            self.chunk.clear();
            scan.fill_batch(&mut self.chunk, 512)?;
            if self.chunk.is_empty() {
                break;
            }
            for i in 0..self.chunk.len() {
                let (k, v) = self.chunk.get(i);
                let x = log::xid_of(k);
                while want.next_if(|w| *w < x).is_some() {}
                if want.peek() != Some(&x) {
                    continue;
                }
                let n = self.rows.len();
                self.at.entry(x).or_insert(n..n).end = n + 1;
                self.rows.push_copy(k, v);
            }
        }
        Ok(())
    }

    pub fn has(&self, xid: u64) -> bool {
        self.at.contains_key(&xid)
    }

    /// Transaction `m`'s changes, as `changes` reads them (None: not read here).
    pub fn changes<'a>(&'a self, shared: &'a Shared, m: &Marker) -> Option<impl Iterator<Item = Result<Change>> + 'a> {
        let r = self.at.get(&m.xid)?.clone();
        let mut at = m.first_lsn;
        Some(r.map(move |i| self.rows.get(i).1).map(move |v| {
            let lsn = at;
            at += log::record_size(v.len());
            shared.change(lsn, v)
        }))
    }
}

/// What a database's decoders (walsenders, the SQL functions) share: relation descriptions,
/// and, while two or more decode, the transactions one of them decoded, so N walsenders on
/// one stream scan and decode each transaction about once. Bounded: at most `SHARED_BYTES`
/// of decoded changes per database (the oldest go first), none of a transaction past an
/// eighth of that (each decoder reads it on its own, as one alone does), and nothing kept
/// once fewer than two decoders are left.
#[derive(Default)]
pub struct Shared {
    /// Each relation's last description: (its record, decoded).
    rels: Mutex<HashMap<u32, (Vec<u8>, Arc<RelDesc>)>>,
    txns: Mutex<Txns>,
    readers: AtomicUsize,
}

#[derive(Default)]
struct Txns {
    /// By (first position, xid): the changes and their estimated size in memory.
    map: BTreeMap<(u64, u64), (Arc<[Change]>, usize)>,
    bytes: usize,
}

/// How much decoded change data a database's decoders keep in common (`Shared`).
pub const SHARED_BYTES: usize = 16 << 20;

/// An estimate of a decoded change's memory, from its record's length.
fn change_bytes(record: usize, rec: &Rec) -> usize {
    let values = match rec {
        Rec::Insert { new, .. } => new.len(),
        Rec::Update { old, new, .. } => new.len() + old.as_ref().map_or(0, |o| o.0.len()),
        Rec::Delete { old, .. } => old.as_ref().map_or(0, |o| o.0.len()),
        _ => 0,
    };
    std::mem::size_of::<Change>() + values * std::mem::size_of::<crate::types::Value>() + record
}

/// A decoder, counted in `Shared::readers` while it lives.
pub struct Reader(Arc<crate::txn::Database>);

impl Drop for Reader {
    fn drop(&mut self) {
        let s = &self.0.log.decoded;
        let left = s.readers.fetch_sub(1, Ordering::AcqRel) - 1;
        if left < 2 {
            let mut t = s.txns.lock();
            t.map.clear();
            t.bytes = 0;
        }
        if left == 0 {
            s.rels.lock().clear();
        }
    }
}

impl Shared {
    /// Whether two or more decoders run (each transaction decoded once for all: `txn`).
    pub fn shared(&self) -> bool {
        self.readers.load(Ordering::Acquire) >= 2
    }

    pub fn reader(db: &Arc<crate::txn::Database>) -> Reader {
        db.log.decoded.readers.fetch_add(1, Ordering::AcqRel);
        Reader(db.clone())
    }

    /// Change record `v` at `lsn`, decoded.
    pub fn change(&self, lsn: u64, v: &[u8]) -> Result<Change> {
        let (sub, v) = log::aborted_of(v)?;
        let rec = if v.first() == Some(&b'R') { Rec::Rel(self.rel(v)?) } else { log::decode_rec(v)? };
        Ok(Change { lsn, rewrite: v.first() == Some(&b'W'), rec, sub, batch: log::batch_of(v) })
    }

    /// Relation description record `b`, decoded: its relation's last one when equal.
    fn rel(&self, b: &[u8]) -> Result<Arc<RelDesc>> {
        let oid = u32::from_be_bytes(b.get(1..5).and_then(|x| x.try_into().ok()).unwrap_or_default());
        if let Some((raw, d)) = self.rels.lock().get(&oid)
            && raw.as_slice() == b
        {
            return Ok(d.clone());
        }
        let Rec::Rel(d) = log::decode_rec(b)? else {
            return Err(crate::error::internal("bad change stream record"));
        };
        self.rels.lock().insert(oid, (b.to_vec(), d.clone()));
        Ok(d)
    }

    /// Transaction `m`'s changes as another decoder decoded them, or decoded now and kept
    /// for the others; None while one decoder is alone or for a transaction too big to keep
    /// (read it through `changes` as it goes).
    pub fn txn(&self, store: &Arc<Store>, m: &Marker) -> Result<Option<Arc<[Change]>>> {
        if self.readers.load(Ordering::Acquire) < 2
            || m.end_lsn.saturating_sub(m.first_lsn) > (SHARED_BYTES / 8) as u64
        {
            return Ok(None);
        }
        let key = (m.first_lsn, m.xid);
        if let Some((c, _)) = self.txns.lock().map.get(&key) {
            return Ok(Some(c.clone()));
        }
        let mut out = Vec::new();
        let mut bytes = 0;
        let (a, b) = log::changes_range(m.xid);
        let mut at = m.first_lsn;
        for r in store.scan(&a, &b, store.visible_ts()) {
            let (_, v) = r?;
            let c = self.change(at, &v)?;
            at += log::record_size(v.len());
            bytes += change_bytes(v.len(), &c.rec);
            out.push(c);
        }
        let out: Arc<[Change]> = out.into();
        let mut t = self.txns.lock();
        if t.map.insert(key, (out.clone(), bytes)).is_none() {
            t.bytes += bytes;
        }
        while t.bytes > SHARED_BYTES {
            let Some((_, (_, n))) = t.map.pop_first() else { break };
            t.bytes -= n;
        }
        Ok(Some(out))
    }
}

/// The non-transactional message a `Message` record holds.
pub fn message_of(m: &Marker) -> Option<(String, Vec<u8>)> {
    match log::decode_rec(m.message.as_deref()?) {
        Ok(Rec::Message { prefix, content, .. }) => Some((prefix, content)),
        _ => None,
    }
}
