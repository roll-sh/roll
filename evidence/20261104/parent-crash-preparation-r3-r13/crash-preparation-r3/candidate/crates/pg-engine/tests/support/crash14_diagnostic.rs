//! PREPARATION ONLY: UNCOMPILED / UNFORMATTED / UNRUN.
use pg_engine::session::Session;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::fmt::{self, Debug};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::{Once, OnceLock};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const REVISION: &str = "preparation-r3";
const SCHEMA: u32 = 3;
const STATUS: &str = "UNCOMPILED/UNFORMATTED/UNRUN";
const RECORD_LIMIT: usize = 1024 * 1024;
const DUMP_LIMIT: u64 = 16 * 1024 * 1024;
const CHUNK: usize = 32 * 1024;
const IMAGE_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
const HARNESS_BYTES: &[u8] = include_bytes!("../crash_random.rs");
const SUPPORT_BYTES: &[u8] = include_bytes!("crash14_diagnostic.rs");
static CHILD_STATE: OnceLock<File> = OnceLock::new();
static CHILD_ARMED: AtomicBool = AtomicBool::new(false);
thread_local! {
    static FAILURE: Cell<bool> = const { Cell::new(false) };
    static IN_DIAGNOSTIC: Cell<bool> = const { Cell::new(false) };
    static CHANNELS: RefCell<BTreeMap<String, Channel>> = RefCell::new(BTreeMap::new());
}

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CRASH_DIAG_DIR").is_some())
}
fn parent() -> bool { std::env::var_os("CRASH_RANDOM_DIR").is_none() }
fn root() -> io::Result<PathBuf> {
    std::env::var_os("CRASH_DIAG_DIR").map(PathBuf::from).ok_or_else(|| io::Error::other("capture root absent"))
}
pub fn clock_ns() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}
pub fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn envelope(value: Value) -> Value {
    json!({"diagnostic_revision":REVISION,"event_schema":SCHEMA,"preparation_status":STATUS,
        "wall_ns":clock_ns().to_string(),"pid":std::process::id(),"event":value})
}
fn install_panic_hook() {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if IN_DIAGNOSTIC.with(Cell::get) {
                let _ = writeln!(io::stderr().lock(), "crash14 diagnostic-only panic caught; oracle panic hook unchanged for original work");
            } else { original(info); }
        }));
    });
}
fn catch_diagnostic<T>(f: impl FnOnce() -> T) -> std::thread::Result<T> {
    let old = IN_DIAGNOSTIC.with(|flag| flag.replace(true));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    IN_DIAGNOSTIC.with(|flag| flag.set(old));
    result
}
pub fn best_effort(f: impl FnOnce()) { let _ = catch_diagnostic(f); }
pub fn begin_failure() { if enabled() { FAILURE.with(|flag| flag.set(true)); } }
fn channel_key(channel: &str) -> String {
    match (FAILURE.with(Cell::get), channel) {
        (true, "main") => "failure".into(),
        (true, "summary") => "failure-summary".into(),
        _ => channel.to_owned(),
    }
}
fn budget(key: &str) -> u64 {
    match key { "failure" => 256 * 1024 * 1024, "summary" | "failure-summary" => 8 * 1024 * 1024,
        _ => 128 * 1024 * 1024 }
}
fn marker_write(file: &File, value: Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&envelope(value))?;
    if bytes.len() > 4095 { return Err(io::Error::other("marker exceeds reserved slot")); }
    bytes.resize(4095, b' ');
    bytes.push(b'\n');
    file.write_all_at(&bytes, 0)?;
    if parent() { file.sync_all()?; } // NEVER fsync a child's marker/event
    Ok(())
}
struct Channel { file: Option<File>, marker: File, bytes: u64 }
impl Channel {
    fn open(key: &str) -> io::Result<Self> {
        let directory = root()?.join("events");
        // Allocate the status slot before the data file. It remains readable if
        // the channel exhausts its data budget. Filesystem-wide I/O failure can
        // still prevent a marker update; parent treats missing/invalid as loss.
        let marker = OpenOptions::new().create_new(true).read(true).write(true)
            .open(directory.join(format!("channel-closed-{key}-{}.json", std::process::id())))?;
        marker_write(&marker, json!({"kind":"channel_status","channel":key,"closed":false,"bytes":0}))?;
        let opened = OpenOptions::new().create_new(true).append(true)
            .open(directory.join(format!("{key}-{}.jsonl", std::process::id())));
        let mut result = Self { file: None, marker, bytes: 0 };
        match opened {
            Ok(file) => result.file = Some(file),
            Err(error) => { result.close(key, &format!("open: {error}")); }
        }
        if parent() { File::open(directory)?.sync_all()?; }
        Ok(result)
    }
    fn close(&mut self, key: &str, reason: &str) {
        self.file = None;
        if marker_write(&self.marker, json!({"kind":"channel_closed","channel":key,
            "closed":true,"reason":reason,"bytes":self.bytes})).is_err() {
            let _ = writeln!(io::stderr().lock(), "crash14 diagnostic channel-closed marker unavailable: {key}; capture incomplete");
        }
    }
}
struct Record(Vec<u8>);
impl Write for Record {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > RECORD_LIMIT - 1 {
            return Err(io::Error::other("per-record byte cap"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
// Lazy closure: disabled mode evaluates none of its JSON/format/hash work.
pub fn event(_dir: &Path, channel: &str, build: impl FnOnce() -> Value) -> bool {
    if !enabled() { return false; }
    let key = channel_key(channel);
    let result = catch_diagnostic(|| CHANNELS.with(|channels| -> io::Result<()> {
        let mut channels = channels.borrow_mut();
        if !channels.contains_key(&key) { channels.insert(key.clone(), Channel::open(&key)?); }
        let writer = channels.get_mut(&key).ok_or_else(|| io::Error::other("channel absent"))?;
        if writer.file.is_none() { return Err(io::Error::other("channel already closed")); }
        let mut line = Record(Vec::new());
        if let Err(error) = serde_json::to_writer(&mut line, &envelope(build())) {
            writer.close(&key, &format!("serialization/record cap: {error}"));
            return Err(error.into());
        }
        line.0.push(b'\n');
        if writer.bytes.saturating_add(line.0.len() as u64) > budget(&key) {
            writer.close(&key, "channel byte budget exhausted");
            return Err(io::Error::other("channel byte budget exhausted"));
        }
        let result = writer.file.as_mut().ok_or_else(|| io::Error::other("closed"))?.write_all(&line.0);
        if let Err(error) = result {
            writer.close(&key, &format!("write: {error}"));
            return Err(error);
        }
        writer.bytes += line.0.len() as u64;
        Ok(())
    }));
    let ok = matches!(&result, Ok(Ok(())));
    if !ok {
        best_effort(|| {
            let mut registered = false;
            CHANNELS.with(|channels| {
                if let Some(writer) = channels.borrow_mut().get_mut(&key) {
                    registered = true;
                    if writer.file.is_some() { writer.close(&key, "diagnostic builder panicked"); }
                }
            });
            if !registered {
                // Open/setup failure may precede registration. Attempt a marker
                // independently; missing/invalid marker is incomplete evidence.
                if let Ok(directory) = root().map(|path| path.join("events")) {
                    let path = directory.join(format!("channel-closed-{key}-{}.json", std::process::id()));
                    let marked = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)
                        .and_then(|file| marker_write(&file, json!({"kind":"channel_closed","channel":key,
                            "closed":true,"reason":"channel setup/I/O failure","bytes":0})));
                    if parent() { let _ = File::open(directory).and_then(|file| file.sync_all()); }
                    if marked.is_err() { let _ = writeln!(io::stderr().lock(), "crash14 diagnostic marker unavailable: {key}; incomplete"); }
                }
            }
        });
    }
    ok
}

struct HashCount { hash: Sha256, bytes: u64 }
impl fmt::Write for HashCount {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.hash.update(value.as_bytes());
        self.bytes += value.len() as u64;
        Ok(())
    }
}
struct Dump<'a> { dir: &'a Path, label: &'a str, id: u64, chunk: Vec<u8>, bytes: u64, ordinal: u64, ok: bool }
impl Dump<'_> {
    fn flush(&mut self) -> fmt::Result {
        if self.chunk.is_empty() { return Ok(()); }
        self.ok &= event(self.dir, "main", || json!({"kind":"debug_dump_chunk","label":self.label,
            "dump_id":self.id,"ordinal":self.ordinal,"offset":self.bytes - self.chunk.len() as u64,
            "raw_hex":self.chunk.iter().map(|byte| format!("{byte:02x}")).collect::<String>()}));
        self.ordinal += 1;
        self.chunk.clear();
        if self.ok { Ok(()) } else { Err(fmt::Error) }
    }
}
impl fmt::Write for Dump<'_> {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        for bytes in value.as_bytes().chunks(CHUNK) {
            if self.bytes + bytes.len() as u64 > DUMP_LIMIT { self.ok = false; return Err(fmt::Error); }
            if self.chunk.len() + bytes.len() > CHUNK { self.flush()?; }
            self.chunk.extend_from_slice(bytes);
            self.bytes += bytes.len() as u64;
            if self.chunk.len() == CHUNK { self.flush()?; }
        }
        Ok(())
    }
}
// The SHA+length descriptor is written FIRST. Debug is the exact deterministic
// byte representation for this instrumented binary, not a cross-version format.
pub fn debug_dump<T: Debug + ?Sized>(dir: &Path, label: &str, value: &T) -> bool {
    if !enabled() { return false; }
    catch_diagnostic(|| {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut hashed = HashCount { hash: Sha256::new(), bytes: 0 };
        let formatted = fmt::write(&mut hashed, format_args!("{value:?}")).is_ok();
        let sha256 = format!("{:x}", hashed.hash.finalize());
        let first = event(dir, "summary", || json!({"kind":"debug_dump_digest","label":label,"dump_id":id,
            "sha256":sha256,"byte_length":hashed.bytes,"format":"Rust Debug UTF-8 bytes for this binary",
            "dump_limit_bytes":DUMP_LIMIT,"digest_complete":formatted}));
        if !first || !formatted { return false; }
        let mut dump = Dump { dir, label, id, chunk: Vec::with_capacity(CHUNK), bytes: 0, ordinal: 0, ok: true };
        let formatted = fmt::write(&mut dump, format_args!("{value:?}")).is_ok();
        let flushed = dump.flush().is_ok();
        let complete = formatted && flushed && dump.ok && dump.bytes == hashed.bytes;
        let ended = event(dir, "summary", || json!({"kind":"debug_dump_end","label":label,"dump_id":id,
            "byte_length":hashed.bytes,"dumped_bytes":dump.bytes,"complete":complete}));
        complete && ended
    }).unwrap_or(false)
}

// Pure seams: same legacy flags, including read-error -> empty. No filesystem.
pub fn flags(result: io::Result<String>) -> Vec<bool> {
    result.unwrap_or_default().lines().map(|line| line.ends_with(" c")).collect()
}
pub fn audit(bytes: &[u8]) -> Value {
    let mut records = Vec::new();
    let mut offset = 0;
    for (expected_id, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let complete = line.ends_with(b"\n");
        let text = std::str::from_utf8(line).ok();
        let fields: Vec<_> = text.unwrap_or("").trim_end_matches('\n').split(' ').collect();
        let id = fields.first().and_then(|s| s.parse::<usize>().ok());
        let status = fields.get(1).copied();
        let valid = complete && fields.len() == 2 && id == Some(expected_id) && matches!(status, Some("c" | "a"));
        records.push(json!({"offset":offset,"bytes":line.len(),"id":id,"expected_id":expected_id,
            "status":status,"complete":complete,"valid":valid}));
        offset += line.len();
    }
    let valid = std::str::from_utf8(bytes).is_ok() && records.iter().all(|record| record["valid"] == true);
    json!({"bytes":bytes.len(),"sha256":digest(bytes),"utf8_valid":std::str::from_utf8(bytes).is_ok(),
        "valid":valid,"raw_hex":bytes.iter().map(|byte|format!("{byte:02x}")).collect::<String>(),"records":records})
}
pub fn journal(dir: &Path, stage: &str) {
    if !enabled() { return; }
    best_effort(|| {
        match fs::read(dir.join("steps")) {
            Ok(bytes) => { event(dir, "main", || json!({"kind":"journal","stage":stage,"audit":audit(&bytes)})); }
            Err(error) => { event(dir, "main", || json!({"kind":"journal","stage":stage,
                "read_error":format!("{error:?}"),"original_read_error_maps_to_empty":true})); }
        }
    });
}
fn stream_hash(path: &Path) -> io::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 { break; }
        hash.update(&buffer[..count]); bytes += count as u64;
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}
fn filesystem(path: &Path) -> io::Result<Value> {
    let path = path.canonicalize()?;
    let mut best: Option<(usize, Value)> = None;
    for line in fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let Some((left, right)) = line.split_once(" - ") else { continue; };
        let fields: Vec<_> = left.split_whitespace().collect();
        let details: Vec<_> = right.split_whitespace().collect();
        if fields.len() < 5 || details.len() < 2 { continue; }
        let mount = fields[4].replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\");
        if path.starts_with(&mount) && best.as_ref().is_none_or(|(length, _)| mount.len() > *length) {
            best = Some((mount.len(), json!({"mount_point":mount,"filesystem_type":details[0],"mount_source":details[1]})));
        }
    }
    best.map(|(_, value)| value).ok_or_else(|| io::Error::other("filesystem metadata unavailable"))
}
fn verify_embedded(manifest: &Value) -> io::Result<Value> {
    let changes = manifest["test_only_changes"].as_array().ok_or_else(|| io::Error::other("test changes absent"))?;
    let mut verified = Vec::new();
    for (path, bytes) in [("crates/pg-engine/tests/crash_random.rs", HARNESS_BYTES),
                          ("crates/pg-engine/tests/support/crash14_diagnostic.rs", SUPPORT_BYTES)] {
        let expected = changes.iter().find(|entry| entry["path"] == path)
            .ok_or_else(|| io::Error::other("embedded input not in manifest"))?;
        let actual = digest(bytes);
        if expected["after_sha256"].as_str() != Some(actual.as_str()) || expected["after_bytes"].as_u64() != Some(bytes.len() as u64) {
            return Err(io::Error::other(format!("embedded input mismatch: {path}")));
        }
        verified.push(json!({"path":path,"sha256":actual,"bytes":bytes.len()}));
    }
    if manifest["revision"] != REVISION || manifest["base_commit"] != "278a28966a8921da90c6ead9cfeb3f332306d788" {
        return Err(io::Error::other("manifest revision/base mismatch"));
    }
    Ok(json!(verified))
}
pub fn begin(seeds: &[u64], rounds: u64) -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var_os("CRASH_DIAG_DIR")?);
    install_panic_hook();
    assert_eq!(seeds, &[14]); assert_eq!(rounds, 3); assert!(root.is_absolute());
    let manifest_path = PathBuf::from(std::env::var_os("CRASH_DIAG_SOURCE_MANIFEST").expect("source manifest required"));
    let manifest_bytes = fs::read(&manifest_path).expect("source manifest read");
    let manifest: Value = serde_json::from_slice(&manifest_bytes).expect("source manifest JSON");
    let embedded = verify_embedded(&manifest).expect("compiled test source identity");
    let current_fs = filesystem(root.parent().unwrap()).expect("current filesystem metadata");
    let configured_fs = std::env::var("CRASH_DIAG_FS_TYPE").expect("preverified filesystem type required");
    assert_eq!(current_fs["filesystem_type"].as_str(), Some(configured_fs.as_str()));
    assert!(!matches!(configured_fs.as_str(), "tmpfs" | "ramfs"));
    fs::create_dir(&root).expect("new capture root required");
    fs::create_dir(root.join("events")).expect("events directory");
    let exe = std::env::current_exe().unwrap();
    let (_, binary_sha256) = stream_hash(&exe).unwrap();
    assert_ne!(binary_sha256, "a35916fbfff20bcbe555d5a5c1d77d0688d24686f9696a1148ce2e1c8ec59ec5");
    assert!(event(&root, "summary", || json!({"kind":"instrumented_binary","path":exe,"sha256":binary_sha256,
        "source_manifest":manifest_path,"source_manifest_sha256":digest(&manifest_bytes),"patch_sha256":manifest["patch_sha256"],
        "embedded_inputs":embedded,"configured_filesystem_type":configured_fs,"current_filesystem":current_fs,
        "original_TMPDIR":"/home/meszmate/.cache/a167/tmp","original_filesystem_at_failure":"not captured",
        "same_as_original":false})));
    Some(root)
}
pub fn seed_dir(root: &Path, seed: u64) -> PathBuf {
    let dir = root.join(format!("seed-{seed}")); fs::create_dir(&dir).unwrap(); dir
}
pub fn diagnostic_stop(message: &str) -> ! { panic!("CRASH_DIAGNOSTIC_INCOMPLETE: {message}") }

pub fn prepare_child(seed: u64, round: u64) -> PathBuf {
    if !enabled() { return PathBuf::new(); }
    let path = root().unwrap().join(format!("seed-{seed}-round-{round}-child-state.bin"));
    let mut file = OpenOptions::new().create_new(true).write(true).open(&path).unwrap();
    file.write_all(&[0u8; 4]).unwrap(); file.sync_all().unwrap(); path
}
fn state_write(offset: u64, value: u8) -> io::Result<()> {
    CHILD_STATE.get().ok_or_else(|| io::Error::other("child state absent"))?.write_all_at(&[value], offset)
}
fn kill_self() {
    let me = std::process::id().to_string();
    let _ = std::process::Command::new("kill").args(["-9", &me]).status();
    std::process::abort();
}
fn watchdog(kind: u8) {
    // One byte to a preallocated, preopened sidecar; no event/JSON/fsync.
    // If this fails, abort (different signal), never classify as planned SIGKILL.
    let trigger = if kind == 1 {
        if CHILD_ARMED.load(Ordering::SeqCst) { 2 } else { 1 }
    } else { 3 };
    if state_write(2, trigger).is_err() { std::process::abort(); }
    kill_self();
}
pub fn child_watchdog() {
    if !enabled() { return; }
    install_panic_hook();
    let state = std::env::var_os("CRASH_DIAG_CHILD_STATE").expect("child state path required");
    let file = OpenOptions::new().write(true).open(state).expect("child state open");
    if CHILD_STATE.set(file).is_err() || state_write(0, 1).is_err() { std::process::abort(); }
    std::thread::spawn(|| { std::thread::sleep(Duration::from_secs(300)); watchdog(1); });
}
pub fn arm(dir: &Path, at: usize, delay_us: u64) {
    CHILD_ARMED.store(true, Ordering::SeqCst);
    if state_write(1, 1).is_err() { std::process::abort(); }
    let armed = Instant::now(); let wall_ns = clock_ns();
    // Primary killer has NO diagnostic I/O. Watchdogs have independent state slots.
    std::thread::spawn(move || { std::thread::sleep(Duration::from_micros(delay_us)); kill_self(); });
    std::thread::spawn(|| { std::thread::sleep(Duration::from_secs(5)); watchdog(2); });
    event(dir, "main", || json!({"kind":"kill_armed","at":at,"requested_delay_us":delay_us,
        "armed_wall_ns":wall_ns.to_string(),"elapsed_since_arm_us":armed.elapsed().as_micros().to_string(),"signal_time_unobserved":true}));
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
    serde_json::to_writer_pretty(&mut file, &envelope(value.clone()))?;
    file.write_all(b"\n")?;
    file.sync_all()
}

fn child_state_path(seed: u64, round: u64) -> io::Result<PathBuf> {
    Ok(root()?.join(format!("seed-{seed}-round-{round}-child-state.bin")))
}
fn child_state(seed: u64, round: u64) -> Value {
    match child_state_path(seed, round).and_then(fs::read) {
        Ok(bytes) if bytes.len() == 4 => json!({"started":bytes[0],"armed":bytes[1],
            "watchdog_trigger":bytes[2],"reserved":bytes[3],"valid":bytes[0] <= 1 && bytes[1] <= 1 && bytes[2] <= 3 && bytes[3] == 0,
            "watchdog_before_arm":bytes[2] == 1}),
        Ok(bytes) => json!({"valid":false,"length":bytes.len()}),
        Err(error) => json!({"valid":false,"read_error":format!("{error:?}")}),
    }
}
fn sync_child_markers() -> io::Result<()> {
    let directory = root()?.join("events");
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("channel-closed-") {
            File::open(entry.path())?.sync_all()?;
        }
    }
    File::open(directory)?.sync_all()
}
pub fn record_child_exit(dir: &Path, seed: u64, round: u64, child: &std::process::Output,
                         at: usize, delay_us: u64) -> io::Result<()> {
    if !enabled() { return Ok(()); }
    catch_diagnostic(|| -> io::Result<()> {
        // The original child-status assertion has already been evaluated/caught.
        // Even if it failed, retain output before resuming its SAME panic payload.
        let exit = json!({"kind":"child_exit","seed":seed,"round":round,"status":format!("{:?}",child.status),
            "code":child.status.code(),"signal":child.status.signal(),"original_child_status_oracle_passed":child.status.code().is_none(),
            "child_state":child_state(seed, round),"kill_at":at,"requested_delay_us":delay_us,
            "stdout_hex":child.stdout.iter().map(|byte|format!("{byte:02x}")).collect::<String>(),
            "stderr_hex":child.stderr.iter().map(|byte|format!("{byte:02x}")).collect::<String>(),
            "classification":"signal/status is observational; no exact planned-kill attribution"});
        write_evidence(&root()?.join(format!("seed-{seed}-round-{round}-child-exit.json")), &exit)?;
        File::open(root()?)?.sync_all()?;
        // Child marker updates were unsynced; parent syncs only AFTER exit.
        File::open(child_state_path(seed, round)?)?.sync_all()?;
        sync_child_markers()?;
        event(dir, "summary", || exit);
        Ok(())
    }).unwrap_or_else(|_| Err(io::Error::other("diagnostic child-exit capture panic")))
}
pub fn before_parent_open(dir: &Path, seed: u64, round: u64) -> io::Result<()> {
    if !enabled() { return Ok(()); }
    catch_diagnostic(|| -> io::Result<()> {
        // Exit sidecar is already complete. Oracle failure was resumed before here.
        journal(dir, "after-child-exit-before-parent-open");
        let root = root()?;
        let image = root.join(format!("seed-{seed}-round-{round}-before-parent-open"));
        fs::create_dir(&image)?;
        let mut entries = Vec::new(); let mut total = 0;
        for name in ["data", "steps"] {
            copy_tree(&dir.join(name), &image.join(name), Path::new(name), &mut entries, &mut total)?;
        }
        File::open(&image)?.sync_all()?;
        write_evidence(&root.join(format!("seed-{seed}-round-{round}-image.json")), &json!({
            "seed":seed,"round":round,"copy_complete":true,"captured_before_parent_engine_open":true,
            "never_opened_reference":true,"bytes":total,"entries":entries}))?;
        File::open(root)?.sync_all()?;
        Ok(())
    }).unwrap_or_else(|_| Err(io::Error::other("diagnostic image capture panic")))
}
pub fn validate_child_status(seed: u64, round: u64, child: &std::process::Output) -> io::Result<()> {
    if !enabled() { return Ok(()); }
    let state = child_state(seed, round);
    if state["valid"] != true || state["started"] != 1 || state["armed"] != 1 || state["watchdog_trigger"] != 0 {
        return Err(io::Error::other("watchdog fired, pre-arm exit or state missing/invalid; NOT an intended-kill observation"));
    }
    if child.status.signal() != Some(9) {
        return Err(io::Error::other("original child oracle accepted a signal, but this diagnostic observed a non-SIGKILL exit"));
    }
    Ok(()) // Armed SIGKILL with no recorded watchdog, NOT proof of signal origin.
}

fn query_snapshot(s: &mut Session, dir: &Path, stage: &str, sql: &str) -> bool {
    event(dir, "main", || json!({"kind":"snapshot_query_begin","stage":stage,"sql":sql}));
    let result = catch_diagnostic(|| s.query(sql));
    let ok = matches!(&result, Ok(Ok(_)));
    let retained = match &result {
        Ok(result) => debug_dump(dir, &format!("snapshot:{stage}:{sql}"), result),
        Err(_) => event(dir, "main", || json!({"kind":"snapshot_query_panic","stage":stage,"sql":sql})),
    };
    event(dir, "main", || json!({"kind":"snapshot_query_result","stage":stage,"sql":sql,
        "ok":ok,"result_dump_complete":retained}));
    ok && retained
}
pub fn snapshot(s: &mut Session, dir: &Path, stage: &str) {
    if !enabled() { return; }
    // Called ONLY after an original settle/check panic; never after a passed check.
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
        let tables = catch_diagnostic(|| s.query(sql));
        let listed = match &tables {
            Ok(result) => debug_dump(dir, "snapshot_table_list", result),
            Err(_) => false,
        };
        complete &= listed;
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
    begin_failure(); // A fresh data budget, never spent by prior pass history.
    event(dir, "summary", || json!({"kind":"complete_mismatch","table":table,"how":how,
        "actual_len":got.len(),"expected_len":want.len(),"duplicates":"retained in ordered full row dumps"}));
    debug_dump(dir, &format!("mismatch:{table}:{how}:actual"), got);
    debug_dump(dir, &format!("mismatch:{table}:{how}:expected"), want);
}
