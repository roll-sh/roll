//! pg_dump, pg_restore, pg_dumpall and psql round trips against the engine and the reference
//! PostgreSQL 18.6, driven by PostgreSQL 18.6's own client programs. Two `#[ignore]`d tests;
//! scores and the classified differences are in `tests/pg_dump/STATUS.md`.
//!
//! - `pg_dump_tap`: PostgreSQL's pg_dump TAP tests (`src/bin/pg_dump/t/*.pl` at REL_18_6, the
//!   big table of object types and expected dump lines in `002_pg_dump.pl` above all), run
//!   unmodified against a fresh engine through stand-ins for `PostgreSQL::Test::Cluster` /
//!   `Utils` (`tests/pg_dump/perl`) that attach to a running server and run the client
//!   programs in a container of the reference image. The reference passes every test.
//! - `pg_dump_roundtrip`: the regression suite's database (left by a `pg_regress` run with
//!   `PGREGRESS_KEEP=1`) dumped from the engine and from the reference in every format and
//!   with the common options, compared entry by entry (the dump's `-- Name:` sections);
//!   each side's custom dump restored into a fresh database on the other side with
//!   pg_restore, and each plain dump with psql, the restores' errors counted and the restored
//!   databases dumped again and compared; pg_dumpall's globals compared.
//!
//! ```text
//! PGDUMP_DIR=~/.cache/x/pgdump PGREGRESS_DIR=~/.cache/x/pgregress \
//!   cargo test -p pg-engine --test pg_dump -- --ignored --nocapture pg_dump_roundtrip
//! ```
//!
//! Environment:
//! - `PGDUMP_DIR`: scratch directory (dumps, the engine's data); on disk, not /tmp (tmpfs).
//!   It must be mounted at the same path in the client container.
//! - `PGDUMP_CLIENT`: the client container: the reference image on the host network, e.g.
//!   `docker run -d --name x --network host --user $(id -u):$(id -g) -v $D:$D --entrypoint
//!   sleep <image> infinity` (default `pgdump-client`).
//! - `PGDUMP_REF_PORT`: the reference server's port on 127.0.0.1 (default 55499), user
//!   postgres, password `pw`. `pg_dump_tap` needs a reference cluster of its own (the TAP
//!   test makes roles, tablespaces, databases and changes system catalogs' privileges), so
//!   it only runs against the engine unless `PGDUMP_TAP_REF_PORT` names a fresh one.
//! - `PGDUMP_PORT`: the port the engine is started on (default 55610).
//! - `PGDUMP_ENGINE_BIN`: the engine binary (default: the one cargo built).
//! - `PGREGRESS_DIR` / `PGREGRESS_DB`: where the `pg_regress` run kept the engine's data
//!   (`$PGREGRESS_DIR/enginedata`, copied first) and the database name (both sides).
//! - `PGDUMP_SRC`: a PostgreSQL REL_18_6 checkout with `src/bin/pg_dump` (default
//!   `$PGREGRESS_DIR/src-repo`; `pg_dump_tap` fetches it when missing).
//! - `PGDUMP_REPORT_DIR`: where `REPORT.md` and the per-run diffs go (default
//!   `tests/pg_dump`).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Config {
    dir: PathBuf,
    client: String,
    ref_port: u16,
    port: u16,
    report_dir: PathBuf,
}

impl Config {
    fn from_env() -> Config {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let dir = PathBuf::from(env("PGDUMP_DIR").expect("PGDUMP_DIR (on disk, mounted in the client container)"));
        std::fs::create_dir_all(&dir).unwrap();
        Config {
            dir,
            client: env("PGDUMP_CLIENT").unwrap_or_else(|| "pgdump-client".into()),
            ref_port: env("PGDUMP_REF_PORT").and_then(|p| p.parse().ok()).unwrap_or(55499),
            port: env("PGDUMP_PORT").and_then(|p| p.parse().ok()).unwrap_or(55610),
            report_dir: env("PGDUMP_REPORT_DIR").map_or_else(
                || Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/pg_dump"),
                PathBuf::from,
            ),
        }
    }
}

/// The output of a client program: (exit code, stdout, stderr).
struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs a client program (pg_dump, psql, ...) in the client container against
/// 127.0.0.1:`port`, with `stdin` fed to it.
fn client(cfg: &Config, port: u16, args: &[&str], stdin: Option<&[u8]>) -> Out {
    let mut cmd = Command::new("docker");
    cmd.args(["exec", "-i"]);
    for e in [
        "PGHOST=127.0.0.1".to_string(),
        format!("PGPORT={port}"),
        "PGUSER=postgres".into(),
        "PGPASSWORD=pw".into(),
        "LC_MESSAGES=C".into(),
        "PGAPPNAME=pg_dump_harness".into(),
    ] {
        cmd.args(["-e", &e]);
    }
    cmd.args(["-w", cfg.dir.to_str().unwrap(), &cfg.client]);
    cmd.args(args);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("docker exec");
    let mut input = child.stdin.take().unwrap();
    let data = stdin.unwrap_or_default().to_vec();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let _ = input.write_all(&data);
    });
    let out = child.wait_with_output().expect("docker exec");
    let _ = writer.join();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// psql -X -q on `db` with ON_ERROR_STOP off: (output, the ERROR lines).
fn psql(cfg: &Config, port: u16, db: &str, sql: &str) -> (String, Vec<String>) {
    let o = client(cfg, port, &["psql", "-X", "-q", "-d", db, "-f", "-"], Some(sql.as_bytes()));
    let errors = o
        .stderr
        .lines()
        .filter(|l| l.contains("ERROR:") || l.contains("FATAL:"))
        .map(str::to_string)
        .collect();
    (o.stdout, errors)
}

/// An engine started on a data directory, stopped when dropped.
struct Engine {
    child: Child,
}

impl Engine {
    fn start(bin: &Path, data: &Path, port: u16, log: &Path) -> Engine {
        assert!(
            std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
            "port {port} is taken (PGDUMP_PORT)"
        );
        let logf = std::fs::File::create(log).unwrap();
        let child = Command::new(bin)
            .args(["--data", data.to_str().unwrap(), "--listen", &format!("127.0.0.1:{port}")])
            .args(["--no-sync", "--idle-close-secs", "100000"])
            .env("RUST_LOG", "warn")
            .stdout(logf.try_clone().unwrap())
            .stderr(logf)
            .spawn()
            .expect("start roll-engine");
        let deadline = Instant::now() + Duration::from_secs(180);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "roll-engine did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        Engine { child }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A private copy of the engine binary (builds may relink cargo's while a run goes on).
fn engine_bin(cfg: &Config) -> PathBuf {
    let src = std::env::var("PGDUMP_ENGINE_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_roll-engine").to_string());
    let bin = cfg.dir.join("roll-engine.bin");
    let _ = std::fs::remove_file(&bin);
    std::fs::copy(&src, &bin).expect("copy roll-engine");
    bin
}

// ---------------------------------------------------------------------------------------------
// The TAP tests

/// The pg_dump TAP files run (each against a fresh engine). (003 needs a test extension
/// installed on the server, 010 clusters made by initdb with other bootstrap superusers.)
const TAP_FILES: &[&str] = &[
    "001_basic.pl",
    "002_pg_dump.pl",
    "004_pg_dump_parallel.pl",
    "005_pg_dump_filterfile.pl",
];

/// Runs one TAP file against 127.0.0.1:`port`: (passed, failed, output).
fn run_tap(cfg: &Config, src: &Path, file: &str, port: u16, tmp: &Path) -> (usize, usize, String) {
    let _ = std::fs::remove_dir_all(tmp);
    std::fs::create_dir_all(tmp).unwrap();
    let perl = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/pg_dump/perl");
    let out = Command::new("perl")
        .arg("-I")
        .arg(&perl)
        .arg(src.join("src/bin/pg_dump/t").join(file))
        .env("PGDUMP_TAP_CONTAINER", &cfg.client)
        .env("PGDUMP_TAP_HOST", "127.0.0.1")
        .env("PGDUMP_TAP_PORT", port.to_string())
        .env("PGDUMP_TAP_PASSWORD", "pw")
        .env("PGDUMP_TAP_DIR", tmp)
        // A failing setup statement doesn't end the run (every later check would fail).
        .env("PGDUMP_TAP_LENIENT", "1")
        .env("with_icu", "yes")
        .env("GZIP_PROGRAM", "gzip")
        .env("ZSTD", "zstd")
        .output()
        .expect("perl");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let passed = stdout.lines().filter(|l| l.starts_with("ok ")).count();
    let failed = stdout.lines().filter(|l| l.starts_with("not ok ")).count();
    let text = format!("{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    (passed, failed, text)
}

#[test]
#[ignore = "runs PostgreSQL's pg_dump TAP tests against a fresh engine; slow"]
fn pg_dump_tap() {
    let cfg = Config::from_env();
    let src = std::env::var("PGDUMP_SRC").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("PGREGRESS_DIR").expect("PGDUMP_SRC or PGREGRESS_DIR")).join("src-repo")
    });
    assert!(
        src.join("src/bin/pg_dump/t/002_pg_dump.pl").exists(),
        "no pg_dump TAP tests in {} (git sparse-checkout add src/bin/pg_dump)",
        src.display()
    );
    let mut report = String::from("# pg_dump TAP tests: last run\n\n| file | passed | failed |\n|---|---|---|\n");
    let mut failures = String::new();
    let (mut passed, mut failed) = (0, 0);
    let bin = engine_bin(&cfg);
    for file in TAP_FILES {
        let data = cfg.dir.join("tapdata");
        let _ = std::fs::remove_dir_all(&data);
        let engine = Engine::start(&bin, &data, cfg.port, &cfg.dir.join("tap-engine.log"));
        let (p, f, text) = run_tap(&cfg, &src, file, cfg.port, &cfg.dir.join("tap"));
        drop(engine);
        std::fs::write(cfg.dir.join(format!("tap-{file}.out")), &text).unwrap();
        eprintln!("{file}: {p} passed, {f} failed");
        let _ = writeln!(report, "| {file} | {p} | {f} |");
        for l in text.lines().filter(|l| l.starts_with("not ok ") || l.starts_with("# create error")) {
            let _ = writeln!(failures, "- {file}: `{}`", l.replace('`', "'"));
        }
        passed += p;
        failed += f;
        // Against a reference cluster of its own, too.
        if let Ok(port) = std::env::var("PGDUMP_TAP_REF_PORT")
            && let Ok(port) = port.parse()
            && *file == "002_pg_dump.pl"
        {
            let (p, f, _) = run_tap(&cfg, &src, file, port, &cfg.dir.join("tap-ref"));
            eprintln!("{file} on the reference: {p} passed, {f} failed");
        }
    }
    let _ = writeln!(report, "| total | {passed} | {failed} |\n\n## Failures\n\n{failures}");
    std::fs::create_dir_all(&cfg.report_dir).unwrap();
    std::fs::write(cfg.report_dir.join("TAP.md"), report).unwrap();
}

// ---------------------------------------------------------------------------------------------
// psql's backslash commands

/// Runs `tests/pg_dump/psql_meta.sql` (psql -a, so each line is echoed) on both sides'
/// regression databases and compares the output line by line of the script: a unit is one
/// script line and what it printed.
#[test]
#[ignore = "runs psql's backslash commands on the engine and the reference; slow"]
fn pg_psql_meta() {
    let cfg = Config::from_env();
    let regress = PathBuf::from(std::env::var("PGREGRESS_DIR").expect("PGREGRESS_DIR"));
    let db = std::env::var("PGREGRESS_DB").unwrap_or_else(|_| "gxg_regression".into());
    let data = cfg.dir.join("enginedata");
    let _ = std::fs::remove_dir_all(&data);
    assert!(
        Command::new("cp")
            .args(["-a", regress.join("enginedata").to_str().unwrap(), data.to_str().unwrap()])
            .status()
            .is_ok_and(|s| s.success())
    );
    let _engine = Engine::start(&engine_bin(&cfg), &data, cfg.port, &cfg.dir.join("engine.log"));
    let script = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/pg_dump/psql_meta.sql"),
    )
    .unwrap();
    let run = |port: u16| {
        let mut cmd = Command::new("docker");
        cmd.args(["exec", "-i"]);
        for e in [
            "PGHOST=127.0.0.1".to_string(),
            format!("PGPORT={port}"),
            "PGUSER=postgres".into(),
            "PGPASSWORD=pw".into(),
            "LC_MESSAGES=C".into(),
            "PSQL_EDITOR=cat".into(),
            "COLUMNS=200".into(),
        ] {
            cmd.args(["-e", &e]);
        }
        cmd.args([&cfg.client, "psql", "-X", "-a", "-d", &db]);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("docker exec");
        let mut input = child.stdin.take().unwrap();
        let data = script.clone();
        let w = std::thread::spawn(move || {
            use std::io::Write;
            let _ = input.write_all(data.as_bytes());
        });
        let out = child.wait_with_output().unwrap();
        let _ = w.join();
        // (stderr interleaved after stdout is good enough: errors are compared per unit too.)
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str("\n-- stderr --\n");
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        s
    };
    let (r, e) = (run(cfg.ref_port), run(cfg.port));
    std::fs::write(cfg.dir.join("psql-ref.out"), &r).unwrap();
    std::fs::write(cfg.dir.join("psql-engine.out"), &e).unwrap();
    let lines: Vec<&str> = script.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("--")).collect();
    let units = |out: &str| -> BTreeMap<usize, String> {
        let mut m = BTreeMap::new();
        let mut cur: Option<usize> = None;
        let mut next = 0;
        for l in out.lines() {
            if next < lines.len() && l == lines[next] {
                cur = Some(next);
                next += 1;
                m.insert(next - 1, String::new());
                continue;
            }
            if let Some(c) = cur {
                let u = m.get_mut(&c).unwrap();
                u.push_str(l);
                u.push('\n');
            }
        }
        m
    };
    let (ur, ue) = (units(&r), units(&e));
    let mut report = String::from("# psql backslash commands: last run\n\n");
    let (mut same, mut diffs) = (0, String::new());
    for (i, l) in lines.iter().enumerate() {
        if ur.get(&i) == ue.get(&i) {
            same += 1;
        } else {
            let _ = writeln!(
                diffs,
                "=== {l}\n--- reference\n{}+++ engine\n{}",
                head(ur.get(&i).map_or("", |s| s), 30),
                head(ue.get(&i).map_or("", |s| s), 30)
            );
            let _ = writeln!(report, "- differs: `{}`", l.replace('`', "'"));
        }
    }
    let _ = writeln!(report, "\n{same}/{} units match", lines.len());
    eprintln!("psql: {same}/{} units match", lines.len());
    std::fs::write(cfg.dir.join("psql-diffs.txt"), diffs).unwrap();
    std::fs::create_dir_all(&cfg.report_dir).unwrap();
    std::fs::write(cfg.report_dir.join("PSQL.md"), report).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Comparing dumps

/// A plain dump's sections, by their `-- Name:` / `-- Data for Name:` header (a repeated
/// header gets `#n`); the preamble before the first header is `(preamble)`.
fn entries(dump: &str) -> BTreeMap<String, String> {
    let dump = normalize(dump);
    let mut out = BTreeMap::new();
    let mut key = "(preamble)".to_string();
    let mut body = String::new();
    let put = |key: &str, body: &mut String, out: &mut BTreeMap<String, String>| {
        let mut k = key.to_string();
        let mut n = 1;
        while out.contains_key(&k) {
            n += 1;
            k = format!("{key}#{n}");
        }
        out.insert(k, std::mem::take(body).trim().to_string());
    };
    let lines: Vec<&str> = dump.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        let header = l.strip_prefix("-- Name: ").or_else(|| l.strip_prefix("-- Data for Name: "));
        if let Some(h) = header
            && i > 0
            && lines[i - 1] == "--"
        {
            // (The `--` before the header belongs to it.)
            if body.ends_with("--\n") {
                body.truncate(body.len() - 3);
            }
            put(&key, &mut body, &mut out);
            key = if l.starts_with("-- Data") { format!("DATA {h}") } else { h.to_string() };
            i += 2; // the header and the `--` after it
            continue;
        }
        body.push_str(l);
        body.push('\n');
        i += 1;
    }
    put(&key, &mut body, &mut out);
    out
}

/// What PostgreSQL itself varies from dump to dump: the `\restrict` key, and large objects'
/// oids (handed out by the oid counter; numbered in order of appearance).
fn normalize(dump: &str) -> String {
    let mut los: Vec<String> = Vec::new();
    for l in dump.lines() {
        let oid = l
            .strip_prefix("ALTER LARGE OBJECT ")
            .and_then(|r| r.split(' ').next())
            .or_else(|| l.strip_prefix("SELECT pg_catalog.lo_create('").and_then(|r| r.split('\'').next()));
        if let Some(oid) = oid
            && oid.bytes().all(|b| b.is_ascii_digit())
            && !los.iter().any(|o| o == oid)
        {
            los.push(oid.to_string());
        }
    }
    let mut out = String::with_capacity(dump.len());
    for l in dump.lines() {
        if l.starts_with("\\restrict ") || l.starts_with("\\unrestrict ") {
            out.push_str(l.split(' ').next().unwrap());
            out.push_str(" KEY\n");
            continue;
        }
        let mut l = l.to_string();
        if !los.is_empty() && (l.contains("lo_") || l.contains("LARGE OBJECT") || l.contains("BLOB")) {
            for (n, oid) in los.iter().enumerate() {
                l = replace_word(&l, oid, &format!("<lo{}>", n + 1));
            }
        }
        out.push_str(&l);
        out.push('\n');
    }
    out
}

/// `s` with `word` replaced where it isn't part of a longer run of digits.
fn replace_word(s: &str, word: &str, by: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(p) = rest.find(word) {
        let before = rest[..p].chars().last();
        let after = rest[p + word.len()..].chars().next();
        out.push_str(&rest[..p]);
        if before.is_some_and(|c| c.is_ascii_digit()) || after.is_some_and(|c| c.is_ascii_digit()) {
            out.push_str(word);
        } else {
            out.push_str(by);
        }
        rest = &rest[p + word.len()..];
    }
    out.push_str(rest);
    out
}

/// One comparison of two dumps.
#[derive(Default)]
struct Cmp {
    total: usize,
    same: usize,
    /// Entries only one side has, or that differ, with both sides' text.
    diffs: Vec<(String, Option<String>, Option<String>)>,
}

fn compare(reference: &str, engine: &str) -> Cmp {
    let (r, e) = (entries(reference), entries(engine));
    let mut c = Cmp::default();
    let keys: std::collections::BTreeSet<&String> = r.keys().chain(e.keys()).collect();
    for k in keys {
        c.total += 1;
        match (r.get(k), e.get(k)) {
            (Some(a), Some(b)) if a == b => c.same += 1,
            (a, b) => c.diffs.push((k.clone(), a.cloned(), b.cloned())),
        }
    }
    c
}

/// The first lines of a section, for the diff files.
fn head(s: &str, n: usize) -> String {
    let mut out: String = s.lines().take(n).map(|l| format!("{l}\n")).collect();
    if s.lines().count() > n {
        let _ = writeln!(out, "... ({} lines)", s.lines().count());
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Dumping

/// pg_dump's output format for a run.
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Plain,
    Custom,
    Directory,
    Tar,
}

/// A dump of `db` on `port` into `file` (named for the format), as plain SQL text: a
/// non-plain archive is read back with `pg_restore -f -`. Err: the failing step's stderr.
fn dump(cfg: &Config, port: u16, db: &str, file: &Path, f: Format, args: &[&str]) -> Result<String, String> {
    let _ = std::fs::remove_dir_all(file);
    let _ = std::fs::remove_file(file);
    let fmt = match f {
        Format::Plain => "p",
        Format::Custom => "c",
        Format::Directory => "d",
        Format::Tar => "t",
    };
    let path = file.to_str().unwrap();
    let mut a = vec!["pg_dump", "--no-sync", "-F", fmt, "-f", path];
    a.extend_from_slice(args);
    a.push(db);
    let o = client(cfg, port, &a, None);
    if o.code != 0 {
        return Err(o.stderr);
    }
    if f == Format::Plain {
        return std::fs::read_to_string(file).map_err(|e| e.to_string());
    }
    let o = client(cfg, port, &["pg_restore", "-f", "-", path], None);
    if o.code != 0 {
        return Err(o.stderr);
    }
    Ok(o.stdout)
}

/// One line of the report: a comparison (or the error that prevented it).
struct Line {
    name: String,
    result: Result<Cmp, String>,
    /// Errors a restore reported (deduplicated, in order).
    restore_errors: Option<Vec<String>>,
}

fn dedup(errors: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in errors {
        if !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

fn write_diffs(path: &Path, c: &Cmp, left: &str, right: &str) {
    let mut s = String::new();
    for (k, a, b) in &c.diffs {
        let _ = writeln!(s, "=== {k}");
        match a {
            Some(a) => {
                let _ = write!(s, "--- {left}\n{}", head(a, 40));
            }
            None => {
                let _ = writeln!(s, "--- {left}: (none)");
            }
        }
        match b {
            Some(b) => {
                let _ = write!(s, "+++ {right}\n{}", head(b, 40));
            }
            None => {
                let _ = writeln!(s, "+++ {right}: (none)");
            }
        }
    }
    std::fs::write(path, s).unwrap();
}

// ---------------------------------------------------------------------------------------------
// The round trips

/// The dumps compared: (name, format, pg_dump options).
const RUNS: &[(&str, Format, &[&str])] = &[
    ("plain", Format::Plain, &[]),
    ("custom", Format::Custom, &[]),
    ("directory", Format::Directory, &[]),
    ("directory_j4", Format::Directory, &["-j", "4"]),
    ("tar", Format::Tar, &[]),
    ("schema_only", Format::Plain, &["--schema-only"]),
    ("data_only", Format::Plain, &["--data-only"]),
    ("inserts", Format::Plain, &["--inserts"]),
    ("column_inserts", Format::Plain, &["--column-inserts", "--rows-per-insert", "3"]),
    ("no_owner_privileges", Format::Plain, &["--no-owner", "--no-privileges"]),
    ("schema_filter", Format::Plain, &["-n", "public", "-N", "regress_*"]),
    ("table_filter", Format::Plain, &["-t", "tenk1", "-t", "onek*", "-t", "public.road"]),
    ("clean_if_exists", Format::Plain, &["--clean", "--if-exists", "--create"]),
    ("lock_timeout", Format::Plain, &["--lock-wait-timeout", "5s", "--serializable-deferrable"]),
];

#[test]
#[ignore = "dumps the regression database from the engine and the reference; slow"]
fn pg_dump_roundtrip() {
    let cfg = Config::from_env();
    let regress = PathBuf::from(std::env::var("PGREGRESS_DIR").expect("PGREGRESS_DIR"));
    let db = std::env::var("PGREGRESS_DB").unwrap_or_else(|_| "gxg_regression".into());
    // A copy of the run's engine data: restores add databases to it.
    let data = cfg.dir.join("enginedata");
    let _ = std::fs::remove_dir_all(&data);
    assert!(
        Command::new("cp")
            .args(["-a", regress.join("enginedata").to_str().unwrap(), data.to_str().unwrap()])
            .status()
            .is_ok_and(|s| s.success()),
        "no engine data in {}/enginedata (run pg_regress with PGREGRESS_KEEP=1)",
        regress.display()
    );
    let _engine = Engine::start(&engine_bin(&cfg), &data, cfg.port, &cfg.dir.join("engine.log"));
    let dumps = cfg.dir.join("dumps");
    let diffs = cfg.dir.join("diffs");
    for d in [&dumps, &diffs] {
        let _ = std::fs::remove_dir_all(d);
        std::fs::create_dir_all(d).unwrap();
    }
    let sides = [("ref", cfg.ref_port), ("engine", cfg.port)];
    let mut lines = Vec::new();
    let mut plain = BTreeMap::new();
    for &(name, f, args) in RUNS {
        let ext = match f {
            Format::Plain => "sql",
            Format::Custom => "dump",
            Format::Directory => "dir",
            Format::Tar => "tar",
        };
        let out: Vec<Result<String, String>> = sides
            .iter()
            .map(|(side, port)| dump(&cfg, *port, &db, &dumps.join(format!("{side}-{name}.{ext}")), f, args))
            .collect();
        let result = match (&out[0], &out[1]) {
            (Ok(r), Ok(e)) => {
                let c = compare(r, e);
                write_diffs(&diffs.join(format!("{name}.txt")), &c, "reference", "engine");
                Ok(c)
            }
            (Err(r), _) => Err(format!("reference: {}", r.trim())),
            (_, Err(e)) => Err(format!("engine: {}", e.trim())),
        };
        eprintln!("{name}: {}", summary(&result));
        if name == "plain" {
            plain.insert("ref", out[0].clone().unwrap_or_default());
            plain.insert("engine", out[1].clone().unwrap_or_default());
        }
        lines.push(Line { name: name.into(), result, restore_errors: None });
    }
    lines.extend(restores(&cfg, &dumps, &diffs, &plain));
    lines.extend(dumpall(&cfg, &dumps, &diffs));
    write_report(&cfg, &lines);
}

fn summary(r: &Result<Cmp, String>) -> String {
    match r {
        Ok(c) => format!("{}/{} entries match", c.same, c.total),
        Err(e) => format!("failed: {}", e.lines().next().unwrap_or("")),
    }
}

/// Each side's custom dump restored with pg_restore, and its plain dump with psql, into a
/// fresh database on the other side, which is dumped again and compared with that side's
/// dump of its own regression database.
fn restores(
    cfg: &Config,
    dumps: &Path,
    diffs: &Path,
    plain: &BTreeMap<&str, String>,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let target = "pgdump_restored";
    for (from, to, port) in [("engine", "ref", cfg.ref_port), ("ref", "engine", cfg.port)] {
        for how in ["pg_restore", "psql"] {
            let name = format!("restore_{from}_to_{to}_{how}");
            let (_, errs) = psql(
                cfg,
                port,
                "postgres",
                &format!(
                    "DROP DATABASE IF EXISTS {target} WITH (FORCE);\nCREATE DATABASE {target} TEMPLATE template0 ENCODING 'UTF8' LOCALE 'C';\n"
                ),
            );
            assert!(errs.is_empty(), "{name}: {errs:?}");
            let errors = if how == "pg_restore" {
                let file = dumps.join(format!("{from}-custom.dump"));
                let o = client(cfg, port, &["pg_restore", "-d", target, file.to_str().unwrap()], None);
                o.stderr
                    .lines()
                    .filter(|l| l.contains("error:") || l.contains("ERROR:"))
                    .map(str::to_string)
                    .collect()
            } else {
                let sql = std::fs::read_to_string(dumps.join(format!("{from}-plain.sql"))).unwrap_or_default();
                psql(cfg, port, target, &sql).1
            };
            let again = dump(cfg, port, target, &dumps.join(format!("{name}.sql")), Format::Plain, &[]);
            let result = again.map(|d| {
                let c = compare(&plain[to], &d);
                write_diffs(&diffs.join(format!("{name}.txt")), &c, &format!("{to} original"), "restored");
                c
            });
            eprintln!("{name}: {} errors, {}", errors.len(), summary(&result));
            lines.push(Line { name, result, restore_errors: Some(dedup(errors)) });
            psql(cfg, port, "postgres", &format!("DROP DATABASE IF EXISTS {target} WITH (FORCE);\n"));
        }
    }
    lines
}

/// pg_dumpall's globals (roles, memberships, tablespaces) from both sides.
fn dumpall(cfg: &Config, dumps: &Path, diffs: &Path) -> Vec<Line> {
    let mut lines = Vec::new();
    for (name, opt) in [
        ("dumpall_globals", "--globals-only"),
        ("dumpall_roles", "--roles-only"),
        ("dumpall_tablespaces", "--tablespaces-only"),
    ] {
        let out: Vec<Result<String, String>> = [("ref", cfg.ref_port), ("engine", cfg.port)]
            .iter()
            .map(|(side, port)| {
                let file = dumps.join(format!("{side}-{name}.sql"));
                let o = client(cfg, *port, &["pg_dumpall", opt, "-f", file.to_str().unwrap()], None);
                if o.code != 0 {
                    return Err(o.stderr);
                }
                std::fs::read_to_string(file).map_err(|e| e.to_string())
            })
            .collect();
        let result = match (&out[0], &out[1]) {
            (Ok(r), Ok(e)) => {
                // (Its sections are statements, not `-- Name:` entries: compared line by line.)
                let c = compare_lines(r, e);
                write_diffs(&diffs.join(format!("{name}.txt")), &c, "reference", "engine");
                Ok(c)
            }
            (Err(r), _) => Err(format!("reference: {}", r.trim())),
            (_, Err(e)) => Err(format!("engine: {}", e.trim())),
        };
        eprintln!("{name}: {}", summary(&result));
        lines.push(Line { name: name.into(), result, restore_errors: None });
    }
    lines
}

/// Two texts compared as sets of (normalized) lines.
fn compare_lines(reference: &str, engine: &str) -> Cmp {
    let as_map = |s: &str| {
        let mut m = BTreeMap::new();
        for l in normalize(s).lines().filter(|l| !l.trim().is_empty() && !l.starts_with("--")) {
            *m.entry(l.to_string()).or_insert(0usize) += 1;
        }
        m
    };
    let (r, e) = (as_map(reference), as_map(engine));
    let mut c = Cmp::default();
    let keys: std::collections::BTreeSet<&String> = r.keys().chain(e.keys()).collect();
    for k in keys {
        c.total += 1;
        if r.get(k) == e.get(k) {
            c.same += 1;
        } else {
            c.diffs.push((k.clone(), r.contains_key(k).then(|| k.clone()), e.contains_key(k).then(|| k.clone())));
        }
    }
    c
}

fn write_report(cfg: &Config, lines: &[Line]) {
    let mut s = String::from(
        "# pg_dump round trips: last run\n\nWritten by `tests/pg_dump.rs` (`pg_dump_roundtrip`); \
         STATUS.md has the classified differences.\n\n| run | entries matching | restore errors |\n|---|---|---|\n",
    );
    for l in lines {
        let errs = l.restore_errors.as_ref().map_or("".into(), |e| e.len().to_string());
        let _ = writeln!(s, "| {} | {} | {errs} |", l.name, summary(&l.result));
    }
    for l in lines {
        let diffs = l.result.as_ref().map(|c| c.diffs.len()).unwrap_or(0);
        let errs = l.restore_errors.as_deref().unwrap_or(&[]);
        if diffs == 0 && errs.is_empty() {
            continue;
        }
        let _ = writeln!(s, "\n## {}\n", l.name);
        if let Ok(c) = &l.result {
            for (k, a, b) in c.diffs.iter().take(25) {
                let what = match (a, b) {
                    (Some(_), Some(_)) => "differs",
                    (Some(_), None) => "missing",
                    _ => "extra",
                };
                let _ = writeln!(s, "- {what}: `{}`", k.replace('`', "'"));
            }
            if c.diffs.len() > 25 {
                let _ = writeln!(s, "- ... {} more", c.diffs.len() - 25);
            }
        }
        for e in errs.iter().take(25) {
            let _ = writeln!(s, "- error: `{}`", e.replace('`', "'"));
        }
    }
    std::fs::create_dir_all(&cfg.report_dir).unwrap();
    std::fs::write(cfg.report_dir.join("REPORT.md"), s).unwrap();
}

