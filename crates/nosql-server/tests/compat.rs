//! Differential compatibility tests: every script in `tests/compat/*.cql` runs against roll (a
//! fresh database per script and dialect) and against the reference of that dialect, and the
//! raw results must be identical: result kind, column specs (keyspace, table, name, type), rows
//! (serialised cells), paging state presence, schema change bodies, warnings, and error code
//! and message. Same pattern as `redis-server/tests/compat.rs`.
//!
//! Script syntax (statements end with `;` and may span lines):
//!
//! ```text
//! -- comment
//! SELECT …;                       compare exactly
//! !unordered SELECT …;            rows compared as a multiset
//! !mask a,b SELECT …;             the values of columns a and b are not compared (now(), ids)
//! !skip …;                        run on both, don't compare
//! !error …;                       both fail with the same code (messages may differ)
//! !nowarn …;                      warnings are not compared
//! !todo …;                        a known difference (tracked in ENGINE-REQUESTS.md): reported,
//!                                 not failing — but failing once it matches, so the list shrinks
//! !scylla …;  !cassandra …;       only in that dialect (combine: `!scylla !unordered …`)
//! @version 3|4|5                  reconnect both sides at that protocol version (ScyllaDB has
//!                                 no v5: it stays on the version it has)
//! @page 10                        page size of the following statements (0: unpaged)
//! ```
//!
//! `{ks}` expands to the script's keyspace (`srv_c_<script>`), dropped before and after on both
//! sides.
//!
//! References: `ROLL_NOSQL_REF_SCYLLA=127.0.0.1:19042` and `ROLL_NOSQL_REF_CASSANDRA=127.0.0.1:29042`
//! (the shared containers; see docs/nosql/contracts.md). A dialect without its variable is
//! skipped. `ROLL_COMPAT_FILTER=ddl` runs matching scripts only; `ROLL_COMPAT_TRACE=1` prints each
//! statement before it runs.

mod common;

use std::path::PathBuf;

use common::*;
use nosql_engine::Dialect;
use nosql_proto::client::{Client, ClientConfig, ClientError};
use nosql_proto::{QueryParams, Query, Request, RequestFrame, Response, ResultBody, Version};

#[derive(Default, Clone)]
struct Directives {
    unordered: bool,
    mask: Vec<String>,
    skip: bool,
    error: bool,
    nowarn: bool,
    todo: bool,
    only: Option<Dialect>,
}

enum Line {
    Statement(String, Directives),
    Version(Version),
    Page(i32),
}

fn parse(script: &str) -> Vec<(usize, Line)> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut start = 0;
    for (n, raw) in script.lines().enumerate() {
        let line = raw.trim();
        if buf.is_empty() {
            if line.is_empty() || line.starts_with("--") {
                continue;
            }
            if let Some(v) = line.strip_prefix("@version ") {
                let v = match v.trim() {
                    "3" => Version::V3,
                    "4" => Version::V4,
                    "5" => Version::V5,
                    other => panic!("line {}: bad version {other}", n + 1),
                };
                out.push((n + 1, Line::Version(v)));
                continue;
            }
            if let Some(p) = line.strip_prefix("@page ") {
                out.push((n + 1, Line::Page(p.trim().parse().unwrap())));
                continue;
            }
            start = n + 1;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(raw);
        if line.ends_with(';') {
            let (stmt, d) = directives(buf.trim());
            out.push((start, Line::Statement(stmt, d)));
            buf.clear();
        }
    }
    assert!(buf.trim().is_empty(), "unterminated statement at line {start}");
    out
}

fn directives(mut s: &str) -> (String, Directives) {
    let mut d = Directives::default();
    while let Some(rest) = s.strip_prefix('!') {
        let (word, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        let mut tail = tail.trim_start();
        match word {
            "unordered" => d.unordered = true,
            "skip" => d.skip = true,
            "error" => d.error = true,
            "nowarn" => d.nowarn = true,
            "todo" => d.todo = true,
            "scylla" => d.only = Some(Dialect::Scylla),
            "cassandra" => d.only = Some(Dialect::Cassandra),
            "mask" => {
                let (cols, t) = tail.split_once(char::is_whitespace).unwrap_or((tail, ""));
                d.mask = cols.split(',').map(str::to_string).collect();
                tail = t.trim_start();
            }
            other => panic!("unknown directive !{other}"),
        }
        s = tail;
    }
    // The statement goes without its final `;` (both servers accept either).
    (s.trim_end_matches(';').trim().to_string(), d)
}

/// What a statement produced, as compared.
#[derive(Debug, PartialEq)]
enum Outcome {
    Rows { columns: Vec<String>, rows: Vec<Vec<Option<String>>>, more_pages: bool, warnings: Vec<String> },
    Other { result: String, warnings: Vec<String> },
    Error { code: i32, message: String, warnings: Vec<String> },
    Closed(String),
}

async fn run(c: &Client, stmt: &str, page: i32, d: &Directives) -> Outcome {
    let params = QueryParams { page_size: (page > 0).then_some(page), ..Default::default() };
    let rf = RequestFrame::new(0, Request::Query(Query { query: stmt.to_string(), params }));
    let resp = match c.send(rf).await {
        Ok(r) => r,
        Err(ClientError::Server(e)) => return Outcome::Error { code: e.code, message: e.message, warnings: vec![] },
        Err(e) => return Outcome::Closed(e.to_string()),
    };
    let warnings = if d.nowarn { vec![] } else { resp.warnings.clone() };
    match resp.response {
        Response::Error(e) => Outcome::Error { code: e.code, message: if d.error { String::new() } else { e.message }, warnings },
        Response::Result(ResultBody::Rows(r)) => {
            let columns: Vec<String> = r.metadata.columns.iter().map(|c| format!("{}.{}.{} {:?}", c.keyspace, c.table, c.name, c.ty)).collect();
            let names: Vec<&str> = r.metadata.columns.iter().map(|c| c.name.as_str()).collect();
            let mut rows: Vec<Vec<Option<String>>> = r
                .rows()
                .map(|row| {
                    row.enumerate()
                        .map(|(i, cell)| {
                            if names.get(i).is_some_and(|n| d.mask.iter().any(|m| m == n)) {
                                return Some("<masked>".into());
                            }
                            cell.map(|b| b.iter().map(|x| format!("{x:02x}")).collect())
                        })
                        .collect()
                })
                .collect();
            if d.unordered {
                rows.sort();
            }
            Outcome::Rows { columns, rows, more_pages: r.metadata.paging_state.is_some(), warnings }
        }
        Response::Result(other) => Outcome::Other { result: format!("{other:?}"), warnings },
        other => Outcome::Other { result: format!("{other:?}"), warnings },
    }
}

async fn client(addr: &str, v: Version) -> Client {
    let mut cfg = ClientConfig::new(addr).version(v);
    cfg.request_timeout = Some(std::time::Duration::from_secs(60));
    Client::connect(cfg).await.unwrap_or_else(|e| panic!("connect {addr}: {e}"))
}

async fn script(dialect: Dialect, reference: &str, name: &str, text: &str) -> Vec<String> {
    let ks = format!("srv_c_{name}");
    let text = text.replace("{ks}", &ks);
    let s = single(dialect).await;
    let roll_addr = s.addr.to_string();
    let mut v = Version::V4;
    let mut roll = client(&roll_addr, v).await;
    let mut refc = client(reference, v).await;
    let drop = format!("DROP KEYSPACE IF EXISTS {ks}");
    let _ = refc.query(&drop, QueryParams::default()).await;
    let trace = std::env::var("ROLL_COMPAT_TRACE").is_ok();
    let mut page = 0;
    let mut diffs = Vec::new();
    for (line, l) in parse(&text) {
        match l {
            Line::Version(nv) => {
                if dialect == Dialect::Scylla && nv >= Version::V5 {
                    continue;
                }
                v = nv;
                roll = client(&roll_addr, v).await;
                refc = client(reference, v).await;
            }
            Line::Page(p) => page = p,
            Line::Statement(stmt, d) => {
                if d.only.is_some_and(|o| o != dialect) {
                    continue;
                }
                if trace {
                    eprintln!("{name}:{line} [{dialect:?}] {stmt}");
                }
                let got = run(&roll, &stmt, page, &d).await;
                let want = run(&refc, &stmt, page, &d).await;
                if d.skip {
                    continue;
                }
                if d.todo {
                    if got == want {
                        diffs.push(format!("{name}.cql:{line} [{dialect:?}] matches now: remove its !todo"));
                    } else {
                        eprintln!("compat todo {name}.cql:{line} [{dialect:?}] {stmt}\n  roll: {got:?}\n  ref:  {want:?}");
                    }
                } else if got != want {
                    diffs.push(format!("{name}.cql:{line} [{dialect:?}] {stmt}\n  roll: {got:?}\n  ref:  {want:?}"));
                }
            }
        }
    }
    let _ = refc.query(&drop, QueryParams::default()).await;
    diffs
}

#[tokio::test]
async fn compat() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/compat");
    let filter = std::env::var("ROLL_COMPAT_FILTER").ok();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "cql")).collect();
    files.sort();
    let refs = [(Dialect::Scylla, "ROLL_NOSQL_REF_SCYLLA"), (Dialect::Cassandra, "ROLL_NOSQL_REF_CASSANDRA")];
    let mut diffs = Vec::new();
    let mut ran = 0;
    for (dialect, var) in refs {
        let Ok(reference) = std::env::var(var) else {
            eprintln!("compat: {var} not set, {dialect:?} skipped");
            continue;
        };
        for f in &files {
            let name = f.file_stem().unwrap().to_string_lossy().into_owned();
            if filter.as_deref().is_some_and(|x| !name.contains(x)) {
                continue;
            }
            let text = std::fs::read_to_string(f).unwrap();
            diffs.extend(script(dialect, &reference, &name, &text).await);
            ran += 1;
        }
    }
    eprintln!("compat: {ran} script runs, {} differences", diffs.len());
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
}

#[test]
fn script_parsing() {
    let p = parse("-- c\n@version 3\n@page 5\nSELECT 1\n  FROM t;\n!scylla !mask a,b !unordered SELECT a;\n!error X;\n");
    assert_eq!(p.len(), 5);
    assert!(matches!(p[0].1, Line::Version(Version::V3)));
    assert!(matches!(p[1].1, Line::Page(5)));
    let Line::Statement(s, d) = &p[2].1 else { panic!() };
    assert_eq!((s.as_str(), p[2].0), ("SELECT 1\n  FROM t", 4));
    assert!(!d.unordered);
    let Line::Statement(s, d) = &p[3].1 else { panic!() };
    assert_eq!(s, "SELECT a");
    assert_eq!((d.only, d.mask.clone(), d.unordered), (Some(Dialect::Scylla), vec!["a".to_string(), "b".to_string()], true));
    let Line::Statement(_, d) = &p[4].1 else { panic!() };
    assert!(d.error);
}
