//! A disk that fills up mid-statement (ENOSPC), on a small tmpfs of the test's own: mounted in
//! a user and mount namespace (`unshare -Urm`), so nothing shared fills and no root is needed.
//! Where the namespace can't be made (user namespaces off), the test says so and passes.
//!
//! PostgreSQL 18.6 on a full disk: the statement fails with 53100 (errcode_for_file_access's
//! ENOSPC), its transaction aborts, the server keeps running and takes new connections, and
//! once there is space again it carries on; a commit it acknowledged is there after a crash,
//! and one it reported failed is not.

use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[path = "common/raw.rs"]
mod raw;

const TEST: &str = "a_full_disk_fails_statements_cleanly";

fn open(dir: &Path) -> std::sync::Arc<Engine> {
    Engine::open(EngineOptions {
        data_dir: dir.to_path_buf(),
        cache_bytes: 4 << 20,
        memtable_bytes: 1 << 20,
        sync: true,
        idle_close: Duration::from_secs(600),
        superuser: "postgres".into(),
        password: None,
        server_files: None,
        server_programs: false,
        shared_preload_libraries: Vec::new(),
        locale: Default::default(),
        outbound: Vec::new(),
        max_prepared_transactions: 0,
        auth_files: Default::default(),
        settings: Vec::new(),
    })
    .unwrap()
}

/// Batch `i`'s rows: ids i*1000 .. i*1000 + n (n 2,000 for every fifth batch: a commit that
/// goes in as an SST of its own), 400-byte values.
fn batch(i: u64) -> String {
    let n = if i % 5 == 4 { 2000 } else { 100 };
    format!("INSERT INTO t SELECT {i} * 10000 + g, repeat(md5(g::text), 12) FROM generate_series(1, {n}) g")
}

fn errors(r: &[String]) -> Vec<&str> {
    r.iter().filter_map(|m| m.strip_prefix("E ")).collect()
}

/// The workload (a process of its own, ended without a shutdown: a crash): batches until the
/// disk is full and some more after; each failure checked as it comes; "acked i" for each batch
/// acknowledged, "failed i" for each that reported an error.
fn work(dir: &Path, filler: &Path) {
    let engine = open(dir);
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust }).unwrap().to_string();
    let mut c = raw::Raw::connect(&addr, "postgres");
    assert!(errors(&c.query("CREATE TABLE t(id bigint PRIMARY KEY, pad text)")).is_empty());
    // All but ~4 MB of the filesystem taken.
    let free = || {
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        let p = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::statvfs(p.as_ptr(), &mut s) }, 0);
        s.f_bavail * s.f_frsize
    };
    std::fs::write(filler, vec![1u8; (free() - (4 << 20)) as usize]).unwrap();
    let (mut i, mut failures, mut freed) = (0u64, 0, false);
    while i < 400 {
        let r = c.query(&batch(i));
        let e = errors(&r);
        if e.is_empty() {
            println!("acked {i}");
            if freed && i > 0 && i.is_multiple_of(7) {
                break;
            }
        } else {
            println!("failed {i}");
            assert_eq!(e, ["53100"], "batch {i}: {r:?}");
            assert_eq!(c.status, b'I', "batch {i}");
            failures += 1;
            // A transaction block's statement failing on the full disk: the block is aborted
            // and ends with ROLLBACK.
            assert!(errors(&c.query("BEGIN")).is_empty());
            let big = c.query(&batch(10_000 + i));
            if !errors(&big).is_empty() {
                assert_eq!(errors(&big), ["53100"], "a block's batch: {big:?}");
                assert_eq!(c.query("SELECT 1"), ["E 25P02"]);
            }
            let end = c.query("ROLLBACK");
            assert!(errors(&end).is_empty(), "{end:?}");
            assert_eq!(c.status, b'I');
            // The server keeps running: this session and a new one read.
            assert_eq!(c.query("SELECT 1")[1..], ["D 1", "C SELECT 1"]);
            let mut other = raw::Raw::connect(&addr, "postgres");
            let n = other.query("SELECT count(*) > 0 FROM t");
            assert_eq!(n[1..], ["D t", "C SELECT 1"], "{n:?}");
            other.send(b'X', b"");
            if failures == 3 {
                // Space again: the same session carries on.
                std::fs::remove_file(filler).unwrap();
                freed = true;
            }
        }
        i += 1;
    }
    assert!(failures >= 3, "the disk never filled (batches {i})");
    assert!(freed);
    // A crash: no shutdown.
    use std::io::Write as _;
    std::io::stdout().flush().unwrap();
    unsafe { libc::_exit(0) };
}

/// In the namespace: the tmpfs, the workload in a process of its own, then the data opened
/// again and checked against what it was told.
fn in_namespace() {
    let base = tempfile::tempdir().unwrap();
    let mnt = base.path().join("disk");
    std::fs::create_dir(&mnt).unwrap();
    let (src, fs, opts) = (c"tmpfs", c"tmpfs", c"size=48m");
    let target = std::ffi::CString::new(mnt.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: plain C strings, a mount in this process's own mount namespace.
    let r = unsafe { libc::mount(src.as_ptr(), target.as_ptr(), fs.as_ptr(), 0, opts.as_ptr().cast()) };
    assert_eq!(r, 0, "mount: {}", std::io::Error::last_os_error());
    let dir = mnt.join("data");
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .env("DISK_FULL_STEP", "work")
        .env("DISK_FULL_DIR", &dir)
        .env("DISK_FULL_FILLER", mnt.join("filler"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "workload: {}\n{stdout}\n{}", out.status, String::from_utf8_lossy(&out.stderr));
    // (The first line follows libtest's "test ... " on its line.)
    let num = |p: &str| stdout.lines().filter_map(|l| l.split(p).nth(1)?.parse::<u64>().ok()).collect::<Vec<_>>();
    let (acked, failed) = (num("acked "), num("failed "));
    let engine = open(&dir);
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", ServerOptions { auth: AuthMethod::Trust }).unwrap().to_string();
    let mut c = raw::Raw::connect(&addr, "postgres");
    let r = c.query("SELECT id / 10000, count(*) FROM t GROUP BY 1 ORDER BY 1");
    let have: Vec<(u64, u64)> = r
        .iter()
        .filter_map(|m| m.strip_prefix("D "))
        .map(|d| {
            let (a, b) = d.split_once('|').unwrap();
            (a.parse().unwrap(), b.parse().unwrap())
        })
        .collect();
    let want: Vec<(u64, u64)> = acked.iter().map(|&i| (i, if i % 5 == 4 { 2000 } else { 100 })).collect();
    assert_eq!(have, want, "after the crash: batches there (and their rows) vs acknowledged; failed {failed:?}");
    engine.shutdown();
}

#[test]
fn a_full_disk_fails_statements_cleanly() {
    match std::env::var("DISK_FULL_STEP").as_deref() {
        Ok("work") => {
            let dir = std::env::var_os("DISK_FULL_DIR").unwrap();
            let filler = std::env::var_os("DISK_FULL_FILLER").unwrap();
            return work(Path::new(&dir), Path::new(&filler));
        }
        Ok("namespace") => return in_namespace(),
        _ => {}
    }
    // A user namespace of our own, root in it, with a mount namespace to mount the tmpfs in.
    if !Command::new("unshare").args(["-Urm", "true"]).status().is_ok_and(|s| s.success()) {
        eprintln!("{TEST}: skipped, no user namespaces here (unshare -Urm failed)");
        return;
    }
    let out = Command::new("unshare")
        .arg("-Urm")
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture", "--test-threads=1"])
        .env("DISK_FULL_STEP", "namespace")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
