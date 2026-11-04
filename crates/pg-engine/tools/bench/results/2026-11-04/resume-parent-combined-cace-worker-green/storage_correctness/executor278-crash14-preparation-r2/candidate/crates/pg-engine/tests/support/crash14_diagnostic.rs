//! Test-only preparation: uncompiled, unformatted and unrun.
use pg_engine::session::Session;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EVENT_LIMIT: u64 = 256 * 1024 * 1024; // per process/channel; no fsync in child
const IMAGE_LIMIT: u64 = 2 * 1024 * 1024 * 1024; // each of three images

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CRASH_DIAG_DIR").is_some())
}

pub fn clock_ns() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

struct Channel { file: File, bytes: u64 }
thread_local! {
    // A channel has one thread/writer and one append handle for its lifetime.
    static CHANNELS: RefCell<BTreeMap<String, Option<Channel>>> = RefCell::new(BTreeMap::new());
}

pub fn best_effort(f: impl FnOnce()) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
}

// The closure is never evaluated when disabled. Event errors never unwind into
// the workload. A disabled/error channel makes the evidence incomplete.
pub fn event(_dir: &Path, channel: &str, build: impl FnOnce() -> Value) -> bool {
    if !enabled() { return false; }
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> io::Result<()> {
        CHANNELS.with(|channels| {
            let mut channels = channels.borrow_mut();
            if !channels.contains_key(channel) {
                let root = PathBuf::from(std::env::var_os("CRASH_DIAG_DIR").ok_or_else(|| io::Error::other("missing root"))?);
                let path = root.join("events").join(format!("{channel}-{}.jsonl", std::process::id()));
                let opened = OpenOptions::new().create_new(true).append(true).open(path)
                    .map(|file| Channel { file, bytes: 0 }).ok();
                channels.insert(channel.to_owned(), opened);
            }
            let slot = channels.get_mut(channel).unwrap();
            let writer = slot.as_mut().ok_or_else(|| io::Error::other("event channel unavailable"))?;
            let mut line = serde_json::to_vec(&json!({"wall_ns":clock_ns().to_string(),
                "pid":std::process::id(),"event":build()}))?;
            line.push(b'\n');
            if writer.bytes.saturating_add(line.len() as u64) > EVENT_LIMIT {
                *slot = None;
                return Err(io::Error::other("event byte budget exhausted"));
            }
            // One write_all call includes the newline. OS partial writes / a torn
            // final record remain possible; this is not a durability guarantee.
            if let Err(error) = writer.file.write_all(&line) {
                *slot = None;
                return Err(error);
            }
            writer.bytes += line.len() as u64;
            Ok(())
        })
    })).is_ok_and(|result| result.is_ok())
}

fn stream_hash(path: &Path) -> io::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

pub fn begin(seeds: &[u64], rounds: u64) -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("CRASH_DIAG_DIR")?);
    assert_eq!(seeds, &[14]);
    assert_eq!(rounds, 3);
    assert!(root.is_absolute());
    let parent = root.parent().unwrap().canonicalize().unwrap();
    assert!(!parent.starts_with("/tmp") && !parent.starts_with("/dev/shm"), "disk cache required");
    fs::create_dir(&root).unwrap(); // refuse reuse
    fs::create_dir(root.join("events")).unwrap();
    let exe = std::env::current_exe().unwrap();
    let (_, binary_sha256) = stream_hash(&exe).unwrap();
    assert_ne!(binary_sha256, "a35916fbfff20bcbe555d5a5c1d77d0688d24686f9696a1148ce2e1c8ec59ec5");
    let source_manifest = PathBuf::from(std::env::var_os("CRASH_DIAG_SOURCE_MANIFEST").expect("source manifest required"));
    let (_, source_manifest_sha256) = stream_hash(&source_manifest).unwrap();
    assert!(event(&root, "main", || json!({"kind":"instrumented_binary","path":exe,
        "sha256":binary_sha256,"source_manifest":source_manifest,
        "source_manifest_sha256":source_manifest_sha256,"same_as_original":false})));
    Some(root)
}

pub fn seed_dir(root: &Path, seed: u64) -> PathBuf {
    let dir = root.join(format!("seed-{seed}"));
    fs::create_dir(&dir).unwrap();
    dir
}

// No event or filesystem operation lies on the killer's path. Watchdogs are
// independent of event writing, SQL, and the main thread. The process-group
// deadline in the future plan is still required, including before arming.
fn kill_self() {
    let me = std::process::id().to_string();
    let _ = std::process::Command::new("kill").args(["-9", &me]).status();
    std::process::abort(); // if the signal command could not execute
}

pub fn child_watchdog() {
    if !enabled() { return; }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(300));
        kill_self();
    });
}

pub fn arm(dir: &Path, at: usize, delay_us: u64) {
    let armed = Instant::now(); // captured in the main thread, before spawn
    let wall_ns = clock_ns();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_micros(delay_us));
        kill_self();
    });
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(5));
        kill_self();
    });
    let _ = event(dir, "main", || json!({"kind":"kill_armed","at":at,
        "requested_delay_us":delay_us,"armed_wall_ns":wall_ns.to_string(),
        "elapsed_since_arm_us":armed.elapsed().as_micros().to_string(),
        "signal_time_unobserved":true}));
}

// Audit only at original read_log sites and after child exit. Never repair flags.
pub fn journal(dir: &Path, stage: &str) {
    if !enabled() { return; }
    best_effort(|| {
        match fs::read(dir.join("steps")) {
            Err(error) => { let _ = event(dir, "main", || json!({"kind":"journal","stage":stage,
                "read_error":format!("{error:?}"),"original_read_error_maps_to_empty":true})); }
            Ok(bytes) => {
                let mut records = Vec::new();
                let mut offset = 0;
                for (expected_id, line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
                    let complete = line.ends_with(b"\n");
                    let text = std::str::from_utf8(line).ok();
                    let fields: Vec<_> = text.unwrap_or("").trim_end_matches('\n').split(' ').collect();
                    let id = fields.first().and_then(|s| s.parse::<usize>().ok());
                    let status = fields.get(1).copied();
                    let valid = complete && fields.len() == 2 && id == Some(expected_id)
                        && matches!(status, Some("c" | "a"));
                    records.push(json!({"offset":offset,"bytes":line.len(),"id":id,
                        "expected_id":expected_id,"status":status,"complete":complete,"valid":valid}));
                    offset += line.len();
                }
                let _ = event(dir, "main", || json!({"kind":"journal","stage":stage,"bytes":bytes.len(),
                    "sha256":digest(&bytes),"raw_hex":bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                    "utf8_valid":std::str::from_utf8(&bytes).is_ok(),"records":records}));
            }
        }
    });
}

fn copy_file(src: &Path, dst: &Path, rel: &Path, entries: &mut Vec<Value>, total: &mut u64) -> io::Result<()> {
    let mut source = File::open(src)?;
    let mut target = OpenOptions::new().write(true).create_new(true).open(dst)?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 { break; }
        *total += count as u64;
        if *total > IMAGE_LIMIT { return Err(io::Error::other("image budget exceeded; partial image retained")); }
        target.write_all(&buffer[..count])?;
        hash.update(&buffer[..count]);
        size += count as u64;
    }
    target.sync_all()?;
    let hash = format!("{:x}", hash.finalize());
    if stream_hash(src)? != (size, hash.clone()) || stream_hash(dst)? != (size, hash.clone()) {
        return Err(io::Error::other("copy hash mismatch"));
    }
    entries.push(json!({"path":rel,"bytes":size,"sha256":hash}));
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path, rel: &Path, entries: &mut Vec<Value>, total: &mut u64) -> io::Result<()> {
    let ty = fs::symlink_metadata(src)?.file_type();
    if ty.is_symlink() { return Err(io::Error::other("symlink refused")); }
    if ty.is_file() { return copy_file(src, dst, rel, entries, total); }
    if !ty.is_dir() { return Err(io::Error::other("special file refused")); }
    fs::create_dir(dst)?;
    let mut children = fs::read_dir(src)?.collect::<Result<Vec<_>, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        copy_tree(&child.path(), &dst.join(child.file_name()), &rel.join(child.file_name()), entries, total)?;
    }
    File::open(dst)?.sync_all()
}

fn write_evidence(path: &Path, value: &Value) -> io::Result<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

pub fn before_parent_open(dir: &Path, seed: u64, round: u64,
                          child: &std::process::Output, at: usize, delay_us: u64) {
    if !enabled() { return; }
    let root = PathBuf::from(std::env::var_os("CRASH_DIAG_DIR").unwrap());
    // Commit child status/output BEFORE journal audit or any image copy. A copy
    // failure cannot erase this independent file. Failure here stops capture.
    let exit = json!({"kind":"child_exit","seed":seed,"round":round,
        "status":format!("{:?}",child.status),"kill_at":at,"requested_delay_us":delay_us,
        "stdout_hex":child.stdout.iter().map(|b|format!("{b:02x}")).collect::<String>(),
        "stderr_hex":child.stderr.iter().map(|b|format!("{b:02x}")).collect::<String>()});
    write_evidence(&root.join(format!("seed-{seed}-round-{round}-child-exit.json")), &exit).unwrap();
    File::open(&root).unwrap().sync_all().unwrap();
    let _ = event(dir, "main", || exit);
    journal(dir, "after-child-exit-before-parent-open");
    let image = root.join(format!("seed-{seed}-round-{round}-before-parent-open"));
    fs::create_dir(&image).unwrap();
    let mut entries = Vec::new();
    let mut total = 0;
    // Never copy event files. The image itself is never opened by the harness.
    for name in ["data", "steps"] {
        copy_tree(&dir.join(name), &image.join(name), Path::new(name), &mut entries, &mut total).unwrap();
    }
    File::open(&image).unwrap().sync_all().unwrap();
    write_evidence(&root.join(format!("seed-{seed}-round-{round}-image.json")), &json!({
        "seed":seed,"round":round,"copy_complete":true,"captured_before_parent_engine_open":true,
        "never_opened_reference":true,"bytes":total,"entries":entries})).unwrap();
    File::open(&root).unwrap().sync_all().unwrap();
}

fn query_snapshot(s: &mut Session, dir: &Path, stage: &str, sql: &str) -> bool {
    let _ = event(dir, "main", || json!({"kind":"snapshot_query_begin","stage":stage,"sql":sql}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.query(sql)));
    let ok = matches!(&result, Ok(Ok(_)));
    let _ = event(dir, "main", || json!({"kind":"snapshot_query_result","stage":stage,"sql":sql,
        "ok":ok,"result_debug":match &result { Ok(result)=>format!("{result:?}"), Err(_)=>"query panicked".into() }}));
    ok
}

pub fn snapshot(s: &mut Session, dir: &Path, stage: &str) {
    if !enabled() { return; }
    // Called only AFTER original check, or AFTER an original settle/check panic.
    // Failure to establish a timeout skips reads; no assert can replace a panic.
    let reset = query_snapshot(s, dir, stage, "RESET ALL");
    let bounded = query_snapshot(s, dir, stage, "SET statement_timeout = '2000ms'");
    let mut complete = reset && bounded;
    if complete {
        for sql in ["SELECT * FROM pg_prepared_xacts ORDER BY gid", "SELECT * FROM txlog ORDER BY i",
                    "SELECT relname, relkind FROM pg_class WHERE relkind = 'i' ORDER BY relname"] {
            complete &= query_snapshot(s, dir, stage, sql);
        }
        for (how, settings) in [
            ("seqscan", "SET enable_seqscan = on; SET enable_indexscan = off; SET enable_bitmapscan = off; SET enable_indexonlyscan = off"),
            ("indexscan", "SET enable_seqscan = off; SET enable_bitmapscan = off; SET enable_indexscan = on; SET enable_indexonlyscan = on"),
            ("bitmapscan", "SET enable_seqscan = off; SET enable_indexscan = off; SET enable_indexonlyscan = off; SET enable_bitmapscan = on"),
        ] {
            let configured = query_snapshot(s, dir, stage, settings);
            complete &= configured;
            if configured {
                for table in ["a", "b"] {
                    let _ = event(dir, "main", || json!({"kind":"scan_snapshot","stage":stage,"table":table,"how":how}));
                    complete &= query_snapshot(s, dir, stage,
                        &format!("SELECT ctid, xmin, * FROM {table} WHERE id > -1 ORDER BY id"));
                    complete &= query_snapshot(s, dir, stage,
                        &format!("SELECT ctid, xmin, * FROM {table} ORDER BY id"));
                }
            }
        }
        let sql = "SELECT relname FROM pg_class WHERE relname ~ '^e[0-9]+$' AND relkind = 'r' ORDER BY relname";
        let tables = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.query(sql)));
        let _ = event(dir, "main", || json!({"kind":"snapshot_table_list","stage":stage,
            "result_debug":match &tables { Ok(result)=>format!("{result:?}"), Err(_)=>"query panicked".into() }}));
        match tables {
            Ok(Ok(results)) => for result in results { for row in result.rows {
                if let Some(pg_engine::types::Value::Text(name)) = row.first() {
                    if name.starts_with('e') && name.len() > 1 && name[1..].bytes().all(|b|b.is_ascii_digit()) {
                        complete &= query_snapshot(s, dir, stage, &format!("SELECT * FROM {name} ORDER BY k"));
                    } else { complete = false; }
                } else { complete = false; }
            }},
            _ => complete = false,
        }
    }
    complete &= query_snapshot(s, dir, stage, "RESET ALL");
    let _ = event(dir, "main", || json!({"kind":"snapshot_complete","stage":stage,"complete":complete,
        "consistency":"separate statement snapshots; planner settings requested, actual path unverified"}));
}

pub fn comparison(dir: &Path, table: &str, how: &str, got: &[String], want: &[String]) {
    if !enabled() || got == want { return; }
    let _ = event(dir, "main", || {
        let counts = |rows: &[String]| {
            let mut counts = BTreeMap::<String, usize>::new();
            for row in rows { *counts.entry(row.clone()).or_default() += 1; }
            counts
        };
        json!({"kind":"complete_mismatch","table":table,"how":how,"actual":got,"expected":want,
            "actual_multiplicities":counts(got),"expected_multiplicities":counts(want)})
    });
}
