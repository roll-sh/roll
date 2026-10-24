//! Live comparisons of `roll nosql cqlsh` with the references' cqlsh, for output that depends
//! on server state (traces, COPY, keyspace lists, logins, piped input): both run against the same
//! server one after the other and must print the same (after masking addresses, rates and
//! timings). Skips without Docker or a reachable reference.

mod common;

use common::*;

fn docker() -> bool {
    std::process::Command::new("docker").arg("version").output().is_ok_and(|o| o.status.success())
}

fn usable(r: &Ref) -> bool {
    if !docker() || !r.reachable() {
        eprintln!("skipping: needs docker and {}", r.container);
        return false;
    }
    true
}

fn e(s: &str) -> Vec<String> {
    vec!["-e".into(), s.into()]
}

fn same(r: &Ref, args: Vec<String>) {
    for attempt in 0..3 {
        let want = mask(&r.real(&args));
        let got = mask(&ours(r, &args, None));
        match compare(&want, &got) {
            None => return,
            Some(d) if attempt == 2 => panic!("{} {args:?}: {d}", r.dialect),
            Some(_) => std::thread::sleep(std::time::Duration::from_millis(500)),
        }
    }
}

fn setup(r: &Ref, ks: &str) {
    let s = format!(
        "DROP KEYSPACE IF EXISTS {ks}; CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}; \
         CREATE TABLE {ks}.t (k int PRIMARY KEY, te text, li list<int>, ma map<text, int>, ts timestamp, f float, do double, bo boolean, bl blob, d date, de decimal, se set<text>, i inet, u uuid); \
         INSERT INTO {ks}.t (k, te, li, ma, ts, f, do, bo, bl, d, de, se, i, u) VALUES (1, 'a,b \"q\"', [1, 2], {{'x': 1}}, '2026-10-20 12:00:00.123', 1.5, 3.141592653589793, true, 0xcafe, '2026-10-20', 1.10, {{'s'}}, '::1', 550e8400-e29b-41d4-a716-446655440000); \
         INSERT INTO {ks}.t (k, te) VALUES (2, 'line\nbreak'); INSERT INTO {ks}.t (k) VALUES (3); \
         CREATE TABLE {ks}.copy (k int PRIMARY KEY, te text, li list<int>, ma map<text, int>, ts timestamp, f float, do double, bo boolean, bl blob, d date, de decimal, se set<text>, i inet, u uuid)"
    );
    let r0 = ours(r, &e(&s), None);
    assert_eq!(r0.code, 0, "setup: {}", r0.err);
}

fn teardown(r: &Ref, ks: &str) {
    ours(r, &e(&format!("DROP KEYSPACE IF EXISTS {ks}")), None);
}

fn traces(r: &Ref) {
    if !usable(r) {
        return;
    }
    let ks = format!("cli_live_tr_{}", r.dialect);
    setup(r, &ks);
    let run = ours(r, &e(&format!("TRACING ON; SELECT k FROM {ks}.t WHERE k = 1")), None);
    let at = run.out.find("Tracing session: ").expect("a trace");
    let id = run.out[at + 17..at + 53].to_string();
    std::thread::sleep(std::time::Duration::from_secs(1));
    let real = r.real(&e(&format!("SHOW SESSION {id}")));
    let mine = ours(r, &e(&format!("SHOW SESSION {id}")), None);
    assert_eq!(compare(&real, &mine), None, "{}", r.dialect);
    // The trace printed after the statement has the same layout.
    let inline = &run.out[at - 1..];
    assert_eq!(inline.lines().nth(3), real.out.lines().nth(3), "header");
    same(r, e("SHOW SESSION 93c355d0-ccc2-11f1-8c36-33d044eb584e"));
    teardown(r, &ks);
}

#[test]
fn traces_scylla() {
    traces(&SCYLLA);
}

#[test]
fn traces_cassandra() {
    traces(&CASSANDRA);
}

fn sorted(s: &str) -> Vec<String> {
    let mut v: Vec<String> = s.split("\r\n").map(String::from).collect();
    v.sort();
    v
}

fn copy(r: &Ref) {
    if !usable(r) {
        return;
    }
    let ks = format!("cli_live_cp_{}", r.dialect);
    setup(r, &ks);
    stage_files(r, true);
    let file = format!("{SCRATCH}/export_{}.csv", r.dialect);
    for opts in ["", " WITH HEADER = true AND DELIMITER = '|' AND NULL = 'NIL'", " WITH QUOTE = '''' AND ESCAPE = ''''", " WITH BOOLSTYLE = 'yes,no' AND FLOATPRECISION = 2 AND DOUBLEPRECISION = 3"] {
        // To STDOUT: rows come in any order from cqlsh's workers.
        let q = format!("COPY {ks}.t TO STDOUT{opts}");
        let (want, got) = (r.real(&e(&q)), ours(r, &e(&q), None));
        assert_eq!(sorted(&want.out), sorted(&got.out), "{q}");
        assert_eq!((want.err.as_str(), want.code), (got.err.as_str(), got.code), "{q}");
        // To a file, with the messages.
        let q = format!("COPY {ks}.t (k, te, li, ma, bo) TO '{file}'{opts}");
        reset_scratch(r, true);
        let want = r.real(&e(&q));
        let real_csv = std::process::Command::new("docker").args(["exec", r.container, "cat", &file]).output().unwrap().stdout;
        let got = ours(r, &e(&q), None);
        let our_csv = std::fs::read(&file).unwrap();
        assert_eq!(compare(&mask(&want), &mask(&got)), None, "{q}");
        assert_eq!(sorted(&String::from_utf8_lossy(&real_csv)), sorted(&String::from_utf8_lossy(&our_csv)), "{q}");
    }
    // From a file: both import what cqlsh exported.
    let q = format!("COPY {ks}.t TO '{file}' WITH HEADER = true");
    reset_scratch(r, true);
    r.real(&e(&q));
    let back = format!("TRUNCATE {ks}.copy; COPY {ks}.copy FROM '{file}' WITH HEADER = true; SELECT * FROM {ks}.copy");
    let want = r.real(&e(&back));
    let _ = std::fs::write(&file, std::process::Command::new("docker").args(["exec", r.container, "cat", &file]).output().unwrap().stdout);
    let got = ours(r, &e(&back), None);
    assert_eq!(compare(&mask(&want), &mask(&got)), None, "{back}");
    // From STDIN.
    let stdin = "10,x,\"[1, 2]\"\n11,y,\n";
    let q = format!("COPY {ks}.copy (k, te, li) FROM STDIN; SELECT k, te, li FROM {ks}.copy WHERE k IN (10, 11)");
    let want = r.real_stdin(&e(&q), stdin);
    let got = ours(r, &e(&q), Some(stdin));
    assert_eq!(compare(&mask(&want), &mask(&got)), None, "{q}");
    for bad in [
        format!("COPY {ks}.t (k, nope) TO STDOUT"),
        format!("COPY {ks}.nope TO STDOUT"),
        format!("COPY {ks}.copy (te) FROM STDIN"),
        format!("COPY {ks}.t TO STDOUT WITH BOGUS = 1"),
    ] {
        same(r, e(&bad));
    }
    teardown(r, &ks);
}

#[test]
fn copy_scylla() {
    copy(&SCYLLA);
}

#[test]
fn copy_cassandra() {
    copy(&CASSANDRA);
}

fn lists(r: &Ref) {
    if !usable(r) {
        return;
    }
    for q in ["DESC KEYSPACES", "DESC TABLES", "DESC TYPES", "SHOW VERSION", "SHOW HOST", "DESC KEYSPACES; DESC TABLES", "DESC FULL SCHEMA"] {
        same(r, e(q));
    }
}

#[test]
fn lists_scylla() {
    lists(&SCYLLA);
}

#[test]
fn lists_cassandra() {
    lists(&CASSANDRA);
}

fn piped(r: &Ref) {
    if !usable(r) {
        return;
    }
    let script = "SELECT key\n  FROM system.local;\nSELEC 1;\nEXPAND ON\nSELECT key FROM system.local;\nSELECT 'open\n";
    let want = r.real_stdin(&[], script);
    let got = ours(r, &[], Some(script));
    assert_eq!(compare(&mask(&want), &mask(&got)), None);
}

#[test]
fn piped_scylla() {
    piped(&SCYLLA);
}

#[test]
fn piped_cassandra() {
    piped(&CASSANDRA);
}

fn auth(r: &Ref) {
    if !usable(r) {
        return;
    }
    let up = |p: &str, rest: &[&str]| {
        let mut v: Vec<String> = vec!["-u".into(), "cassandra".into(), "-p".into(), p.into()];
        v.extend(rest.iter().map(|s| s.to_string()));
        v
    };
    same(r, up("cassandra", &["-e", "SELECT role FROM system_auth.roles WHERE role = 'cassandra'; LOGIN cassandra 'wrong'; LIST ROLES"]));
    same(r, up("cassandra", &["-e", "LOGIN cassandra 'cassandra'; SELECT role FROM system_auth.roles WHERE role = 'cassandra'"]));
    same(r, up("wrong", &["-e", "SELECT 1"]));
    same(r, e("SELECT 1"));
    same(r, up("cassandra", &["--insecure-password-without-warning", "-k", "nope", "-e", "SELECT 1"]));
    same(r, up("cassandra", &["--insecure-password-without-warning", "-k", "system", "-e", "SELECT key FROM local"]));
}

#[test]
fn auth_scylla() {
    auth(&SCYLLA_AUTH);
}

#[test]
fn auth_cassandra() {
    auth(&CASSANDRA_AUTH);
}

/// `--tty` with piped input: prompts, the banner, tty table layout and `---MORE---` paging.
fn forced_tty(r: &Ref) {
    if !usable(r) {
        return;
    }
    let script = "SELECT key FROM system.local;\nSELECT\n key FROM system.local;\nPAGING 1\nSELECT keyspace_name FROM system_schema.keyspaces WHERE keyspace_name IN ('system', 'system_auth');\n\n\nEXPAND ON\nSELECT key FROM system.local;\nUSE system;\nSELECT key\nFROM local;\n";
    let args = vec!["-t".to_string()];
    let want = r.real_stdin(&args, script);
    let got = ours(r, &args, Some(script));
    assert_eq!(compare(&mask(&want), &mask(&got)), None);
}

#[test]
fn forced_tty_scylla() {
    forced_tty(&SCYLLA);
}

#[test]
fn forced_tty_cassandra() {
    forced_tty(&CASSANDRA);
}

/// `-C`: colours by value type and by key role, escapes in text, errors in red.
fn colors(r: &Ref) {
    if !usable(r) {
        return;
    }
    let ks = format!("cli_live_co_{}", r.dialect);
    setup(r, &ks);
    let extra = format!(
        "CREATE TABLE {ks}.w (p int, c text, s int static, v frozen<tuple<int, text>>, PRIMARY KEY (p, c)); \
         INSERT INTO {ks}.w (p, c, s, v) VALUES (1, 'a\\\\b\tc', 2, (null, 'x'))"
    );
    assert_eq!(ours(r, &e(&extra), None).code, 0);
    same(r, vec!["-C".into(), "-e".into(), format!("SELECT * FROM {ks}.t; SELECT * FROM {ks}.w; SELECT p, writetime(s) FROM {ks}.w; SELEC")]);
    same(r, vec!["-C".into(), "-e".into(), format!("EXPAND ON; SELECT * FROM {ks}.w; CONSISTENCY")]);
    teardown(r, &ks);
}

#[test]
fn colors_scylla() {
    colors(&SCYLLA);
}

#[test]
fn colors_cassandra() {
    colors(&CASSANDRA);
}
