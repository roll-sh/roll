//! Locales and collations across databases: CREATE DATABASE's locale options and errors,
//! each provider's database default (case mapping, ordering, regular expression classes,
//! text search), pg_database's locale columns, collation versions and the connection-time
//! version check. Every `tests/locale/<name>.sql` runs over the wire statement by statement;
//! a `\c <db>` line connects to another database (reporting the connection's notices, or
//! the error refusing it). The output must match `<name>.out`, recorded from PostgreSQL 18.6
//! initialised as the reference is (`--locale=C --encoding=UTF8`):
//!
//!     LOCALE_RECORD=<reference host:port> cargo test -p pg-engine --test locale
//!
//! (user `postgres`, password from LOCALE_PASSWORD or `pw`; LOCALE_FILTER=<substring> runs a
//! subset). The engine side is a new cluster with initdb's defaults. A file's databases
//! are named `locale_*`; the harness drops them before and after each file, on both sides.
//! Mismatches are written to `<name>.actual`.

mod common;

use common::pgclient::PgClient;
use common::{Event, render, statements};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/locale");
    let filter = std::env::var("LOCALE_FILTER").ok();
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

/// A file's text with each `\i <name>` line replaced by `tests/locale/<name>.inc`.
fn source(path: &Path) -> String {
    let src = std::fs::read_to_string(path).unwrap();
    let mut out = String::new();
    for line in src.lines() {
        match line.strip_prefix("\\i ") {
            Some(name) => out.push_str(
                &std::fs::read_to_string(path.with_file_name(format!("{}.inc", name.trim())))
                    .unwrap(),
            ),
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// Drop the `locale_*` databases (what a file made).
fn cleanup(addr: &str, password: &str) {
    let mut c = PgClient::connect(addr, "postgres", password, "postgres").expect("connect");
    let ev = c
        .simple_query("SELECT datname FROM pg_database WHERE datname LIKE 'locale\\_%'")
        .unwrap();
    for e in ev {
        if let Event::Result { rows, .. } = e {
            for r in rows {
                let name = r[0].clone().unwrap_or_default();
                let _ = c.simple_query(&format!("DROP DATABASE \"{name}\" WITH (FORCE)"));
            }
        }
    }
}

/// Run `src` against the server at `addr`: its statements, and `\c` lines as connections.
fn run(addr: &str, password: &str, src: &str) -> String {
    cleanup(addr, password);
    let mut out = String::new();
    let mut client = Some(PgClient::connect(addr, "postgres", password, "postgres").unwrap());
    let mut chunk = String::new();
    let flush = |chunk: &mut String, client: &mut Option<PgClient>, out: &mut String| {
        for s in statements(chunk) {
            let ev = match client.as_mut() {
                Some(c) => c.simple_query(&s).unwrap(),
                None => Vec::new(),
            };
            out.push_str(&render(&s, &ev));
        }
        chunk.clear();
    };
    for line in src.lines() {
        if let Some(db) = line.strip_prefix("\\c ") {
            flush(&mut chunk, &mut client, &mut out);
            let (c, ev) =
                PgClient::connect_events(addr, "postgres", password, db.trim()).unwrap();
            let mut shown = render(line, &ev);
            if c.is_none() {
                let _ = writeln!(shown, "(not connected)");
            }
            out.push_str(&shown);
            client = c;
        } else {
            chunk.push_str(line);
            chunk.push('\n');
        }
    }
    flush(&mut chunk, &mut client, &mut out);
    drop(client);
    cleanup(addr, password);
    out
}

#[test]
fn locale() {
    let files = files();
    if let Ok(addr) = std::env::var("LOCALE_RECORD") {
        let pw = std::env::var("LOCALE_PASSWORD").unwrap_or_else(|_| "pw".into());
        for f in &files {
            let out = run(&addr, &pw, &source(f));
            std::fs::write(f.with_extension("out"), out).unwrap();
            eprintln!("recorded {}", f.display());
        }
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let opts = pg_engine::engine::EngineOptions {
        data_dir: dir.path().to_path_buf(),
        cache_bytes: 8 << 20,
        memtable_bytes: 2 << 20,
        sync: false,
        idle_close: std::time::Duration::from_millis(200),
        password: Some("pw".into()),
        ..Default::default()
    };
    let engine = pg_engine::engine::Engine::open(opts).unwrap();
    let server = pg_engine::server::ServerOptions {
        auth: pg_engine::server::AuthMethod::ScramSha256,
    };
    let listen = format!("{}:0", std::net::Ipv4Addr::LOCALHOST);
    let addr = pg_engine::server::spawn(engine.clone(), &listen, server)
        .unwrap()
        .to_string();
    let mut failures = Vec::new();
    for f in &files {
        let expected = std::fs::read_to_string(f.with_extension("out"))
            .unwrap_or_else(|_| panic!("{}: no expected output (record it)", f.display()));
        let actual = run(&addr, "pw", &source(f));
        let actual_path = f.with_extension("actual");
        if actual == expected {
            let _ = std::fs::remove_file(&actual_path);
        } else {
            std::fs::write(&actual_path, &actual).unwrap();
            let line = expected
                .lines()
                .zip(actual.lines())
                .position(|(a, b)| a != b)
                .unwrap_or(expected.lines().count().min(actual.lines().count()));
            failures.push(format!("{} (first difference at line {})", f.display(), line + 1));
        }
    }
    engine.shutdown();
    assert!(failures.is_empty(), "locale files differ:\n{}", failures.join("\n"));
}
