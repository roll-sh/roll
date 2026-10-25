//! Golden SQL tests. Every `tests/sql/<name>.sql` runs statement by statement; the output
//! (rows with column types, command tags, notices, errors with SQLSTATE/detail/hint) must
//! match `tests/sql/<name>.out`, which is recorded from real Postgres 18:
//!
//!     GOLDEN_RECORD=127.0.0.1:55499 cargo test -p pg-engine --test golden
//!
//! (user `postgres`, password from GOLDEN_PASSWORD or `pw`). GOLDEN_FILTER=<substring> runs
//! a subset. On mismatch the engine's output is written next to the expected file as
//! `<name>.actual` (gitignored).
//!
//! `golden_wire` runs the same files through the wire protocol server (SCRAM, a database
//! per file made with CREATE DATABASE); its mismatches go to `<name>.wire.actual`.

mod common;

use common::pgclient::PgClient;
use common::{
    engine_fcall, engine_query, engine_session, open_store, parse_fcall, render, set_raw_order,
    statements, wants_raw_order,
};
use std::path::{Path, PathBuf};

fn sql_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sql");
    let filter = std::env::var("GOLDEN_FILTER").ok();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "sql"))
        .filter(|p| {
            filter
                .as_deref()
                .is_none_or(|f| p.file_stem().unwrap().to_string_lossy().contains(f))
        })
        .collect();
    files.sort();
    files
}

fn db_name(path: &Path) -> String {
    format!(
        "golden_{}",
        path.file_stem()
            .unwrap()
            .to_string_lossy()
            .replace('-', "_")
    )
}

fn record(addr: &str, path: &Path) -> String {
    let password = std::env::var("GOLDEN_PASSWORD").unwrap_or_else(|_| "pw".into());
    let db = db_name(path);
    {
        let mut admin = PgClient::connect(addr, "postgres", &password, "postgres")
            .expect("connect to reference Postgres");
        admin
            .simple_query(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"))
            .unwrap();
        admin.simple_query(&format!("CREATE DATABASE {db} TEMPLATE template0 LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LOCALE 'C' ENCODING 'UTF8'")).unwrap();
    }
    let src = std::fs::read_to_string(path).unwrap();
    set_raw_order(wants_raw_order(&src));
    let mut out = String::new();
    {
        let mut c = PgClient::connect(addr, "postgres", &password, &db).unwrap();
        for s in statements(&src) {
            let ev = match parse_fcall(&s) {
                Some(fc) => c.function_call(&fc).unwrap(),
                None => c.simple_query(&s).unwrap(),
            };
            out.push_str(&render(&s, &ev));
        }
    }
    let mut admin = PgClient::connect(addr, "postgres", &password, "postgres").unwrap();
    admin
        .simple_query(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"))
        .unwrap();
    out
}

fn run_engine(path: &Path) -> String {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    let mut s = engine_session(store, &db_name(path));
    let src = std::fs::read_to_string(path).unwrap();
    set_raw_order(wants_raw_order(&src));
    let mut out = String::new();
    for stmt in statements(&src) {
        let ev = match parse_fcall(&stmt) {
            Some(fc) => engine_fcall(&mut s, &fc),
            None => engine_query(&mut s, &stmt),
        };
        out.push_str(&render(&stmt, &ev));
    }
    out
}

/// First differing region, with a little context.
fn diff(expected: &str, actual: &str) -> String {
    let e: Vec<&str> = expected.lines().collect();
    let a: Vec<&str> = actual.lines().collect();
    let first = e
        .iter()
        .zip(&a)
        .position(|(x, y)| x != y)
        .unwrap_or(e.len().min(a.len()));
    // The statement the difference is in.
    let start = (0..=first.min(e.len().saturating_sub(1)))
        .rev()
        .find(|i| e.get(*i).is_some_and(|l| l.starts_with("> ")))
        .unwrap_or(0);
    let mut out = String::new();
    for i in start..(first + 8).min(e.len().max(a.len())) {
        let x = e.get(i).copied().unwrap_or("<eof>");
        let y = a.get(i).copied().unwrap_or("<eof>");
        if x == y {
            out.push_str(&format!("   {x}\n"));
        } else {
            out.push_str(&format!(" - {x}\n + {y}\n"));
        }
    }
    out
}

#[test]
fn golden() {
    // Sessions get a big stack, as the server gives its session threads.
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            pg_engine::stack::set_limit(60 << 20);
            golden_inner()
        })
        .unwrap()
        .join()
        .unwrap();
}

fn golden_inner() {
    let files = sql_files();
    if let Ok(addr) = std::env::var("GOLDEN_RECORD") {
        for f in &files {
            let out = record(&addr, f);
            std::fs::write(f.with_extension("out"), out).unwrap();
            eprintln!("recorded {}", f.display());
        }
        return;
    }
    compare(&files, "actual", run_engine);
}

/// Run every file through `run`, comparing with the recorded output.
fn compare(files: &[PathBuf], suffix: &str, mut run: impl FnMut(&Path) -> String) {
    let mut failures = Vec::new();
    let mut passed = 0;
    for f in files {
        let expected_path = f.with_extension("out");
        let Ok(expected) = std::fs::read_to_string(&expected_path) else {
            failures.push(format!(
                "{}: no expected output (record it with GOLDEN_RECORD)",
                f.display()
            ));
            continue;
        };
        let actual = run(f);
        let actual_path = f.with_extension(suffix);
        if actual == expected {
            let _ = std::fs::remove_file(&actual_path);
            passed += 1;
        } else {
            std::fs::write(&actual_path, &actual).unwrap();
            failures.push(format!(
                "{}:\n{}",
                f.file_name().unwrap().to_string_lossy(),
                diff(&expected, &actual)
            ));
        }
    }
    if !failures.is_empty() {
        panic!(
            "{} of {} golden files differ:\n\n{}",
            failures.len(),
            files.len(),
            failures.join("\n")
        );
    }
    eprintln!("{passed} golden files match");
}

#[test]
fn golden_wire() {
    if std::env::var("GOLDEN_RECORD").is_ok() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let opts = pg_engine::engine::EngineOptions {
        data_dir: dir.path().to_path_buf(),
        cache_bytes: 8 << 20,
        memtable_bytes: 2 << 20,
        sync: false,
        idle_close: std::time::Duration::from_millis(200),
        superuser: "postgres".into(),
        password: Some("pw".into()),
        server_files: None,
        shared_preload_libraries: Vec::new(),
    };
    let engine = pg_engine::engine::Engine::open(opts).unwrap();
    let server = pg_engine::server::ServerOptions {
        auth: pg_engine::server::AuthMethod::ScramSha256,
    };
    let addr = pg_engine::server::spawn(engine.clone(), "127.0.0.1:0", server)
        .unwrap()
        .to_string();
    compare(&sql_files(), "wire.actual", |f| run_wire(&addr, f));
    engine.shutdown();
}

fn run_wire(addr: &str, path: &Path) -> String {
    let db = db_name(path);
    {
        let mut admin =
            PgClient::connect(addr, "postgres", "pw", "postgres").expect("connect to the engine");
        admin
            .simple_query(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"))
            .unwrap();
        admin
            .simple_query(&format!("CREATE DATABASE {db} TEMPLATE template0"))
            .unwrap();
    }
    let src = std::fs::read_to_string(path).unwrap();
    set_raw_order(wants_raw_order(&src));
    let mut out = String::new();
    {
        let mut c = PgClient::connect(addr, "postgres", "pw", &db).unwrap();
        for s in statements(&src) {
            let ev = match parse_fcall(&s) {
                Some(fc) => c.function_call(&fc).unwrap(),
                None => c.simple_query(&s).unwrap(),
            };
            out.push_str(&render(&s, &ev));
        }
    }
    let mut admin = PgClient::connect(addr, "postgres", "pw", "postgres").unwrap();
    admin
        .simple_query(&format!("DROP DATABASE {db} WITH (FORCE)"))
        .unwrap();
    out
}

/// Run `f` on a thread with a session's stack (as `golden` does).
fn on_session_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            pg_engine::stack::set_limit(60 << 20);
            f()
        })
        .unwrap()
        .join()
        .unwrap()
}

/// The goldens again, in a shuffled order on one thread that keeps running file after file
/// (each in a fresh database), so each file runs after other files' statements than the ones
/// sorting before it. State outliving a statement (thread-local caches, process-wide caches
/// keyed by oids, which every fresh database hands out again) must not change any output: a
/// file that matches in `golden` and differs here depends on what ran before it.
///
/// `GOLDEN_SHUFFLE`: unset runs a sample of `SHUFFLE_SAMPLE` files, `all` every file, a number
/// that many, `off` none. `GOLDEN_SHUFFLE_SEED` picks the order and sample (default: a fixed
/// seed, so a failure reproduces; set it to try others). `GOLDEN_FILTER` narrows the files as
/// in `golden`. A difference is reported with the files that ran just before it, and whether
/// the file matches when run alone on a fresh thread.
#[test]
fn golden_shuffled() {
    const SHUFFLE_SAMPLE: usize = 60;
    if std::env::var("GOLDEN_RECORD").is_ok() {
        return;
    }
    let n = match std::env::var("GOLDEN_SHUFFLE").unwrap_or_default().as_str() {
        "off" | "0" => return,
        "all" => usize::MAX,
        "" => SHUFFLE_SAMPLE,
        s => s.parse().expect("GOLDEN_SHUFFLE: off, all or a number"),
    };
    let seed: u64 = std::env::var("GOLDEN_SHUFFLE_SEED")
        .ok()
        .map(|s| s.parse().expect("GOLDEN_SHUFFLE_SEED: a number"))
        .unwrap_or(0xa13);
    let mut files = sql_files();
    // Fisher-Yates over splitmix64.
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    for i in (1..files.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        files.swap(i, j);
    }
    files.truncate(n);
    let expected = |f: &Path| std::fs::read_to_string(f.with_extension("out")).unwrap_or_default();
    let order = files.clone();
    let differing: Vec<(PathBuf, String)> = on_session_thread(move || {
        let mut out = Vec::new();
        for f in order {
            let (want, got) = (expected(&f), run_engine(&f));
            if got != want {
                let d = diff(&want, &got);
                out.push((f, d));
            }
        }
        out
    });
    let mut failures = Vec::new();
    for (f, d) in differing {
        let pos = files.iter().position(|x| *x == f).unwrap();
        let before: Vec<String> = files[pos.saturating_sub(5)..pos]
            .iter()
            .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
            .collect();
        let alone = {
            let f = f.clone();
            on_session_thread(move || run_engine(&f) == expected(&f))
        };
        failures.push(format!(
            "{} (file {} of the order, after {}; {}):\n{d}",
            f.file_name().unwrap().to_string_lossy(),
            pos + 1,
            before.join(", "),
            if alone {
                "matches alone on a fresh thread"
            } else {
                "differs alone too"
            },
        ));
    }
    if !failures.is_empty() {
        panic!(
            "GOLDEN_SHUFFLE_SEED={seed}: {} of {} golden files differ in this order:\n\n{}",
            failures.len(),
            files.len(),
            failures.join("\n")
        );
    }
    eprintln!("{} golden files match in a shuffled order (seed {seed})", files.len());
}
