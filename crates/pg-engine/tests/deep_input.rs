//! Deep input never crashes a host thread: long operator chains, nested parentheses, function
//! calls, subqueries, CASE, JSON, jsonpath, regex, XML and recursive functions run through the
//! embedded API on a plain 2 MB thread and on the test's own thread. Each either computes what
//! Postgres 18.6 computes or fails with Postgres's error (stack depth limit exceeded with its
//! HINT, or the grammar's own limits), as `tests/deep_input.out` records from Postgres:
//!
//!     DEEP_RECORD=<host:port> cargo test -p pg-engine --test deep_input -- --nocapture
//!
//! (user `postgres`, password from GOLDEN_PASSWORD or `pw`). DEEP_FILTER=<shape> runs one
//! shape. Sizes are below and well above Postgres's boundary with max_stack_depth = 2MB; a
//! release build also runs `RELEASE_CASES`, recursive functions just below and above it.

mod common;

use common::pgclient::PgClient;
use common::{Event, engine_query, engine_session, open_store};
use std::fmt::Write;

/// (shape, size): sizes Postgres computes, then sizes where it fails.
const CASES: &[(&str, usize)] = &[
    ("plus", 400),
    ("plus", 4000),
    ("plus", 100_000),
    ("concat", 4000),
    ("concat", 50_000),
    ("parens", 9900),
    ("parens", 10_200),
    ("func", 4000),
    ("func", 50_000),
    // (libpg_query hands the parse tree over as protobuf, at most 10,000 messages deep: deeper
    // subqueries (about 1,650) and NOT chains (about 4,900) fail here where Postgres goes
    // on to 2,176 and 8,064.)
    ("subq", 1500),
    ("subq", 20_000),
    ("fromsubq", 1600),
    ("fromsubq", 1700),
    ("case", 1900),
    ("case", 2100),
    ("and", 50_000),
    ("or", 50_000),
    ("not", 4000),
    ("not", 50_000),
    ("neg", 9900),
    ("neg", 10_200),
    ("row", 4900),
    ("row", 5100),
    ("arrayc", 6),
    ("arrayc", 7),
    ("arrlit", 6),
    ("arrlit", 7),
    ("union", 7000),
    ("union", 30_000),
    ("json", 21_000),
    ("json", 30_000),
    ("jsonb", 18_000),
    ("jsonb", 30_000),
    ("jsonobj", 16_000),
    ("jsonobj", 30_000),
    ("jsonobjt", 16_000),
    ("jsonobjt", 30_000),
    ("json100k", 10_000),
    ("jsonpath", 9900),
    ("jsonpath", 10_200),
    ("jsonpathplus", 13_000),
    ("jsonpathplus", 40_000),
    // (The regex engine emulates the frames of the reference's build for this architecture:
    // 8,710 nested groups on x86-64. XML's limit is libxml2's 256.)
    ("regex", 5000),
    ("regex", 8700),
    ("regex", 8720),
    ("regex", 9000),
    ("xml", 256),
    ("xml", 300),
    ("plpgsql", 900),
    ("plpgsql", 2000),
    // Recursive SQL functions (more near Postgres's boundaries in `RELEASE_CASES`).
    ("sqlfn", 1000),
    ("sqlfn", 20_000),
    ("sqlfnnoinline", 300),
    ("sqlfnnoinline", 20_000),
    ("sqlfnmutual", 1000),
    ("sqlfnpoly", 1000),
    ("sqlfnsubq", 300),
    ("sqlfnsubq", 20_000),
    ("sqlfnpl", 300),
    ("sqlfnsrf", 300),
    ("sqlfnsrf", 20_000),
    ("sqlfnsrfinline", 100),
    ("colplus", 4000),
    ("colplus", 100_000),
    ("viewdef", 300),
    ("viewdef", 2000),
    ("explain", 2000),
    ("deepjsonb", 18_000),
    ("setting", 0),
];

/// Cases a release build runs too: recursive functions near Postgres's boundaries, where a
/// call level is a call (a debug build's frames are five times bigger, and its stack budget
/// stops them first). A SQL function's call that isn't inlined takes 498 bytes of
/// Postgres's stack: 8,422 levels of `sqlfn`, every other one inlined.
const RELEASE_CASES: &[(&str, usize)] = &[
    ("sqlfn", 8000),
    ("sqlfn", 9000),
    ("sqlfnnoinline", 4000),
    ("sqlfnnoinline", 4500),
    ("sqlfnmutual", 12_000),
    ("sqlfnmutual", 13_000),
    ("sqlfnpoly", 8000),
    ("sqlfnpoly", 9000),
    ("sqlfnsubq", 2300),
    ("sqlfnsubq", 2600),
    ("sqlfnpl", 900),
    ("sqlfnpl", 1000),
    ("sqlfnsrf", 3000),
    ("sqlfnsrf", 3100),
    ("plpgsql", 925),
    ("plpgsql", 940),
];

fn statements_of(shape: &str, n: usize) -> Vec<String> {
    let rep = |s: &str| s.repeat(n);
    let join = |s: &str, sep: &str| vec![s; n].join(sep);
    let one = match shape {
        "plus" => format!("SELECT {}", join("1", "+")),
        "concat" => format!("SELECT length({})", join("'a'", "||")),
        "parens" => format!("SELECT {}1{}", rep("("), rep(")")),
        "func" => format!("SELECT {}1{}", rep("abs("), rep(")")),
        "subq" => format!("SELECT {}1{}", rep("(SELECT "), rep(")")),
        "fromsubq" => format!(
            "SELECT * FROM {}(SELECT 1 AS a) s0{}",
            rep("(SELECT * FROM "),
            rep(") s")
        ),
        "case" => format!("SELECT {}1{}", rep("CASE WHEN true THEN "), rep(" END")),
        "and" | "or" => {
            let (op, sep) = if shape == "and" { ("<>", " AND ") } else { ("=", " OR ") };
            let terms: Vec<String> = (2..n + 2).map(|i| format!("x{op}{i}")).collect();
            format!("SELECT count(*) FROM (SELECT 1 AS x) t WHERE {}", terms.join(sep))
        }
        "not" => format!("SELECT {}true", rep("NOT ")),
        "neg" => format!("SELECT {}1", rep("- ")),
        "row" => format!("SELECT {}1{} IS NOT NULL", rep("ROW("), rep(")")),
        "arrayc" => format!("SELECT {}1{}", rep("ARRAY["), rep("]")),
        "arrlit" => format!("SELECT ('{}1{}'::int[]) IS NULL", rep("{"), rep("}")),
        "union" => format!("SELECT count(*) FROM ({}) u", join("SELECT 1", " UNION ALL ")),
        "json" => format!("SELECT length((repeat('[', {n}) || repeat(']', {n}))::json::text)"),
        "jsonb" => format!("SELECT jsonb_typeof((repeat('[', {n}) || repeat(']', {n}))::jsonb)"),
        "jsonobj" => format!(
            "SELECT jsonb_typeof((repeat('{{\"a\":', {n}) || '1' || repeat('}}', {n}))::jsonb)"
        ),
        "jsonobjt" => format!(
            "SELECT length((repeat('{{\"a\":', {n}) || '1' || repeat('}}', {n}))::json::text)"
        ),
        "json100k" => {
            return vec![
                "SET max_stack_depth = '100kB'".into(),
                format!("SELECT repeat('[', {n})::json"),
                format!("SELECT repeat('{{\"a\":', {n})::jsonb"),
                "RESET max_stack_depth".into(),
            ];
        }
        "jsonpath" => format!("SELECT length((repeat('(', {n}) || '$' || repeat(')', {n}))::jsonpath::text)"),
        "jsonpathplus" => format!("SELECT length(('$' || repeat(' + 1', {n}))::jsonpath::text)"),
        "regex" => format!("SELECT 'a' ~ ('{}a{}')", rep("("), rep(")")),
        "xml" => format!("SELECT length((repeat('<a>', {n}) || repeat('</a>', {n}))::xml::text)"),
        "plpgsql" => {
            return vec![
                "CREATE OR REPLACE FUNCTION deep_r(n int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN IF n = 0 THEN RETURN 0; END IF; RETURN deep_r(n - 1) + 1; END $$".into(),
                format!("SELECT deep_r({n})"),
            ];
        }
        "colplus" => format!("SELECT {} FROM (VALUES (1)) t(x)", join("x", "+")),
        "viewdef" => {
            return vec![
                format!("CREATE VIEW deep_v AS SELECT {} AS s FROM (VALUES (1)) t(x)", join("x", "+")),
                "SELECT s, length(pg_get_viewdef('deep_v')) FROM deep_v".into(),
                "DROP VIEW deep_v".into(),
            ];
        }
        "explain" => format!(
            "EXPLAIN (VERBOSE, COSTS OFF) SELECT {} FROM (VALUES (1)) t(x)",
            join("x", "+")
        ),
        "deepjsonb" => {
            // A deep jsonb value stored, then read back, printed, compared, hashed, copied and
            // dropped: all of which recurse its depth.
            return vec![
                format!("CREATE TABLE deep_j AS SELECT (repeat('[', {n}) || repeat(']', {n}))::jsonb AS v"),
                "SELECT length(v::text), v = v, jsonb_typeof(v), count(DISTINCT v) FROM deep_j GROUP BY v".into(),
                "DROP TABLE deep_j".into(),
            ];
        }
        "sqlfn" => {
            return vec![
                "CREATE OR REPLACE FUNCTION deep_s(n int) RETURNS int LANGUAGE sql AS $$ SELECT CASE WHEN n = 0 THEN 0 ELSE deep_s(n - 1) + 1 END $$".into(),
                format!("SELECT deep_s({n})"),
            ];
        }
        // (Not inlinable: it has a FROM clause.)
        "sqlfnnoinline" => {
            return vec![
                "CREATE OR REPLACE FUNCTION deep_sn(n int) RETURNS int LANGUAGE sql AS $$ SELECT CASE WHEN n = 0 THEN 0 ELSE deep_sn(n - 1) + 1 END FROM (VALUES (1)) v $$".into(),
                format!("SELECT deep_sn({n})"),
            ];
        }
        "sqlfnmutual" => {
            return vec![
                "CREATE OR REPLACE FUNCTION deep_ev(n int) RETURNS bool LANGUAGE sql AS $$ SELECT true $$".into(),
                "CREATE OR REPLACE FUNCTION deep_od(n int) RETURNS bool LANGUAGE sql AS $$ SELECT CASE WHEN n = 0 THEN false ELSE deep_ev(n - 1) END $$".into(),
                "CREATE OR REPLACE FUNCTION deep_ev(n int) RETURNS bool LANGUAGE sql AS $$ SELECT CASE WHEN n = 0 THEN true ELSE deep_od(n - 1) END $$".into(),
                format!("SELECT deep_ev({n}), deep_od({n})"),
            ];
        }
        "sqlfnpoly" => {
            return vec![
                "CREATE OR REPLACE FUNCTION deep_sp(n int, x anyelement) RETURNS anyelement LANGUAGE sql AS $$ SELECT CASE WHEN n = 0 THEN x ELSE deep_sp(n - 1, x) END $$".into(),
                format!("SELECT deep_sp({n}, 'a'::text), deep_sp({n}, 2.5)"),
            ];
        }
        "sqlfnsubq" => {
            return vec![
                "CREATE OR REPLACE FUNCTION deep_sq(n int) RETURNS int LANGUAGE sql AS $$ SELECT n + coalesce((SELECT deep_sq(n - 1) FROM (VALUES (1)) v WHERE n > 0), 0) $$".into(),
                format!("SELECT deep_sq({n})"),
            ];
        }
        // (SQL calling PL/pgSQL calling SQL: the SQL function is inlined into each PL/pgSQL
        // statement.)
        "sqlfnpl" => {
            return vec![
                "CREATE OR REPLACE FUNCTION deep_ss(n int) RETURNS int LANGUAGE sql AS $$ SELECT 0 $$".into(),
                "CREATE OR REPLACE FUNCTION deep_pp(n int) RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN deep_ss(n); END $$".into(),
                "CREATE OR REPLACE FUNCTION deep_ss(n int) RETURNS int LANGUAGE sql AS $$ SELECT CASE WHEN n = 0 THEN 0 ELSE deep_pp(n - 1) + 1 END $$".into(),
                format!("SELECT deep_ss({n})"),
            ];
        }
        "sqlfnsrf" => {
            return vec![
                "CREATE OR REPLACE FUNCTION deep_srf(n int) RETURNS SETOF int LANGUAGE sql AS $$ SELECT n UNION ALL SELECT * FROM deep_srf(n - 1) WHERE n > 0 $$".into(),
                format!("SELECT count(*), sum(x) FROM deep_srf({n}) x"),
            ];
        }
        // (Stable: Postgres inlines it in FROM level by level, until `n > 0` is false.)
        "sqlfnsrfinline" => {
            return vec![
                "CREATE OR REPLACE FUNCTION deep_srfi(n int) RETURNS SETOF int STABLE LANGUAGE sql AS $$ SELECT n UNION ALL SELECT * FROM deep_srfi(n - 1) WHERE n > 0 $$".into(),
                format!("SELECT count(*), sum(x) FROM deep_srfi({n}) x"),
            ];
        }
        "setting" => {
            return [
                "SHOW max_stack_depth",
                "SET max_stack_depth = '99kB'",
                "SET max_stack_depth = '100kB'",
                "SHOW max_stack_depth",
                "SET max_stack_depth = '2MB'",
                "SHOW max_stack_depth",
                "SET max_stack_depth = '7680kB'",
                "SET max_stack_depth = '7681kB'",
                "SET max_stack_depth = '1GB'",
                "RESET max_stack_depth",
                "SELECT setting, unit, min_val, context FROM pg_settings WHERE name = 'max_stack_depth'",
            ]
            .map(String::from)
            .to_vec();
        }
        _ => panic!("unknown shape {shape}"),
    };
    vec![one]
}

/// One line per statement: its first value, or its error (SQLSTATE, message, hint).
fn summary(events: &[Event]) -> String {
    let mut out = String::new();
    for e in events {
        match e {
            Event::Notice(_) => {}
            Event::Error(f) => {
                let _ = write!(out, "ERROR {} {}", f.code, f.message);
                if let Some(d) = &f.detail {
                    // (One line per case: a DETAIL of several lines, libxml2's context and
                    // caret, is kept with its line breaks written as `\n`.)
                    let _ = write!(out, " DETAIL: {}", d.replace('\n', "\\n"));
                }
                if let Some(h) = &f.hint {
                    let _ = write!(out, " HINT: {h}");
                }
            }
            Event::Result { rows, tag, .. } => match rows.first() {
                Some(r) => {
                    let vals: Vec<&str> = r.iter().map(|v| v.as_deref().unwrap_or("NULL")).collect();
                    out.push_str(&vals.join("|"));
                }
                None => out.push_str(tag),
            },
        }
    }
    out
}

fn cases() -> Vec<(&'static str, usize)> {
    // DEEP_SIZES=<shape>:<n>,...: other sizes (probing where the engine's limits are).
    if let Ok(sizes) = std::env::var("DEEP_SIZES") {
        return sizes
            .split(',')
            .filter_map(|c| c.split_once(':'))
            .map(|(s, n)| {
                let s = CASES.iter().find(|(c, _)| *c == s).expect("known shape").0;
                (s, n.parse().unwrap())
            })
            .collect();
    }
    let filter = std::env::var("DEEP_FILTER").ok();
    let release: &[(&str, usize)] = if cfg!(debug_assertions) { &[] } else { RELEASE_CASES };
    CASES
        .iter()
        .chain(release)
        .copied()
        .filter(move |(s, _)| filter.as_deref().is_none_or(|f| f == *s))
        .collect()
}

fn key(shape: &str, n: usize, i: usize) -> String {
    format!("{shape} {n} #{i}")
}

fn expected() -> std::collections::HashMap<String, String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/deep_input.out");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Run every case through one embedded session on this thread; the differences from the
/// recorded output.
fn run_here() -> Vec<String> {
    let want = expected();
    let dir = tempfile::tempdir().unwrap();
    let mut s = engine_session(open_store(dir.path()), "deep");
    let mut diffs = Vec::new();
    for (shape, n) in cases() {
        for (i, sql) in statements_of(shape, n).iter().enumerate() {
            let k = key(shape, n, i);
            eprintln!("deep_input: {k}");
            let t = std::time::Instant::now();
            let got = summary(&engine_query(&mut s, sql));
            eprintln!("deep_input: {k}: {:.1?}", t.elapsed());
            match want.get(&k) {
                Some(w) if *w == got => {}
                w => diffs.push(format!("{k}\n  expected: {w:?}\n  got:      {got:?}")),
            }
        }
    }
    diffs
}

fn check(diffs: Vec<String>) {
    assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
}

#[test]
fn deep_input_on_2mb_thread() {
    if std::env::var("DEEP_RECORD").is_ok() {
        return;
    }
    let diffs = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(run_here)
        .unwrap()
        .join()
        .unwrap();
    check(diffs);
}

#[test]
fn deep_input_on_test_thread() {
    if std::env::var("DEEP_RECORD").is_ok() {
        return;
    }
    check(run_here());
}

#[test]
fn deep_input_record() {
    let Ok(addr) = std::env::var("DEEP_RECORD") else {
        return;
    };
    let password = std::env::vars()
        .find(|(k, _)| k == "GOLDEN_PASSWORD")
        .map_or_else(|| "pw".to_string(), |(_, v)| v);
    let mut admin = PgClient::connect(&addr, "postgres", &password, "postgres").unwrap();
    admin.simple_query("DROP DATABASE IF EXISTS deep_input WITH (FORCE)").unwrap();
    admin.simple_query("CREATE DATABASE deep_input TEMPLATE template0").unwrap();
    let mut out = String::new();
    {
        let mut c = PgClient::connect(&addr, "postgres", &password, "deep_input").unwrap();
        for (shape, n) in CASES.iter().chain(RELEASE_CASES).copied() {
            for (i, sql) in statements_of(shape, n).iter().enumerate() {
                eprintln!("record: {}", key(shape, n, i));
                let got = summary(&c.simple_query(sql).unwrap());
                let _ = writeln!(out, "{}\t{got}", key(shape, n, i));
            }
        }
    }
    admin.simple_query("DROP DATABASE deep_input WITH (FORCE)").unwrap();
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/deep_input.out");
    std::fs::write(path, out).unwrap();
}
