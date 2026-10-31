//! Reading the change stream back (`log`'s records): commit records in position order, and
//! each transaction's change records with the positions they took.

use super::log::{self, Marker, Rec, RelDesc};
use crate::error::Result;
use crate::storage::Store;
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
    pub fn reader(db: &Arc<crate::txn::Database>) -> Reader {
        db.log.decoded.readers.fetch_add(1, Ordering::AcqRel);
        Reader(db.clone())
    }

    /// Change record `v` at `lsn`, decoded.
    fn change(&self, lsn: u64, v: &[u8]) -> Result<Change> {
        let rec = if v.first() == Some(&b'R') { Rec::Rel(self.rel(v)?) } else { log::decode_rec(v)? };
        Ok(Change { lsn, rewrite: v.first() == Some(&b'W'), rec })
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
        let ts = store.visible_ts();
        let (a, b) = log::changes_range(m.xid);
        let mut at = m.first_lsn;
        let mut out = Vec::new();
        let mut bytes = 0;
        for r in store.scan(&a, &b, ts) {
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
