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

/// The fan of runs that are linked at commit (`take_linkable`): merging them would write
/// again what linking saves writing, so more of them are read side by side first.
const LINKED_MERGE_FAN: usize = 16;

pub(crate) struct Runs {
    dir: PathBuf,
    next_id: u64,
    compress: bool,
    cache: Arc<BlockCache>,
    /// Oldest first, each with its tier and whether it can be linked into the store as it
    /// is at commit (`take_linkable`): every key in it written once in the whole
    /// transaction, by a write of a key nothing held before (`Txn::put_fresh`).
    files: Vec<(Arc<SstReader>, u32, bool)>,
    /// The files newest first, as the store reads its sources.
    view: Arc<View>,
    /// The transaction's range deletions (`Txn::delete_range`), at their journal places:
    /// versions before one in its range are gone as of it.
    rdels: Vec<crate::storage::rdel::RangeDel>,
    /// Each file's keys (by file id) as ranges, one per index or table they are in: what
    /// `invalidate` and `take_linkable` test writes against (a run's whole range spans every
    /// table in between).
    segs: std::collections::HashMap<u64, Vec<(Vec<u8>, Vec<u8>)>>,
}

/// A fresh run being written (`Runs::fresh_run`). Dropped before `Runs::add_fresh_run`
/// (a push failed), its file goes.
pub(crate) struct FreshRun {
    w: Option<SstWriter>,
    cid: u32,
    val: Vec<u8>,
    /// The last key pushed, with its version: each key made in one buffer, no allocation
    /// per key.
    key: Vec<u8>,
    version: [u8; 8],
    /// The keys as ranges, one per index or table (`Runs::segs`), and the current one's
    /// first key (its last is the one in `key`).
    segs: Vec<(Vec<u8>, Vec<u8>)>,
    first: Option<Vec<u8>>,
}

impl FreshRun {
    /// The next write, in key order, its value in two parts put end to end.
    pub fn push(&mut self, k: &[u8], [v1, v2]: [&[u8]; 2]) -> Result<()> {
        self.val.clear();
        self.val.extend_from_slice(v1);
        self.val.extend_from_slice(v2);
        // (The table's and index's ids: 8 bytes. A key of other ids ends the range at the key
        // before, still in `key`.)
        match &self.first {
            Some(f) if f.get(..8) == k.get(..8) => {}
            Some(_) => {
                let last = self.key[..self.key.len() - 8].to_vec();
                let first = self.first.replace(k.to_vec()).expect("a range");
                self.segs.push((first, last));
            }
            None => self.first = Some(k.to_vec()),
        }
        self.key.clear();
        self.key.extend_from_slice(k);
        self.key.extend_from_slice(&self.version);
        self.w.as_mut().expect("not added yet").add(&self.key, Some(&self.val))
    }
}

impl Drop for FreshRun {
    fn drop(&mut self) {
        if let Some(w) = self.w.take() {
            w.abandon();
        }
    }
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

pub(super) fn decode_entry(b: Bytes) -> (Option<Bytes>, Option<u32>) {
    let tag = b[0];
    let (cid, at) = if tag & 2 != 0 {
        (Some(u32::from_be_bytes([b[1], b[2], b[3], b[4]])), 5)
    } else {
        (None, 1)
    };
    ((tag & 1 != 0).then(|| b.slice(at..)), cid)
}

impl Runs {
    pub fn new(dir: PathBuf, compress: bool) -> Runs {
        Runs {
            dir,
            next_id: 1,
            compress,
            cache: BlockCache::new(8 << 20),
            files: Vec::new(),
            view: View::of_files(Vec::new()),
            rdels: Vec::new(),
            segs: Default::default(),
        }
    }

    /// The transaction's range deletions now (`rdels`).
    pub fn set_rdels(&mut self, rdels: &[crate::storage::rdel::RangeDel]) {
        if self.rdels != rdels {
            self.rdels = rdels.to_vec();
            self.reset_view();
        }
    }

    /// About how many versions the runs hold.
    pub fn entries(&self) -> usize {
        self.files.iter().map(|f| f.0.meta.entries as usize).sum()
    }

    /// Write a run of `versions`: in key order, a key's newest first. `linkable`: each
    /// key's one version anywhere in the transaction (`files`).
    pub fn write(&mut self, versions: &mut dyn Iterator<Item = (Vec<u8>, Ent)>, linkable: bool) -> Result<()> {
        let Some(f) = self.write_file(&mut versions.map(Ok))? else {
            return Ok(());
        };
        self.add_file(f, linkable)
    }

    /// `write` of fresh writes (`Txn::put_fresh`) given as slices in key order, each key
    /// once, all of command `cid` at journal place `ts`: a linkable run, encoded straight
    /// from the slices (a bulk load's sink, `Txn::flush_sink`; a parallel index build's
    /// entries, `index`: one index's, `SstWriter::index_run`). Each value comes in two parts,
    /// put end to end.
    pub fn write_fresh<'a>(&mut self, items: &mut dyn Iterator<Item = (&'a [u8], [&'a [u8]; 2])>, cid: u32, ts: u64, index: bool) -> Result<()> {
        let mut run = self.fresh_run(cid, ts, index)?;
        for (k, v) in items {
            run.push(k, v)?;
        }
        self.add_fresh_run(run)
    }

    /// `write_fresh`'s run, its writes given one at a time (`FreshRun::push`), then added
    /// (`add_fresh_run`): for writes that aren't all in memory at once (a parallel index
    /// build's spilled runs merged).
    pub fn fresh_run(&mut self, cid: u32, ts: u64, index: bool) -> Result<FreshRun> {
        crate::storage::durable::create_dir_all(&self.dir)?;
        let id = self.next_id;
        self.next_id += 1;
        // (The store's compression policy: a linked run may go straight to L1. Format 2, the
        // values plain: the run's reads here put its tag back, `SstReader::retagged`.)
        let mut w = SstWriter::create_run(&self.dir, id, self.compress)?;
        if index {
            w.index_run();
        }
        Ok(FreshRun { w: Some(w), cid, val: Vec::new(), key: Vec::new(), version: (!ts).to_be_bytes(), segs: Vec::new(), first: None })
    }

    pub fn add_fresh_run(&mut self, mut run: FreshRun) -> Result<()> {
        let w = run.w.take().expect("a run is added once");
        let mut segs = std::mem::take(&mut run.segs);
        if let Some(first) = run.first.take() {
            segs.push((first, run.key[..run.key.len() - 8].to_vec()));
        }
        if w.entries() == 0 {
            w.abandon();
            return Ok(());
        }
        let meta = w.finish()?;
        let r = SstReader::open(&self.dir, meta, self.cache.clone())?.retagged(run.cid);
        r.mark_obsolete();
        self.segs.insert(r.meta.id, segs);
        self.add_file(Arc::new(r), true)
    }

    /// `write_fresh` of deletions: `keys` in order, each once, all of command `cid` at
    /// journal place `ts` (a statement's deferred index entry and row deletes, `Txn::
    /// put_entry_deletes`): a linkable run of tombstones (format 2, not compressed: a
    /// compaction drops them soon), read here as deletions of `cid` (`SstReader::retagged`).
    pub fn write_deletes(&mut self, keys: &mut dyn Iterator<Item = Vec<u8>>, cid: u32, ts: u64) -> Result<()> {
        crate::storage::durable::create_dir_all(&self.dir)?;
        let id = self.next_id;
        self.next_id += 1;
        let mut w = SstWriter::create_run(&self.dir, id, false)?;
        let mut segs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        // (Each key with its version made in one buffer: no allocation per key. A key of 16 to
        // 32 bytes, a row's or an index entry's, by fixed copies: not a call to memcpy each.)
        let (mut key, version) = (Vec::new(), (!ts).to_be_bytes());
        let mut short = [0u8; 40];
        for k in keys {
            let n = k.len();
            let key: &[u8] = if (16..=32).contains(&n) {
                short[..16].copy_from_slice(&k[..16]);
                short[n - 16..n].copy_from_slice(&k[n - 16..]);
                short[n..n + 8].copy_from_slice(&version);
                &short[..n + 8]
            } else {
                key.clear();
                key.extend_from_slice(&k);
                key.extend_from_slice(&version);
                &key
            };
            if let Err(err) = w.add(key, None) {
                w.abandon();
                return Err(err);
            }
            // (The table's and index's ids: 8 bytes.)
            match segs.last_mut() {
                Some(g) if g.0.get(..8) == k.get(..8) => g.1 = k,
                _ => segs.push((k.clone(), k)),
            }
        }
        if w.entries() == 0 {
            w.abandon();
            return Ok(());
        }
        let meta = w.finish()?;
        let r = SstReader::open(&self.dir, meta, self.cache.clone())?.retagged(cid);
        r.mark_obsolete();
        self.segs.insert(r.meta.id, segs);
        self.add_file(Arc::new(r), true)
    }

    /// A new run of level 0 joins the others, merging the newest runs of a level once
    /// enough share it.
    fn add_file(&mut self, f: Arc<SstReader>, linkable: bool) -> Result<()> {
        self.files.push((f, 0, linkable));
        loop {
            let n = self.files.len();
            let level = self.files[n - 1].1;
            let fan = if self.files[n - 1].2 { LINKED_MERGE_FAN } else { MERGE_FAN };
            if n < fan || self.files[n - fan..].iter().any(|f| f.1 != level || f.2 != self.files[n - 1].2) {
                break;
            }
            let tail: Vec<RawIter> = self.files[n - fan..]
                .iter()
                .map(|f| RawIter::new(f.0.clone(), |_| true))
                .collect();
            let linkable = self.files[n - fan..].iter().all(|f| f.2);
            let merged = self.write_file(&mut merge(tail))?;
            self.files.truncate(n - fan);
            self.files.extend(merged.map(|m| (m, level + 1, linkable)));
        }
        self.reset_view();
        Ok(())
    }

    fn write_file(
        &mut self,
        versions: &mut dyn Iterator<Item = Result<(Vec<u8>, Ent)>>,
    ) -> Result<Option<Arc<SstReader>>> {
        crate::storage::durable::create_dir_all(&self.dir)?;
        let id = self.next_id;
        self.next_id += 1;
        let mut w = SstWriter::create_temp(&self.dir, id)?;
        let mut val = Vec::new();
        let mut segs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
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
            // (The table's and index's ids: 8 bytes.)
            match segs.last_mut() {
                Some(g) if g.0.get(..8) == k.get(..8) => g.1 = k,
                _ => segs.push((k.clone(), k)),
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
        self.segs.insert(r.meta.id, segs);
        Ok(Some(Arc::new(r)))
    }

    fn reset_view(&mut self) {
        self.view = View::of_files_with(self.files.iter().rev().map(|f| f.0.clone()).collect(), self.rdels.clone());
    }

    /// Drop every version past `mark` (ROLLBACK TO a savepoint taken before a spill): runs
    /// wholly past it go, one straddling it is rewritten without them.
    pub fn truncate(&mut self, mark: u64) -> Result<()> {
        let mut kept = Vec::with_capacity(self.files.len());
        for (f, level, linkable) in self.files.clone() {
            if f.meta.min_ts > mark {
                continue;
            }
            if f.meta.max_ts <= mark {
                kept.push((f, level, linkable));
                continue;
            }
            let mut it = RawIter::new(f, move |ts| ts <= mark);
            // (On an error the runs stay as they were.)
            let rest = self.write_file(&mut std::iter::from_fn(|| it.next_entry().transpose()))?;
            kept.extend(rest.map(|r| (r, level, linkable)));
        }
        self.files = kept;
        self.reset_view();
        Ok(())
    }

    /// A write of `key` that isn't fresh: no run holding a key around it is linked.
    pub fn invalidate(&mut self, key: &[u8]) {
        self.invalidate_range(key, &[]);
    }

    /// `invalidate` of every key in [start, end] (an empty `end`: `start` alone).
    pub fn invalidate_range(&mut self, start: &[u8], end: &[u8]) {
        let end = if end.is_empty() { start } else { end };
        for f in self.files.iter_mut().filter(|f| f.2) {
            if self.segs.get(&f.0.meta.id).is_none_or(|g| g.iter().any(|(a, b)| a.as_slice() <= end && start <= b.as_slice())) {
                tracing::debug!(key = ?start, "a run can't be linked: its key range written again");
                f.2 = false;
            }
        }
    }

    /// The runs linked into the store as they are at commit (`files`), taken out of these.
    /// (As a check, none whose keys' range another run or the buffer, `held`, writes in.)
    pub fn take_linkable(&mut self, held: impl Fn(&[u8], &[u8]) -> bool) -> (Vec<Arc<SstReader>>, Vec<u32>) {
        let none = Vec::new();
        let segs = |f: &SstReader| self.segs.get(&f.meta.id).unwrap_or(&none);
        let others: Vec<&(Vec<u8>, Vec<u8>)> = self.files.iter().filter(|f| !f.2).flat_map(|f| segs(&f.0)).collect();
        let keep: Vec<bool> = self
            .files
            .iter()
            .map(|f| {
                f.2 && !segs(&f.0).is_empty()
                    && segs(&f.0).iter().all(|(a, b)| !held(a, b) && !others.iter().any(|(x, y)| x <= b && a <= y))
            })
            .collect();
        for (f, k) in self.files.iter_mut().zip(keep) {
            f.2 = k;
        }
        let (linked, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.files).into_iter().partition(|f| f.2);
        tracing::debug!(linked = linked.len(), merged = kept.len(), "runs at commit");
        self.files = kept;
        self.reset_view();
        // (The tables their keys are in: what `watch` drops structures of.)
        let mut tables: Vec<u32> = linked
            .iter()
            .flat_map(|f| self.segs.get(&f.0.meta.id).into_iter().flatten())
            .filter_map(|(k, _)| k.get(..4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]])))
            .collect();
        tables.sort_unstable();
        tables.dedup();
        (linked.into_iter().map(|f| f.0).collect(), tables)
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
            .map(decode_entry))
    }

    /// The runs' sources (values as `decode_entry` reads them), for reading later
    /// (`toast::Source`): None while there are none.
    pub fn view(&self) -> Option<Arc<View>> {
        (!self.files.is_empty()).then(|| self.view.clone())
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
        let _ = crate::storage::durable::remove_dir_all(&self.dir);
    }
}

pub(crate) struct RunScan(Scan);

impl Iterator for RunScan {
    type Item = Result<(Vec<u8>, Option<Bytes>)>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.0.next()?.map(|(k, v)| (k, decode_entry(v).0)))
    }
}

pub(crate) struct RunRevScan(RevScan);

impl Iterator for RunRevScan {
    type Item = Result<(Vec<u8>, Option<Bytes>)>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.0.next()?.map(|(k, v)| (k, decode_entry(v).0)))
    }
}

/// Every version of one run, in storage-key order, `keep` choosing by version ts.
struct RawIter {
    file: Arc<SstReader>,
    block: usize,
    pos: usize,
    cur: Option<crate::storage::sst::Expanded>,
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
                self.cur = Some(self.file.block_with(self.block, false)?.expand());
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
            let (v, cid) = decode_entry(b.value(i).expect("a run value"));
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
