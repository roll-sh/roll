//! The statistics file (PostgreSQL's `pg_stat/pgstat.stat`): the cumulative statistics written
//! at a clean shutdown and read back at the next start (pgstat_write_statsfile /
//! pgstat_read_statsfile).
//!
//! As in PostgreSQL 18.6: it is written once, at a clean shutdown (not at checkpoints), read
//! at the start that follows and removed once read; after a crash or an immediate shutdown it
//! isn't read and every statistic starts again from zero, the fixed-numbered ones with that
//! start as their reset time. A file that doesn't parse (torn, truncated, of another format or
//! of another shutdown than the one the cluster starts from) is discarded the same way, with
//! PostgreSQL's LOG line: it can never stop a start.
//!
//! The format: a header (magic, format version, the WAL position the shutdown recorded, which
//! the start compares with its own as PostgreSQL compares the redo LSN), then the cluster's
//! fixed-numbered statistics, each database's entries and the subscriptions' counters, all
//! little-endian, and a CRC-32 of everything before it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{DbEntry, FuncCounts, IoKey, Shared, TabEntry};

const MAGIC: &[u8; 8] = b"ROLLSTAT";
/// PGSTAT_FILE_FORMAT_ID: a file of another version is discarded.
const FORMAT: u32 = 1;

/// The file, in the data directory (PGSTAT_STAT_PERMANENT_FILENAME).
const NAME: &str = "pg_stat/pgstat.stat";

/// Where the file is in data directory `dir`.
pub fn path(dir: &Path) -> PathBuf {
    dir.join(NAME)
}

/// What the file holds.
#[derive(Debug, Default)]
pub struct Saved {
    /// When the statistics were last discarded (the fixed-numbered ones' reset time when not
    /// reset since).
    pub since: Option<i64>,
    pub resets: HashMap<String, i64>,
    pub checkpoints_requested: i64,
    pub checkpoints_performed: i64,
    pub wal_records: i64,
    pub wal_bytes: i64,
    pub io: Vec<(IoKey, i64)>,
    pub slru_zeroed: HashMap<String, i64>,
    /// Each database's statistics (only `tables`, `funcs`, `db` and `shared_db` are kept).
    pub dbs: Vec<(u32, Shared)>,
    /// pg_stat_subscription_stats: by subscription, its reset time and counters.
    pub subscriptions: Vec<(u32, Option<i64>, [i64; 9])>,
}

struct W(Vec<u8>);

impl W {
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    fn opt(&mut self, v: Option<i64>) {
        match v {
            Some(v) => {
                self.0.push(1);
                self.i64(v);
            }
            None => self.0.push(0),
        }
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
}

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_bits(self.u64()?))
    }
    fn opt(&mut self) -> Option<Option<i64>> {
        match self.take(1)?[0] {
            0 => Some(None),
            1 => Some(Some(self.i64()?)),
            _ => None,
        }
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).ok()
    }
    /// A count of entries that can't be more than the bytes left (each at least `min` bytes).
    fn count(&mut self, min: usize) -> Option<usize> {
        let n = self.u32()? as usize;
        (n.saturating_mul(min) <= self.0.len()).then_some(n)
    }
}

/// An entry's fields, each written and read in this order. The pattern names every field, so
/// one added to the struct and not here doesn't compile.
macro_rules! codec {
    ($t:ident { $($kind:ident $f:ident),* $(,)? }) => {
        impl $t {
            fn put(&self, w: &mut W) {
                let $t { $($f),* } = self;
                $( w.$kind(*$f); )*
            }
            fn get(r: &mut R<'_>) -> Option<$t> {
                Some($t { $($f: r.$kind()?),* })
            }
        }
    };
}

codec!(TabEntry {
    i64 numscans,
    opt lastscan,
    i64 tuples_returned,
    i64 tuples_fetched,
    i64 tuples_inserted,
    i64 tuples_updated,
    i64 tuples_deleted,
    i64 tuples_hot_updated,
    i64 tuples_newpage_updated,
    i64 live_tuples,
    i64 dead_tuples,
    i64 mod_since_analyze,
    i64 ins_since_vacuum,
    i64 blocks_fetched,
    i64 blocks_hit,
    opt last_vacuum_time,
    i64 vacuum_count,
    opt last_analyze_time,
    i64 analyze_count,
    f64 total_vacuum_time,
    f64 total_analyze_time,
    opt last_autovacuum_time,
    i64 autovacuum_count,
    f64 total_autovacuum_time,
    opt last_autoanalyze_time,
    i64 autoanalyze_count,
    f64 total_autoanalyze_time,
});

codec!(FuncCounts { i64 calls, f64 total_time, f64 self_time });

codec!(DbEntry {
    i64 xact_commit,
    i64 xact_rollback,
    i64 blocks_fetched,
    i64 blocks_hit,
    i64 tuples_returned,
    i64 tuples_fetched,
    i64 tuples_inserted,
    i64 tuples_updated,
    i64 tuples_deleted,
    i64 sessions,
    i64 parallel_workers_to_launch,
    i64 parallel_workers_launched,
    i64 temp_files,
    i64 temp_bytes,
    opt stat_reset,
});

/// The file's bytes for `s`, tagged with WAL position `lsn`.
pub fn encode(s: &Saved, lsn: u64) -> Vec<u8> {
    let mut w = W(Vec::with_capacity(4096));
    w.0.extend_from_slice(MAGIC);
    w.u32(FORMAT);
    w.u64(lsn);
    w.opt(s.since);
    let mut resets: Vec<_> = s.resets.iter().collect();
    resets.sort();
    w.u32(resets.len() as u32);
    for (k, v) in resets {
        w.str(k);
        w.i64(*v);
    }
    for v in [s.checkpoints_requested, s.checkpoints_performed, s.wal_records, s.wal_bytes] {
        w.i64(v);
    }
    w.u32(s.io.len() as u32);
    for ((a, b, c, d), n) in &s.io {
        for x in [a, b, c, d] {
            w.str(x);
        }
        w.i64(*n);
    }
    w.u32(s.slru_zeroed.len() as u32);
    for (k, v) in &s.slru_zeroed {
        w.str(k);
        w.i64(*v);
    }
    w.u32(s.dbs.len() as u32);
    for (oid, d) in &s.dbs {
        w.u32(*oid);
        d.db.put(&mut w);
        d.shared_db.put(&mut w);
        w.u32(d.tables.len() as u32);
        for (rel, e) in &d.tables {
            w.u32(*rel);
            e.put(&mut w);
        }
        w.u32(d.funcs.len() as u32);
        for (f, e) in &d.funcs {
            w.u32(*f);
            e.put(&mut w);
        }
    }
    w.u32(s.subscriptions.len() as u32);
    for (sub, reset, counts) in &s.subscriptions {
        w.u32(*sub);
        w.opt(*reset);
        for c in counts {
            w.i64(*c);
        }
    }
    let crc = crc32fast::hash(&w.0);
    w.u32(crc);
    w.0
}

/// The statistics in `bytes`, when they are a whole file of this format written at the
/// shutdown that ended at WAL position `lsn`.
pub fn decode(bytes: &[u8], lsn: u64) -> Option<Saved> {
    let (body, crc) = bytes.split_at_checked(bytes.len().checked_sub(4)?)?;
    if crc32fast::hash(body) != u32::from_le_bytes(crc.try_into().ok()?) {
        return None;
    }
    let mut r = R(body);
    if r.take(8)? != MAGIC || r.u32()? != FORMAT || r.u64()? != lsn {
        return None;
    }
    let mut s = Saved { since: r.opt()?, ..Saved::default() };
    for _ in 0..r.count(12)? {
        let k = r.str()?;
        s.resets.insert(k, r.i64()?);
    }
    s.checkpoints_requested = r.i64()?;
    s.checkpoints_performed = r.i64()?;
    s.wal_records = r.i64()?;
    s.wal_bytes = r.i64()?;
    for _ in 0..r.count(24)? {
        let key = (intern(&r.str()?), intern(&r.str()?), intern(&r.str()?), intern(&r.str()?));
        s.io.push((key, r.i64()?));
    }
    for _ in 0..r.count(12)? {
        let k = r.str()?;
        s.slru_zeroed.insert(k, r.i64()?);
    }
    for _ in 0..r.count(8)? {
        let oid = r.u32()?;
        let mut d = Shared { db: DbEntry::get(&mut r)?, shared_db: DbEntry::get(&mut r)?, ..Shared::default() };
        let n = r.count(8)?;
        d.tables.reserve(n);
        for _ in 0..n {
            let rel = r.u32()?;
            d.tables.insert(rel, TabEntry::get(&mut r)?);
        }
        for _ in 0..r.count(8)? {
            let f = r.u32()?;
            d.funcs.insert(f, FuncCounts::get(&mut r)?);
        }
        s.dbs.push((oid, d));
    }
    for _ in 0..r.count(5)? {
        let sub = r.u32()?;
        let reset = r.opt()?;
        let mut counts = [0; 9];
        for c in &mut counts {
            *c = r.i64()?;
        }
        s.subscriptions.push((sub, reset, counts));
    }
    r.0.is_empty().then_some(s)
}

/// pg_stat_io's names as `'static` strings (`IoKey`'s), each kept once per process.
fn intern(s: &str) -> &'static str {
    static NAMES: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashSet<&'static str>>> =
        std::sync::LazyLock::new(Default::default);
    let mut n = NAMES.lock();
    match n.get(s) {
        Some(v) => v,
        None => {
            let v: &'static str = Box::leak(s.to_string().into_boxed_str());
            n.insert(v);
            v
        }
    }
}

/// Write `s` durably (`sync`: with the fsyncs) to the file in data directory `dir`. Errors
/// are the caller's to log: a shutdown goes on without the file (the next start then
/// begins from zero, as PostgreSQL's does when it can't write it).
pub fn write(dir: &Path, s: &Saved, lsn: u64, sync: bool) -> std::io::Result<()> {
    let p = path(dir);
    if let Some(parent) = p.parent() {
        crate::storage::durable::create_dir_all_synced(parent)?;
    }
    crate::storage::durable::replace(&p, &encode(s, lsn), sync)
}

/// Read and remove the file in data directory `dir` (pgstat_restore_stats; `crashed`:
/// pgstat_discard_stats, the file only removed). None when there is none or it is
/// discarded, and then the startup process's LOG line when PostgreSQL's writes one (a file
/// that can't be read or doesn't parse).
pub fn take(dir: &Path, lsn: u64, crashed: bool) -> (Option<Saved>, Option<String>) {
    let p = path(dir);
    let mut tmp = p.clone().into_os_string();
    tmp.push(".tmp");
    let _ = std::fs::remove_file(&tmp);
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || crashed => return (None, None),
        Err(e) => {
            let _ = std::fs::remove_file(&p);
            // (%m: strerror's text, std's without its " (os error N)".)
            let m = e.to_string();
            let m = m.split(" (os error").next().unwrap_or_default();
            return (None, Some(format!("could not open statistics file \"{NAME}\": {m}")));
        }
    };
    let _ = std::fs::remove_file(&p);
    if crashed {
        return (None, None);
    }
    match decode(&bytes, lsn) {
        Some(s) => (Some(s), None),
        None => (None, Some(format!("corrupted statistics file \"{NAME}\""))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Saved {
        let mut d = Shared::default();
        d.db.xact_commit = 7;
        d.db.stat_reset = Some(-5);
        d.tables.insert(16384, TabEntry { tuples_inserted: 1000, dead_tuples: 99, last_vacuum_time: Some(42), total_vacuum_time: 1.5, ..TabEntry::default() });
        d.funcs.insert(16390, FuncCounts { calls: 3, total_time: 0.25, self_time: 0.125 });
        Saved {
            since: Some(11),
            resets: [("wal".to_string(), 12)].into(),
            checkpoints_requested: 2,
            wal_records: 9,
            io: vec![(("client backend", "relation", "normal", "reads"), 4)],
            slru_zeroed: [("notify".to_string(), 1)].into(),
            dbs: vec![(5, d)],
            subscriptions: vec![(16400, None, [1, 0, 0, 0, 0, 0, 0, 0, 2])],
            ..Saved::default()
        }
    }

    #[test]
    fn a_file_reads_back_whole_and_nothing_else_does() {
        let b = encode(&sample(), 77);
        let s = decode(&b, 77).expect("reads back");
        assert_eq!(s.since, Some(11));
        assert_eq!(s.checkpoints_requested, 2);
        assert_eq!(s.io[0], (("client backend", "relation", "normal", "reads"), 4));
        let t = &s.dbs[0].1.tables[&16384];
        assert_eq!((t.tuples_inserted, t.dead_tuples, t.last_vacuum_time, t.total_vacuum_time), (1000, 99, Some(42), 1.5));
        assert_eq!(s.dbs[0].1.db.stat_reset, Some(-5));
        assert_eq!(s.dbs[0].1.funcs[&16390].calls, 3);
        assert_eq!(s.subscriptions[0].2[8], 2);
        // Another shutdown's file, every prefix (a torn write), a flipped byte: discarded.
        assert!(decode(&b, 78).is_none());
        for n in 0..b.len() {
            assert!(decode(&b[..n], 77).is_none(), "prefix {n}");
        }
        let mut bad = b.clone();
        bad[30] ^= 1;
        assert!(decode(&bad, 77).is_none());
    }
}
