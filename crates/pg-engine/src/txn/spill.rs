//! A transaction's spilled writes. Once its write buffer passes
//! `StoreOptions::txn_spill_bytes`, the buffer goes to a sorted file of its own (a run) and
//! starts again empty, so a transaction of any size runs in bounded memory. (Postgres writes
//! a transaction's tuples into the heap as it goes; the engine's stay private until commit,
//! so they go to private files.)
//!
//! A run holds every version the buffer had, each stamped with its place in the
//! transaction's write journal (`Ent::ts`). What a statement snapshot or a savepoint needs of
//! writes made before a spill is then a read of the runs at that place, under the store's
//! own version rules (`View::of_files`). Runs are SST files keyed `key | !ts`, never fsynced;
//! the store deletes what a crash leaves (`Store::spill_dir`).

use crate::error::Result;
use crate::storage::cache::BlockCache;
use crate::storage::sst::{SstReader, SstWriter, user_key};
use crate::storage::{RevScan, Scan, View, mvcc_key};
use bytes::Bytes;
use std::path::PathBuf;
use std::sync::Arc;

/// A buffered write: the value (None: a delete), the command id of the write (None: one
/// that records none), and the journal place it is visible from: a snapshot whose mark is at
/// or past `ts` sees it.
#[derive(Clone, Debug)]
pub(crate) struct Ent {
    pub v: Option<Bytes>,
    pub cid: Option<u32>,
    pub ts: u64,
}

/// A mark past every write: the newest version of each key.
pub(crate) const LATEST: u64 = u64::MAX >> 1;

/// Runs merge in tiers: once the newest `MERGE_FAN` share a level, they become one run of
/// the next (a transaction of n spills keeps O(log n) runs, each version rewritten
/// O(log n) times; a scan reads every run).
const MERGE_FAN: usize = 4;

pub(crate) struct Runs {
    dir: PathBuf,
    next_id: u64,
    cache: Arc<BlockCache>,
    /// Oldest first, each with its tier.
    files: Vec<(Arc<SstReader>, u32)>,
    /// The files newest first, as the store reads its sources.
    view: Arc<View>,
}

/// A run value: a tag (bit 0: a value follows, bit 1: a command id follows), the command
/// id (4 bytes), the value.
fn encode(out: &mut Vec<u8>, e: &Ent) {
    out.clear();
    out.push(e.v.is_some() as u8 | (e.cid.is_some() as u8) << 1);
    if let Some(c) = e.cid {
        out.extend_from_slice(&c.to_be_bytes());
    }
    if let Some(v) = &e.v {
        out.extend_from_slice(v);
    }
}

fn decode(b: Bytes) -> (Option<Bytes>, Option<u32>) {
    let tag = b[0];
    let (cid, at) = if tag & 2 != 0 {
        (Some(u32::from_be_bytes([b[1], b[2], b[3], b[4]])), 5)
    } else {
        (None, 1)
    };
    ((tag & 1 != 0).then(|| b.slice(at..)), cid)
}

impl Runs {
    pub fn new(dir: PathBuf) -> Runs {
        Runs {
            dir,
            next_id: 1,
            cache: BlockCache::new(8 << 20),
            files: Vec::new(),
            view: View::of_files(Vec::new()),
        }
    }

    /// About how many versions the runs hold.
    pub fn entries(&self) -> usize {
        self.files.iter().map(|f| f.0.meta.entries as usize).sum()
    }

    /// Write a run of `versions`: in key order, a key's newest first.
    pub fn write(&mut self, versions: &mut dyn Iterator<Item = (Vec<u8>, Ent)>) -> Result<()> {
        let Some(f) = self.write_file(&mut versions.map(Ok))? else {
            return Ok(());
        };
        self.files.push((f, 0));
        loop {
            let n = self.files.len();
            let level = self.files[n - 1].1;
            if n < MERGE_FAN || self.files[n - MERGE_FAN..].iter().any(|f| f.1 != level) {
                break;
            }
            let tail: Vec<RawIter> = self.files[n - MERGE_FAN..]
                .iter()
                .map(|f| RawIter::new(f.0.clone(), |_| true))
                .collect();
            let merged = self.write_file(&mut merge(tail))?;
            self.files.truncate(n - MERGE_FAN);
            self.files.extend(merged.map(|m| (m, level + 1)));
        }
        self.reset_view();
        Ok(())
    }

    fn write_file(
        &mut self,
        versions: &mut dyn Iterator<Item = Result<(Vec<u8>, Ent)>>,
    ) -> Result<Option<Arc<SstReader>>> {
        std::fs::create_dir_all(&self.dir)?;
        let id = self.next_id;
        self.next_id += 1;
        let mut w = SstWriter::create_temp(&self.dir, id)?;
        let mut val = Vec::new();
        for item in versions {
            let (k, e) = match item {
                Ok(x) => x,
                Err(err) => {
                    w.abandon();
                    return Err(err);
                }
            };
            encode(&mut val, &e);
            if let Err(err) = w.add(&mvcc_key(&k, e.ts), Some(&val)) {
                w.abandon();
                return Err(err);
            }
        }
        if w.entries() == 0 {
            w.abandon();
            return Ok(None);
        }
        let meta = w.finish()?;
        let r = SstReader::open(&self.dir, meta, self.cache.clone())?;
        // (Deleted when the last reader drops it: the run is replaced or the transaction ends.)
        r.mark_obsolete();
        Ok(Some(Arc::new(r)))
    }

    fn reset_view(&mut self) {
        self.view = View::of_files(self.files.iter().rev().map(|f| f.0.clone()).collect());
    }

    /// Drop every version past `mark` (ROLLBACK TO a savepoint taken before a spill): runs
    /// wholly past it go, one straddling it is rewritten without them.
    pub fn truncate(&mut self, mark: u64) -> Result<()> {
        let mut kept = Vec::with_capacity(self.files.len());
        for (f, level) in self.files.clone() {
            if f.meta.min_ts > mark {
                continue;
            }
            if f.meta.max_ts <= mark {
                kept.push((f, level));
                continue;
            }
            let mut it = RawIter::new(f, move |ts| ts <= mark);
            // (On an error the runs stay as they were.)
            let rest = self.write_file(&mut std::iter::from_fn(|| it.next_entry().transpose()))?;
            kept.extend(rest.map(|r| (r, level)));
        }
        self.files = kept;
        self.reset_view();
        Ok(())
    }

    /// The newest version of `key` at `mark`: (value, command id); None: not written.
    pub fn get(&self, key: &[u8], mark: u64) -> Result<Option<(Option<Bytes>, Option<u32>)>> {
        if self.files.is_empty() {
            return Ok(None);
        }
        Ok(self
            .view
            .get_version(key, mark)?
            .and_then(|(_, v)| v)
            .map(decode))
    }

    /// Keys in [start, end) as of `mark`, ascending: (key, value; None: deleted).
    pub fn scan(&self, start: &[u8], end: &[u8], mark: u64) -> RunScan {
        RunScan(self.view.scan(start, end, mark))
    }

    pub fn scan_rev(&self, start: &[u8], end: &[u8], mark: u64) -> RunRevScan {
        RunRevScan(self.view.scan_rev(start, end, mark))
    }
}

impl Drop for Runs {
    fn drop(&mut self) {
        self.files.clear();
        self.view = View::of_files(Vec::new());
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) struct RunScan(Scan);

impl Iterator for RunScan {
    type Item = Result<(Vec<u8>, Option<Bytes>)>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.0.next()?.map(|(k, v)| (k, decode(v).0)))
    }
}

pub(crate) struct RunRevScan(RevScan);

impl Iterator for RunRevScan {
    type Item = Result<(Vec<u8>, Option<Bytes>)>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.0.next()?.map(|(k, v)| (k, decode(v).0)))
    }
}

/// Every version of one run, in storage-key order, `keep` choosing by version ts.
struct RawIter {
    file: Arc<SstReader>,
    block: usize,
    pos: usize,
    cur: Option<Arc<crate::storage::sst::Block>>,
    keep: Box<dyn Fn(u64) -> bool>,
}

impl RawIter {
    fn new(file: Arc<SstReader>, keep: impl Fn(u64) -> bool + 'static) -> RawIter {
        RawIter {
            file,
            block: 0,
            pos: 0,
            cur: None,
            keep: Box::new(keep),
        }
    }

    fn next_entry(&mut self) -> Result<Option<(Vec<u8>, Ent)>> {
        loop {
            if self.cur.is_none() {
                if self.block >= self.file.block_count() {
                    return Ok(None);
                }
                self.cur = Some(self.file.block_with(self.block, false)?);
                self.pos = 0;
            }
            let b = self.cur.as_ref().unwrap();
            if self.pos >= b.len() {
                self.cur = None;
                self.block += 1;
                continue;
            }
            let i = self.pos;
            self.pos += 1;
            let k = b.key(i);
            let ts = crate::storage::sst::key_ts(k);
            if !(self.keep)(ts) {
                continue;
            }
            let (v, cid) = decode(b.value(i).expect("a run value"));
            return Ok(Some((user_key(k).to_vec(), Ent { v, cid, ts })));
        }
    }
}

/// The versions of several runs merged in storage-key order (key ascending, newest first).
fn merge(mut runs: Vec<RawIter>) -> impl Iterator<Item = Result<(Vec<u8>, Ent)>> {
    let mut heads: Vec<Option<(Vec<u8>, Ent)>> = Vec::new();
    let mut started = false;
    std::iter::from_fn(move || {
        if !started {
            started = true;
            for r in &mut runs {
                match r.next_entry() {
                    Ok(h) => heads.push(h),
                    Err(e) => return Some(Err(e)),
                }
            }
        }
        let i = heads
            .iter()
            .enumerate()
            .filter_map(|(i, h)| h.as_ref().map(|h| (i, h)))
            .min_by(|(_, a), (_, b)| a.0.cmp(&b.0).then(b.1.ts.cmp(&a.1.ts)))?
            .0;
        let out = heads[i].take();
        match runs[i].next_entry() {
            Ok(h) => heads[i] = h,
            Err(e) => return Some(Err(e)),
        }
        out.map(Ok)
    })
}
