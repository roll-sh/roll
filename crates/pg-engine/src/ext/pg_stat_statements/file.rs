//! pg_stat_statements' file (`pg_stat/pg_stat_statements.stat`, PGSS_DUMP_FILE): the
//! statistics kept across a clean shutdown when `pg_stat_statements.save` is on.
//!
//! As in PostgreSQL 18.6: written at a clean shutdown (pgss_shmem_shutdown, never after a
//! crash or an immediate shutdown), read at the next start and removed once read, so a crash
//! after that start begins from nothing. With `pg_stat_statements.save` off at the start the
//! file is neither read nor removed (PostgreSQL's historical behaviour). A file that can't be
//! read or doesn't decode is removed with PostgreSQL's LOG line and the statistics start
//! empty: it never stops a start.

use std::path::Path;

use super::Store;
use super::store::Bad;

/// PGSS_DUMP_FILE, in the data directory.
pub const NAME: &str = "pg_stat/pg_stat_statements.stat";

/// Write `store`'s statistics durably (`sync`: with the fsyncs) to the file in data
/// directory `dir` (a temporary file renamed into place, as durable_rename).
pub fn write(dir: &Path, store: &Store, sync: bool) -> std::io::Result<()> {
    let p = dir.join(NAME);
    if let Some(parent) = p.parent() {
        crate::storage::durable::create_dir_all_synced(parent)?;
    }
    crate::storage::durable::replace(&p, &store.dump(), sync)
}

/// Load the file in data directory `dir` into `store` and remove it. The LOG line
/// PostgreSQL writes when it can't be read or doesn't decode (then removed too).
pub fn take(dir: &Path, store: &Store) -> Option<String> {
    let p = dir.join(NAME);
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            let _ = std::fs::remove_file(&p);
            // (%m: strerror's text, std's without its " (os error N)".)
            let m = e.to_string();
            let m = m.split(" (os error").next().unwrap_or_default().to_string();
            return Some(format!("could not read file \"{NAME}\": {m}"));
        }
    };
    let _ = std::fs::remove_file(&p);
    match store.restore(&bytes) {
        Ok(()) => None,
        // (A short fread's %m: the errno the startup's unlink of the absent query text file
        // left, as PostgreSQL 18.6 logs it.)
        Err(Bad::Short) => Some(format!("could not read file \"{NAME}\": No such file or directory")),
        Err(Bad::Invalid) => Some(format!("ignoring invalid data in file \"{NAME}\"")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Key, Kind, Store, Usage};
    use super::Bad;

    #[test]
    fn a_dump_restores_whole_and_nothing_else_does() {
        let a = Store::new(-7, 100);
        let key = |q| Key { userid: 10, dbid: 5, queryid: q, toplevel: true };
        let u = Usage { millis: 2.5, rows: 3, wal_bytes: 99, ..Usage::default() };
        a.store(key(1), || "SELECT $1".into(), Some(Kind::Exec), u, 1);
        a.store(key(1), || unreachable!(), Some(Kind::Exec), u, 2);
        a.store(key(2), || "SELECT 2".into(), None, Usage::default(), 3);
        a.store(key(3), || "UPDATE t SET a = $1".into(), Some(Kind::Plan), u, 4);
        let b = a.dump();
        let r = Store::new(0, 100);
        assert_eq!(r.restore(&b), Ok(()));
        // (The sticky entry, never planned or run, isn't loaded.)
        let got: Vec<_> = r.entries().into_iter().map(|(k, e)| (k.queryid, e.query.to_string(), e.counters.calls, e.counters.rows, e.counters.wal_bytes, e.stats_since)).collect();
        assert_eq!(got, [(1, "SELECT $1".into(), [0, 2], 6, 198, 1), (3, "UPDATE t SET a = $1".into(), [1, 0], 3, 99, 4)]);
        assert_eq!(r.info(), (0, -7));
        // Every prefix (a torn write), a flipped byte: nothing loaded.
        for n in 0..b.len() {
            assert_eq!(Store::new(0, 100).restore(&b[..n]), Err(Bad::Short), "prefix {n}");
        }
        let mut bad = b.clone();
        bad[30] ^= 1;
        let s = Store::new(0, 100);
        assert_eq!(s.restore(&bad), Err(Bad::Invalid));
        assert!(s.entries().is_empty());
        assert_eq!(Store::new(0, 100).restore(b"junkjunk\n"), Err(Bad::Short));
        let mut other = b.clone();
        other[0] ^= 1;
        assert_eq!(Store::new(0, 100).restore(&other), Err(Bad::Invalid));
    }
}
