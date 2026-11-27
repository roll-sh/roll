//! The statistics pg_stat_statements keeps (`pgss_hash`): one entry per user, database,
//! statement (its query id) and nesting (top level or not), shared by the cluster's sessions.
//! There only when the library is preloaded (`shared_preload_libraries`).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

use crate::exec::pgstat::file::{R, W};

/// `pgssHashKey`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub userid: u32,
    pub dbid: u32,
    pub queryid: i64,
    pub toplevel: bool,
}

/// `pgssStoreKind`: planning or execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Plan = 0,
    Exec = 1,
}

/// `Counters`.
#[derive(Clone, Debug, Default)]
pub struct Counters {
    pub calls: [i64; 2],
    pub total_time: [f64; 2],
    pub min_time: [f64; 2],
    pub max_time: [f64; 2],
    pub mean_time: [f64; 2],
    pub sum_var_time: [f64; 2],
    pub rows: i64,
    /// The blocks a statement added to the table it wrote (shared: a permanent table's,
    /// local: a temporary one's), dirtied and written.
    pub shared_blks_written: i64,
    pub local_blks_written: i64,
    pub wal_records: i64,
    pub wal_fpi: i64,
    pub wal_bytes: u64,
    pub parallel_workers_to_launch: i64,
    pub parallel_workers_launched: i64,
    /// Temporary file blocks read and written (spills past work_mem).
    pub temp_blks_read: i64,
    pub temp_blks_written: i64,
    /// `usage`: what deallocation keeps the most used by.
    pub usage: f64,
}

impl Counters {
    /// `IS_STICKY`: made at parse analysis, never planned or run (not shown).
    pub fn sticky(&self) -> bool {
        self.calls[0] + self.calls[1] == 0
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// The (normalized) statement text, as first seen.
    pub query: Arc<str>,
    pub counters: Counters,
    pub stats_since: i64,
    pub minmax_stats_since: i64,
    /// Insertion order: the order entries are listed in.
    order: u64,
}

/// One execution's (or planning's) measures.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub millis: f64,
    pub rows: u64,
    pub shared_blks: i64,
    pub local_blks: i64,
    pub wal_records: i64,
    pub wal_bytes: u64,
    pub workers_to_launch: i64,
    pub workers_launched: i64,
    /// (temp_blks_read, temp_blks_written).
    pub temp_blks: (i64, i64),
}

#[derive(Default)]
struct Inner {
    entries: HashMap<Key, Entry>,
    next: u64,
    dealloc: i64,
    stats_reset: i64,
}

/// The shared statistics.
pub struct Store {
    inner: Mutex<Inner>,
    /// `pg_stat_statements.max` at server start.
    max: usize,
}

const USAGE_INIT: f64 = 1.0;
/// `USAGE_DEALLOC_PERCENT`.
const DEALLOC_PERCENT: usize = 5;

impl Store {
    /// What its entries take (and their query texts).
    pub fn memory_bytes(&self) -> usize {
        let g = self.inner.lock();
        g.entries.capacity() * (std::mem::size_of::<Key>() + std::mem::size_of::<Entry>() + 8)
            + g.entries.values().map(|e| e.query.len()).sum::<usize>()
    }

    pub fn new(now: i64, max: usize) -> Store {
        Store {
            inner: Mutex::new(Inner {
                stats_reset: now,
                ..Default::default()
            }),
            max: max.max(100),
        }
    }

    /// `pgss_store`: the entry for `key` (made with `text()` when new), counting `usage` of
    /// `kind` (None: only make it, as parse analysis does for a statement with constants).
    pub fn store(&self, key: Key, text: impl FnOnce() -> String, kind: Option<Kind>, usage: Usage, now: i64) {
        let mut g = self.inner.lock();
        if !g.entries.contains_key(&key) {
            if g.entries.len() >= self.max {
                Self::dealloc(&mut g, self.max);
            }
            let order = g.next;
            g.next += 1;
            let sticky = kind.is_none();
            let counters = Counters {
                usage: if sticky { median_usage(&g) } else { USAGE_INIT },
                ..Default::default()
            };
            g.entries.insert(
                key,
                Entry {
                    query: Arc::from(text()),
                    counters,
                    stats_since: now,
                    minmax_stats_since: now,
                    order,
                },
            );
        }
        let Some(kind) = kind else { return };
        let e = g.entries.get_mut(&key).expect("entry");
        let c = &mut e.counters;
        if c.sticky() {
            c.usage = USAGE_INIT;
        }
        let k = kind as usize;
        let t = usage.millis;
        c.calls[k] += 1;
        c.total_time[k] += t;
        if c.calls[k] == 1 {
            c.min_time[k] = t;
            c.max_time[k] = t;
            c.mean_time[k] = t;
        } else {
            let old = c.mean_time[k];
            c.mean_time[k] += (t - old) / c.calls[k] as f64;
            c.sum_var_time[k] += (t - old) * (t - c.mean_time[k]);
            if c.min_time[k] == 0.0 && c.max_time[k] == 0.0 {
                c.min_time[k] = t;
                c.max_time[k] = t;
            } else {
                c.min_time[k] = c.min_time[k].min(t);
                c.max_time[k] = c.max_time[k].max(t);
            }
        }
        c.rows += usage.rows as i64;
        c.shared_blks_written += usage.shared_blks;
        c.local_blks_written += usage.local_blks;
        c.usage += 1.0 + t / 1000.0;
        c.wal_records += usage.wal_records;
        c.wal_bytes += usage.wal_bytes;
        c.parallel_workers_to_launch += usage.workers_to_launch;
        c.parallel_workers_launched += usage.workers_launched;
        c.temp_blks_read += usage.temp_blks.0;
        c.temp_blks_written += usage.temp_blks.1;
    }

    /// `entry_dealloc`: the least used 5% (at least 10) go.
    fn dealloc(g: &mut Inner, max: usize) {
        let mut v: Vec<(Key, f64)> = g.entries.iter().map(|(k, e)| (*k, e.counters.usage)).collect();
        v.sort_by(|a, b| a.1.total_cmp(&b.1));
        let n = (max * DEALLOC_PERCENT / 100).max(10).min(v.len());
        for (k, _) in v.into_iter().take(n) {
            g.entries.remove(&k);
        }
        g.dealloc += 1;
    }

    /// `entry_reset`: the entries of `userid`, `dbid` and `queryid` (0: any) removed, or with
    /// `minmax_only` their minimum and maximum times cleared. Returns the reset's time.
    pub fn reset(&self, userid: u32, dbid: u32, queryid: i64, minmax_only: bool, now: i64) -> i64 {
        let mut g = self.inner.lock();
        let total = g.entries.len();
        let matches = |k: &Key| (userid == 0 || k.userid == userid) && (dbid == 0 || k.dbid == dbid) && (queryid == 0 || k.queryid == queryid);
        // (One user, database and statement: its two entries, top level and not.)
        let mut removed = 0;
        if minmax_only {
            for (k, e) in g.entries.iter_mut() {
                if matches(k) {
                    e.counters.min_time = [0.0; 2];
                    e.counters.max_time = [0.0; 2];
                    e.minmax_stats_since = now;
                }
            }
        } else {
            g.entries.retain(|k, _| {
                let gone = matches(k);
                removed += gone as usize;
                !gone
            });
        }
        if removed == total {
            g.dealloc = 0;
            g.stats_reset = now;
        }
        now
    }

    /// The entries in the order they were made.
    pub fn entries(&self) -> Vec<(Key, Entry)> {
        let g = self.inner.lock();
        let mut v: Vec<(Key, Entry)> = g.entries.iter().map(|(k, e)| (*k, e.clone())).collect();
        v.sort_by_key(|(_, e)| e.order);
        v
    }

    /// pg_stat_statements_info: deallocations and the last full reset.
    pub fn info(&self) -> (i64, i64) {
        let g = self.inner.lock();
        (g.dealloc, g.stats_reset)
    }
}

impl Counters {
    fn put(&self, w: &mut W) {
        let Counters {
            calls,
            total_time,
            min_time,
            max_time,
            mean_time,
            sum_var_time,
            rows,
            shared_blks_written,
            local_blks_written,
            wal_records,
            wal_fpi,
            wal_bytes,
            parallel_workers_to_launch,
            parallel_workers_launched,
            temp_blks_read,
            temp_blks_written,
            usage,
        } = self;
        for k in 0..2 {
            w.i64(calls[k]);
            for v in [total_time[k], min_time[k], max_time[k], mean_time[k], sum_var_time[k]] {
                w.f64(v);
            }
        }
        for v in [
            *rows,
            *shared_blks_written,
            *local_blks_written,
            *wal_records,
            *wal_fpi,
            *wal_bytes as i64,
            *parallel_workers_to_launch,
            *parallel_workers_launched,
            *temp_blks_read,
            *temp_blks_written,
        ] {
            w.i64(v);
        }
        w.f64(*usage);
    }

    fn get(r: &mut R<'_>) -> Option<Counters> {
        let mut c = Counters::default();
        for k in 0..2 {
            c.calls[k] = r.i64()?;
            c.total_time[k] = r.f64()?;
            c.min_time[k] = r.f64()?;
            c.max_time[k] = r.f64()?;
            c.mean_time[k] = r.f64()?;
            c.sum_var_time[k] = r.f64()?;
        }
        c.rows = r.i64()?;
        c.shared_blks_written = r.i64()?;
        c.local_blks_written = r.i64()?;
        c.wal_records = r.i64()?;
        c.wal_fpi = r.i64()?;
        c.wal_bytes = r.i64()? as u64;
        c.parallel_workers_to_launch = r.i64()?;
        c.parallel_workers_launched = r.i64()?;
        c.temp_blks_read = r.i64()?;
        c.temp_blks_written = r.i64()?;
        c.usage = r.f64()?;
        Some(c)
    }
}

/// The file's start and format version (PGSS_FILE_HEADER, PGSS_PG_MAJOR_VERSION): another's
/// is discarded.
const MAGIC: &[u8; 8] = b"ROLLPGSS";
const FORMAT: u32 = 1;

impl Store {
    /// The file's bytes (pgss_shmem_shutdown): every entry in the order they were made, sticky
    /// ones too, then the deallocation count and the last full reset; a CRC-32 of it all.
    pub fn dump(&self) -> Vec<u8> {
        let entries = self.entries();
        let (dealloc, stats_reset) = self.info();
        let mut w = W(Vec::with_capacity(64 + entries.len() * 256));
        w.0.extend_from_slice(MAGIC);
        w.u32(FORMAT);
        w.u32(entries.len() as u32);
        for (k, e) in &entries {
            w.u32(k.userid);
            w.u32(k.dbid);
            w.i64(k.queryid);
            w.0.push(k.toplevel as u8);
            w.str(&e.query);
            e.counters.put(&mut w);
            w.i64(e.stats_since);
            w.i64(e.minmax_stats_since);
        }
        w.i64(dealloc);
        w.i64(stats_reset);
        let crc = crc32fast::hash(&w.0);
        w.u32(crc);
        w.0
    }

    /// pgss_shmem_startup's load of the file's `bytes` into this new store: its entries but
    /// the sticky ones (never planned or run), the least used deallocated past
    /// pg_stat_statements.max, and the deallocation count and last reset. On an error the
    /// store is left as it was.
    pub fn restore(&self, bytes: &[u8]) -> Result<(), Bad> {
        let (entries, dealloc, stats_reset) = decode(bytes)?;
        let mut g = self.inner.lock();
        for (key, query, counters, stats_since, minmax_stats_since) in entries {
            if counters.sticky() || g.entries.contains_key(&key) {
                continue;
            }
            if g.entries.len() >= self.max {
                Self::dealloc(&mut g, self.max);
            }
            let order = g.next;
            g.next += 1;
            g.entries.insert(key, Entry { query: Arc::from(query), counters, stats_since, minmax_stats_since, order });
        }
        // (pgss->stats as the file has it, read last: the deallocations above aren't counted.)
        g.dealloc = dealloc;
        g.stats_reset = stats_reset;
        Ok(())
    }
}

/// Why a file wasn't loaded: it ends early (pgss_shmem_startup's read_error), or it isn't
/// one of this format or doesn't check (data_error).
#[derive(Debug, PartialEq, Eq)]
pub enum Bad {
    Short,
    Invalid,
}

type Saved = (Key, String, Counters, i64, i64);

fn decode(bytes: &[u8]) -> Result<(Vec<Saved>, i64, i64), Bad> {
    let mut r = R(bytes);
    // (The header, the version and the count read before either is looked at, as
    // pgss_shmem_startup does.)
    let (magic, format, n) = (|| Some((r.take(8)? == MAGIC, r.u32()?, r.u32()? as usize)))().ok_or(Bad::Short)?;
    if !magic || format != FORMAT {
        return Err(Bad::Invalid);
    }
    let mut toplevel_ok = true;
    let mut body = || -> Option<(Vec<Saved>, i64, i64)> {
        let mut v = Vec::with_capacity(n.min(r.0.len() / 17));
        for _ in 0..n {
            let (userid, dbid, queryid) = (r.u32()?, r.u32()?, r.i64()?);
            let toplevel = r.take(1)?[0];
            toplevel_ok &= toplevel < 2;
            let query = r.str()?;
            let counters = Counters::get(&mut r)?;
            let key = Key { userid, dbid, queryid, toplevel: toplevel == 1 };
            v.push((key, query, counters, r.i64()?, r.i64()?));
        }
        Some((v, r.i64()?, r.i64()?))
    };
    let (v, dealloc, reset) = body().ok_or(Bad::Short)?;
    let end = bytes.len() - r.0.len();
    let crc = r.u32().ok_or(Bad::Short)?;
    let valid = r.0.is_empty() && toplevel_ok && crc == crc32fast::hash(&bytes[..end]);
    if valid { Ok((v, dealloc, reset)) } else { Err(Bad::Invalid) }
}

/// `cur_median_usage`: a sticky entry starts at the median usage.
fn median_usage(g: &Inner) -> f64 {
    let mut v: Vec<f64> = g.entries.values().map(|e| e.counters.usage).collect();
    if v.is_empty() {
        return USAGE_INIT;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}
