//! PostgreSQL's own regression suite (`src/test/regress`, REL_18_6) run through the engine and
//! through the reference Postgres 18.6, compared statement by statement. It writes a gap map to
//! `tests/regress/REPORT.md` and the first differing hunks per file to `tests/regress/hunks/`.
//!
//!     CARGO_BUILD_JOBS=2 cargo test -p pg-engine --test pg_regress -- --ignored --nocapture
//!
//! How it works (mirrors `pg_regress --schedule=parallel_schedule`, run serially):
//!
//! - The suite is sparse-cloned into `PGREGRESS_DIR` (default `/tmp/pgregress`) if missing, and
//!   its `data/` is copied into the reference container at the same absolute path, so
//!   `COPY ... FROM :'filename'` reads the same file on both servers.
//! - The engine (`roll-engine`, built by cargo for this test) is started on 127.0.0.1:55600 with
//!   a fresh data directory, and with server-side files allowed in `PGREGRESS_DIR`
//!   (`--allow-server-files`): it holds the suite's `data/` and `results/` (the absolute
//!   paths the scripts use) and stands for the data directory (`pg_read_file('postmaster.pid')`,
//!   `pg_ls_dir('.')`), so the harness puts a `postmaster.pid` and a `base/` there. The
//!   reference is the `roll-engine-ref` container (PG 18.6).
//! - Both sides get one database, `gxg_regression`, created and configured the way pg_regress
//!   does (template0, UTF8, locale C, lc_*/bytea_output/timezone_abbreviations settings), and
//!   every file of `parallel_schedule` runs in order against it, so objects created by earlier
//!   files (test_setup, create_table, ...) are there for later ones.
//! - Each file is fed to the real `psql -X -a -q -v HIDE_TABLEAM=on -v HIDE_TOAST_COMPRESSION=on`
//!   inside the container, with pg_regress's environment (PGTZ, PGDATESTYLE, PGOPTIONS, PG_ABS_SRCDIR,
//!   ...). The engine is reached through `host.docker.internal`, so both outputs come from the
//!   same psql binary and formatter.
//! - Before running, the file is split into psql execution units (a statement, several on one
//!   line, a meta-command, a COPY with its data) with a psql-like lexer, and a marker comment
//!   `-- @@@ <n>` is inserted after each unit. psql echoes every input line (`-a`), so the markers
//!   cut both outputs into the same units, which are compared one by one.
//! - The reference output is primary; the reference's output is also checked against
//!   `expected/<name>*.out` as a secondary signal (the container lacks `regress.so`, so files that
//!   load it differ from expected on the reference too).
//!
//! Environment:
//! - `PGREGRESS_ONLY=a,b,c`: run only these files (plus `test_setup`, always first).
//! - `PGREGRESS_REPORT_ONLY=1`: don't run anything, rebuild the report from saved outputs
//!   (`$PGREGRESS_DIR/out`).
//! - `PGREGRESS_TIMEOUT=<secs>`: how long one statement may run (psql printing nothing) before
//!   it counts as stalled (default 300). `PGREGRESS_FILE_TIMEOUT=<secs>`: the cap on a whole run
//!   of a file (default 3600). After an engine crash or stall the file resumes after the failing
//!   unit (up to 25 times, 2 timeouts).
//! - `PGREGRESS_PORT`, `PGREGRESS_CONTAINER`: engine port (55600) and reference container.
//! - `PGREGRESS_ENGINE_HOST`: the host name psql in the container uses for the engine
//!   (default `host.docker.internal`).
//! - `PGREGRESS_ENGINE_CONTAINER`: run the engine's psql in this container instead, reaching the
//!   engine at 127.0.0.1. For hosts whose firewall drops the docker bridge's traffic to the host
//!   (`host.docker.internal`): a container of the same image with `--network host`, `PGREGRESS_DIR`
//!   mounted at the same path, e.g.
//!   `docker run -d --name x --network host --user $(id -u):$(id -g) -v $D:$D --entrypoint sleep <image> infinity`.
//! - `PGREGRESS_REPORT_DIR`: where `REPORT.md`, `results.tsv` and `hunks/` go (default
//!   `tests/regress` for a full run, `$PGREGRESS_DIR/report` for a subset).
//! - A full run also regenerates STATUS.md's numbers (its `<!-- generated: ... -->` blocks:
//!   summary, differing units, per-file table with ref=exp / eng=exp) and writes the report to
//!   `tests/regress`: always when that is its report directory, and with
//!   `PGREGRESS_UPDATE_STATUS=1` when it isn't (a private run, or `PGREGRESS_REPORT_ONLY=1` over a
//!   full run's saved outputs). A subset never does. `PGREGRESS_COMMIT`: the commit the report
//!   names (default the checkout's HEAD, `+dirty` with uncommitted changes).
//! - Every run of the main suite also lists, in `REPORT.md` ("Changes against the baseline run")
//!   and on stderr, the units that differ now but matched in a baseline run and the other way
//!   round: `PGREGRESS_BASELINE=<results.tsv>` (`none`: no comparison), by default the committed
//!   `tests/regress/results.tsv`. Run info, `results.tsv` and the report name the engine's build
//!   profile (`engine_build`, from `roll-engine --version`): debug and release builds once
//!   differed in 4 units, so compare runs of one profile.
//! - `PGREGRESS_KEEP=1`: keep the reference's database, roles and tablespaces (and the engine's
//!   data directory, `$PGREGRESS_DIR/enginedata`) at the end, e.g. to dump the regression
//!   database as `tests/pg_dump.rs` does.
//!
//! The reference's database and any databases, roles and tablespaces the run created there
//! are dropped at the end.
//!
//! This file is the harness, shared by the `pg_regress` and `pg_contrib` test binaries
//! (`tests/pg_regress.rs`, `tests/pg_contrib.rs`), each running one suite.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The database both sides run in: `gxg_regression`, or `PGREGRESS_DB` (lets several runs share the
/// reference container).
/// (`contrib_regression` for the contrib modules' suites, as pg_regress names it there.)
fn db_name() -> &'static str {
    static NAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        std::env::var("PGREGRESS_DB").unwrap_or_else(|_| {
            if contrib_mode() { "contrib_regression" } else { "gxg_regression" }.into()
        })
    })
}

/// Set once by `pg_contrib` before anything runs.
static CONTRIB: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn contrib_mode() -> bool {
    CONTRIB.load(std::sync::atomic::Ordering::Relaxed)
}

/// The settings of the contrib modules' `--temp-config` files (test_decoding's
/// `logical.conf`: wal_level = logical, ...), which the engine starts with as `-c` options,
/// as the reference must.
static TEMP_CONFIG: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

fn temp_config(module_dir: &Path) -> Vec<(String, String)> {
    let mk = std::fs::read_to_string(module_dir.join("Makefile")).unwrap_or_default();
    let mut out = Vec::new();
    for w in mk.split_whitespace() {
        if let Some(rest) = w.strip_prefix("$(top_srcdir)/contrib/")
            && w.ends_with(".conf")
        {
            let path = module_dir.parent().unwrap().join(rest);
            for line in std::fs::read_to_string(path).unwrap_or_default().lines() {
                if let Some((k, v)) = line.split('#').next().unwrap_or("").split_once('=') {
                    out.push((k.trim().to_string(), v.trim().trim_matches('\'').to_string()));
                }
            }
        }
    }
    out
}
const GIT_TAG: &str = "REL_18_6";
const HUNKS_PER_FILE: usize = 12;

#[derive(Clone)]
struct Config {
    root: PathBuf,
    src: PathBuf,
    out: PathBuf,
    /// psql's working directory (a contrib module's directory: its scripts read `data/...`).
    cwd: PathBuf,
    container: String,
    port: u16,
    timeout: u64,
    only: Option<BTreeSet<String>>,
    report_only: bool,
    report_dir: PathBuf,
}

impl Config {
    fn from_env() -> Self {
        let root = PathBuf::from(
            std::env::var("PGREGRESS_DIR").unwrap_or_else(|_| "/tmp/pgregress".into()),
        );
        let src = root.join("src-repo/src/test/regress");
        let only: Option<BTreeSet<String>> = std::env::var("PGREGRESS_ONLY").ok().map(|s| {
            s.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        });
        // Only a full run's report belongs in `tests/regress` (STATUS.md's numbers are
        // regenerated from it); a subset's goes next to its outputs.
        let report_dir = std::env::var("PGREGRESS_REPORT_DIR").map_or_else(
            |_| if only.is_some() { root.join("report") } else { committed_report_dir() },
            PathBuf::from,
        );
        Config {
            out: root.join("out"),
            cwd: root.join("build"),
            src,
            root,
            container: std::env::var("PGREGRESS_CONTAINER")
                .unwrap_or_else(|_| "roll-engine-ref".into()),
            port: std::env::var("PGREGRESS_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(55600),
            timeout: std::env::var("PGREGRESS_TIMEOUT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(300),
            only,
            report_only: std::env::var("PGREGRESS_REPORT_ONLY").is_ok_and(|v| v == "1"),
            report_dir,
        }
    }

    fn builddir(&self) -> PathBuf {
        self.root.join("build")
    }
}

/// The `pg_regress` test (`tests/pg_regress.rs`).
pub fn pg_regress() {
    let cfg = Config::from_env();
    if !cfg.report_only {
        ensure_suite(&cfg);
    }
    let mut schedule = read_schedule(&cfg.src.join("parallel_schedule"));
    if let Some(only) = &cfg.only {
        schedule.retain(|t| t == "test_setup" || only.contains(t));
    }
    if !cfg.report_only {
        run_all(&cfg, &schedule);
    }
    let results: Vec<FileResult> = schedule
        .iter()
        .scan(BTreeSet::new(), |failed, t| Some(analyze(&cfg, t, failed)))
        .flatten()
        .collect();
    write_report(&cfg, &results);
    let units: usize = results.iter().map(|r| r.units).sum();
    let matched: usize = results.iter().map(|r| r.matched).sum();
    eprintln!(
        "pg_regress: {matched}/{units} units match ({:.1}%), report in {}",
        pct(matched, units),
        cfg.report_dir.join("REPORT.md").display()
    );
    // A full run's numbers go into the committed report and STATUS.md's generated blocks:
    // always when its report is `tests/regress`'s, and with `PGREGRESS_UPDATE_STATUS=1` when it
    // went elsewhere (a private run's `PGREGRESS_REPORT_DIR`, or `PGREGRESS_REPORT_ONLY=1` over
    // its saved outputs). A subset never touches them.
    let committed = committed_report_dir();
    let to_committed = cfg.report_dir == committed;
    let forced = std::env::var("PGREGRESS_UPDATE_STATUS").is_ok_and(|v| v == "1");
    if to_committed || forced {
        // (Files `run_test` can't read run nothing: collate.windows.win1252.sql isn't UTF-8.)
        let full: Vec<String> = read_schedule(&cfg.src.join("parallel_schedule"))
            .into_iter()
            .filter(|t| std::fs::read_to_string(cfg.src.join("sql").join(format!("{t}.sql"))).is_ok())
            .collect();
        if cfg.only.is_some() || results.len() != full.len() {
            assert!(
                !forced,
                "PGREGRESS_UPDATE_STATUS needs a full run's outputs ({} of {} files here)",
                results.len(),
                full.len()
            );
            return;
        }
        if !to_committed {
            let mut c = cfg.clone();
            c.report_dir = committed.clone();
            write_report(&c, &results);
        }
        update_status(&cfg, &committed.join("STATUS.md"), &results);
        eprintln!("pg_regress: full run; {} regenerated", committed.join("STATUS.md").display());
    }
}

/// `tests/regress`: the committed report and STATUS.md.
fn committed_report_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/regress")
}

/// The contrib modules whose own regression tests `pg_contrib` runs by default.
const CONTRIB_MODULES: &[&str] = &[
    "pgcrypto",
    "uuid-ossp",
    "citext",
    "hstore",
    "pg_trgm",
    "btree_gin",
    "btree_gist",
    "pg_stat_statements",
    "unaccent",
    "fuzzystrmatch",
    "ltree",
    "intarray",
    "tablefunc",
    "cube",
    "earthdistance",
    "isn",
    "seg",
    "lo",
    "amcheck",
    "pageinspect",
    "pg_buffercache",
    "file_fdw",
    "postgres_fdw",
    "dblink",
    "tsm_system_rows",
    "tsm_system_time",
    "pg_prewarm",
    "pg_freespacemap",
    "pg_visibility",
    "pgstattuple",
    "dict_int",
    "dict_xsyn",
    "auto_explain",
    "passwordcheck",
    "bloom",
    "spi",
    "xml2",
    "pg_overexplain",
];

/// Extensions outside contrib whose own regression tests run the same way: (module, git
/// repository, tag, extension). Each is cloned at its tag next to the contrib sources; its tests
/// are `test/sql/*.sql` with `test/expected` (`REGRESS_OPTS = --inputdir=test
/// --load-extension=<extension>`), psql running in the repository's root (scripts write
/// `results/`). The reference needs the extension installed (pgvector: its pinned release built
/// from source into the pinned image, `tests/contrib/STATUS.md`); a default run leaves a module
/// out when the reference doesn't have it.
const EXTERNAL_MODULES: &[(&str, &str, &str, &str)] =
    &[("pgvector", "https://github.com/pgvector/pgvector.git", "v0.8.7", "vector")];

fn external(m: &str) -> Option<&'static (&'static str, &'static str, &'static str, &'static str)> {
    EXTERNAL_MODULES.iter().find(|e| e.0 == m)
}

/// The contrib modules' own regression tests (`contrib/<module>/sql` and `expected` at
/// REL_18_6), run like `make installcheck` runs them: each module in a fresh
/// `contrib_regression` database (template0, pg_regress's settings), its `REGRESS_OPTS`
/// `--load-extension`s created first, then its `REGRESS` files in order, psql in the module's
/// directory (scripts read `data/...`). Compared unit by unit with the reference as
/// `pg_regress` is; the summary goes to `tests/contrib/REPORT.md` (or `PGREGRESS_REPORT_DIR`),
/// each module's report and hunks to `<PGREGRESS_DIR>/contrib-report/<module>/`.
///
///     PGCONTRIB_ONLY=pgcrypto,citext PGREGRESS_DIR=... cargo test -p pg-engine \
///       --test pg_contrib -- --ignored --nocapture
///
/// (`tests/pg_contrib.rs`, a test binary of its own: it never runs alongside `pg_regress`.
/// Its engine's data directory and binary copy are its own, per process.)
///
/// - `PGCONTRIB_ONLY=a,b`: these modules (default: `CONTRIB_MODULES`). `PGREGRESS_ONLY=x,y`:
///   only these files of them. `PGREGRESS_REPORT_ONLY=1`: rebuild the reports.
/// - The reference must run with the modules' `--temp-config` settings (pg_stat_statements:
///   `shared_preload_libraries = 'pg_stat_statements'`, `max_prepared_transactions = 5`), so
///   use a container of the pinned image started with them (`PGREGRESS_CONTAINER`) rather
///   than the shared `roll-engine-ref`; the module directories must be at the same absolute
///   path in it (a bind mount of `PGREGRESS_DIR`, or they are copied in).
pub fn pg_contrib() {
    CONTRIB.store(true, std::sync::atomic::Ordering::Relaxed);
    let base = Config::from_env();
    let modules: Vec<String> = std::env::var("PGCONTRIB_ONLY")
        .ok()
        .map(|s| s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect())
        .unwrap_or_else(|| {
            let mut v: Vec<String> = CONTRIB_MODULES.iter().map(|m| m.to_string()).collect();
            for e in EXTERNAL_MODULES {
                let has = psql_cmd(&base, &Target::Reference, "postgres", &format!("SELECT count(*) FROM pg_available_extensions WHERE name = '{}';", e.3));
                if has.lines().any(|l| l.trim() == "1") {
                    v.push(e.0.to_string());
                } else {
                    eprintln!("pg_contrib: {} left out: the reference has no {} extension", e.0, e.3);
                }
            }
            v
        });
    if !base.report_only {
        ensure_contrib(&base, &modules);
    }
    let module_cfg = |m: &str| {
        let mut cfg = base.clone();
        match external(m) {
            Some(e) => {
                cfg.cwd = base.root.join(format!("{}-repo", e.0));
                cfg.src = cfg.cwd.join("test");
            }
            None => {
                cfg.src = base.root.join("src-repo/contrib").join(m);
                cfg.cwd = cfg.src.clone();
            }
        }
        cfg.out = base.root.join("contrib-out").join(m);
        cfg.report_dir = base.root.join("contrib-report").join(m);
        cfg
    };
    let schedules: Vec<(String, Vec<String>, Vec<String>)> = modules
        .iter()
        .map(|m| {
            let (mut tests, load) = match external(m) {
                Some(e) => (external_tests(&module_cfg(m).src), vec![e.3.to_string()]),
                None => contrib_schedule(&module_cfg(m).src.join("Makefile")),
            };
            if let Some(only) = &base.only {
                tests.retain(|t| only.contains(t));
            }
            (m.clone(), tests, load)
        })
        .collect();
    if !base.report_only {
        for (m, _, _) in &schedules {
            TEMP_CONFIG.lock().unwrap().extend(temp_config(&module_cfg(m).src));
        }
        // (Never the main suite's: a run of each may share `PGREGRESS_DIR`.)
        let data = base.root.join(format!("contrib-enginedata-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(base.root.join("contrib-out")).unwrap();
        let log = base.root.join("contrib-out/engine.log");
        let _ = std::fs::remove_file(&log);
        let bin = base.root.join(format!("contrib-roll-engine-{}.bin", std::process::id()));
        let src = std::env::var("PGREGRESS_ENGINE_BIN")
            .unwrap_or_else(|_| env!("CARGO_BIN_EXE_roll-engine").to_string());
        std::fs::copy(&src, &bin).expect("copy roll-engine");
        let mut engine = EngineProc::start(&bin, &data, &log, base.port, &base.root);
        let eng = Target::Engine(base.port);
        let before = globals(&base);
        assert!(!before.is_empty(), "cannot reach the reference container {}", base.container);
        for (m, tests, load) in &schedules {
            let cfg = module_cfg(m);
            for d in ["ref", "engine", "prepared", "meta"] {
                let p = cfg.out.join(d);
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
            }
            let setup = setup_db_sql();
            let mut ext = String::new();
            for e in load {
                let _ = writeln!(ext, "CREATE EXTENSION IF NOT EXISTS \"{e}\";");
            }
            for target in [Target::Reference, Target::Engine(base.port)] {
                let out = psql_cmd(&cfg, &target, "postgres", &setup);
                let out2 = if ext.is_empty() { String::new() } else { psql_cmd(&cfg, &target, db_name(), &ext) };
                if out.contains("ERROR") || out2.contains("ERROR") {
                    eprintln!("{m}: database setup reported:\n{out}{out2}");
                }
            }
            eprintln!("== {m}: {}", tests.join(" "));
            for t in tests {
                run_test(&cfg, &mut engine, &log, t, &before);
            }
        }
        let _ = psql_cmd(&base, &eng, "postgres", &format!("DROP DATABASE IF EXISTS \"{}\";", db_name()));
        engine.stop();
        cleanup_reference(&base, &before);
        let _ = std::fs::remove_dir_all(&data);
        let _ = std::fs::remove_file(&bin);
    }
    let mut summary = Vec::new();
    for (m, tests, _) in &schedules {
        let cfg = module_cfg(m);
        let results: Vec<FileResult> = tests
            .iter()
            .scan(BTreeSet::new(), |failed, t| Some(analyze(&cfg, t, failed)))
            .flatten()
            .collect();
        std::fs::create_dir_all(&cfg.report_dir).unwrap();
        write_report(&cfg, &results);
        summary.push((m.clone(), results));
    }
    let report_dir = std::env::var("PGREGRESS_REPORT_DIR").map_or_else(
        |_| Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/contrib"),
        PathBuf::from,
    );
    write_contrib_report(&report_dir, &base, &summary);
    let units: usize = summary.iter().flat_map(|s| &s.1).map(|r| r.units).sum();
    let matched: usize = summary.iter().flat_map(|s| &s.1).map(|r| r.matched).sum();
    eprintln!(
        "pg_contrib: {matched}/{units} units match ({:.1}%), report in {}",
        pct(matched, units),
        report_dir.join("REPORT.md").display()
    );
}

/// The contrib sources (sparse, next to the main suite's), and each module's directory at the
/// same absolute path in the reference container.
fn ensure_contrib(cfg: &Config, modules: &[String]) {
    let repo = cfg.root.join("src-repo");
    if !repo.join("contrib").exists() {
        std::fs::create_dir_all(&cfg.root).unwrap();
        let ok = if repo.exists() {
            sh(Command::new("git").current_dir(&repo).args(["sparse-checkout", "add", "contrib"]))
        } else {
            sh(Command::new("git").args([
                "clone",
                "-q",
                "--depth",
                "1",
                "--filter=blob:none",
                "--sparse",
                "--branch",
                GIT_TAG,
                "https://github.com/postgres/postgres.git",
                repo.to_str().unwrap(),
            ])) && sh(Command::new("git").current_dir(&repo).args(["sparse-checkout", "set", "contrib"]))
        };
        assert!(ok, "could not fetch the contrib modules");
    }
    // psql's working directory for the setup statements.
    let build = cfg.builddir();
    std::fs::create_dir_all(&build).unwrap();
    let _ = sh(Command::new("docker").args([
        "exec", "-u", "root", &cfg.container, "mkdir", "-p", build.to_str().unwrap(),
    ]));
    for m in modules {
        let dir = match external(m) {
            Some(e) => {
                let dir = cfg.root.join(format!("{}-repo", e.0));
                if !dir.join("test").exists() {
                    assert!(
                        sh(Command::new("git").args(["clone", "-q", "--depth", "1", "--branch", e.2, e.1, dir.to_str().unwrap()])),
                        "could not fetch {}",
                        e.0
                    );
                }
                std::fs::create_dir_all(dir.join("results")).unwrap();
                dir
            }
            None => repo.join("contrib").join(m),
        };
        let d = dir.to_str().unwrap();
        let there = sh(Command::new("docker").args(["exec", &cfg.container, "test", "-f", &format!("{d}/Makefile")]));
        if !there {
            assert!(sh(Command::new("docker").args([
                "exec", "-u", "root", &cfg.container, "mkdir", "-p", d,
            ])));
            assert!(sh(Command::new("docker").args([
                "cp",
                &format!("{d}/."),
                &format!("{}:{d}", cfg.container),
            ])));
        }
        // (An external module's scripts write `results/` from psql's directory.)
        if external(m).is_some() {
            let _ = sh(Command::new("docker").args([
                "exec", "-u", "root", &cfg.container, "sh", "-c", &format!("mkdir -p {d}/results && chmod -R a+rwX {d}/results"),
            ]));
        }
    }
}

/// An external module's tests: `sql/*.sql` under its test directory, in name order (its
/// Makefile's `$(wildcard test/sql/*.sql)`).
fn external_tests(src: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(src.join("sql"))
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().to_str()?.strip_suffix(".sql").map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// A module's `REGRESS` files and `--load-extension`s, from its Makefile: `NAME = ...` /
/// `+=` assignments (with `\` continuations), `$(NAME)` references and `$(if c, a, b)` (the
/// configured branch: the reference is built with zlib, as pgcrypto's tests need).
fn contrib_schedule(makefile: &Path) -> (Vec<String>, Vec<String>) {
    let text = std::fs::read_to_string(makefile).unwrap_or_default().replace("\\\n", " ");
    let mut vars: HashMap<String, String> = HashMap::new();
    let mut depth = 0;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("ifdef") || t.starts_with("ifeq") || t.starts_with("ifneq") || t.starts_with("ifndef") {
            depth += 1;
            continue;
        }
        if t.starts_with("endif") {
            depth -= 1;
            continue;
        }
        if depth > 0 || t.starts_with('#') {
            continue;
        }
        let (name, val, append) = if let Some((n, v)) = t.split_once("+=") {
            (n, v, true)
        } else if let Some((n, v)) = t.split_once('=') {
            (n.trim_end_matches([':', '?']), v, false)
        } else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            continue;
        }
        let e = vars.entry(name.to_string()).or_default();
        if !append {
            e.clear();
        }
        e.push(' ');
        e.push_str(val.trim());
    }
    fn expand(s: &str, vars: &HashMap<String, String>, depth: usize) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find("$(") {
            out.push_str(&rest[..i]);
            let body = &rest[i + 2..];
            let mut level = 1;
            let mut end = body.len();
            for (k, c) in body.char_indices() {
                match c {
                    '(' => level += 1,
                    ')' => {
                        level -= 1;
                        if level == 0 {
                            end = k;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let inner = &body[..end];
            rest = body.get(end + 1..).unwrap_or("");
            if depth > 20 {
                continue;
            }
            if let Some(args) = inner.strip_prefix("if ") {
                // Top-level commas split the condition and the branches.
                let mut parts = Vec::new();
                let (mut lv, mut st) = (0, 0);
                for (k, c) in args.char_indices() {
                    match c {
                        '(' => lv += 1,
                        ')' => lv -= 1,
                        ',' if lv == 0 => {
                            parts.push(&args[st..k]);
                            st = k + 1;
                        }
                        _ => {}
                    }
                }
                parts.push(&args[st..]);
                out.push_str(&expand(parts.get(1).copied().unwrap_or(""), vars, depth + 1));
            } else if let Some(v) = vars.get(inner.trim()) {
                out.push_str(&expand(v, vars, depth + 1));
            }
        }
        out.push_str(rest);
        out
    }
    let words = |n: &str| -> Vec<String> {
        vars.get(n)
            .map(|v| expand(v, &vars, 0))
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };
    let load = words("REGRESS_OPTS")
        .iter()
        .filter_map(|w| w.strip_prefix("--load-extension=").map(str::to_string))
        .collect();
    (words("REGRESS"), load)
}

/// `tests/contrib/REPORT.md`: per module and file, units matching, and each file's first
/// differing units.
fn write_contrib_report(dir: &Path, base: &Config, summary: &[(String, Vec<FileResult>)]) {
    let mut s = String::new();
    let _ = writeln!(s, "# contrib modules' regression tests: engine parity report\n");
    let _ = writeln!(
        s,
        "Generated by `tests/pg_contrib.rs` (`pg_contrib` in `tests/harness/regress.rs` has how to run it). Sources: `contrib/<module>/sql` at `{GIT_TAG}`; reference: PostgreSQL 18.6. A unit matches when its psql output is byte-identical to the reference's. Per-module reports and hunks: `{}/contrib-report/<module>/`.\n",
        base.root.display()
    );
    let units: usize = summary.iter().flat_map(|s| &s.1).map(|r| r.units).sum();
    let matched: usize = summary.iter().flat_map(|s| &s.1).map(|r| r.matched).sum();
    let _ = writeln!(s, "**Total: {matched}/{units} units ({:.1}%).**\n", pct(matched, units));
    let _ = writeln!(s, "| module | files | full files | units | matching | % | ref == expected |");
    let _ = writeln!(s, "|---|---|---|---|---|---|---|");
    for (m, rs) in summary {
        let u: usize = rs.iter().map(|r| r.units).sum();
        let k: usize = rs.iter().map(|r| r.matched).sum();
        let _ = writeln!(
            s,
            "| {m} | {} | {} | {u} | {k} | {:.1}% | {}/{} |",
            rs.len(),
            rs.iter().filter(|r| r.units == r.matched).count(),
            pct(k, u),
            rs.iter().filter(|r| r.ref_expected == Some(true)).count(),
            rs.iter().filter(|r| r.ref_expected.is_some()).count(),
        );
    }
    for (m, rs) in summary {
        let _ = writeln!(s, "\n## {m}\n");
        let _ = writeln!(s, "| file | units | matching | first differing units |");
        let _ = writeln!(s, "|---|---|---|---|");
        for r in rs {
            let first: Vec<String> = r
                .gaps
                .iter()
                .take(4)
                .map(|g| format!("{} `{}` ({})", g.unit, md_escape(&first_line(&g.stmt)), short_class(g.class)))
                .collect();
            let _ = writeln!(s, "| {} | {} | {} | {} |", r.name, r.units, r.matched, first.join("<br>"));
        }
    }
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("REPORT.md"), s).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Fetching the suite

fn ensure_suite(cfg: &Config) {
    if !cfg.src.join("parallel_schedule").exists() {
        std::fs::create_dir_all(&cfg.root).unwrap();
        let repo = cfg.root.join("src-repo");
        let ok = sh(Command::new("git").args([
            "clone",
            "-q",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            "--branch",
            GIT_TAG,
            "https://github.com/postgres/postgres.git",
            repo.to_str().unwrap(),
        ])) && sh(Command::new("git").current_dir(&repo).args([
            "sparse-checkout",
            "set",
            "src/test/regress",
        ]));
        assert!(ok, "could not fetch the regression suite");
    }
    // Server-side COPY/lo_import paths must exist on both servers at the same absolute path.
    let src = cfg.src.to_str().unwrap();
    let build = cfg.builddir();
    std::fs::create_dir_all(build.join("results")).unwrap();
    let build = build.to_str().unwrap();
    assert!(sh(Command::new("docker").args([
        "exec",
        "-u",
        "root",
        &cfg.container,
        "sh",
        "-c",
        &format!("mkdir -p {src} {build}/results && chown -R postgres {build}"),
    ])));
    assert!(sh(Command::new("docker").args([
        "cp",
        cfg.src.join("data").to_str().unwrap(),
        &format!("{}:{src}/", cfg.container),
    ])));
}

fn sh(cmd: &mut Command) -> bool {
    cmd.status().is_ok_and(|s| s.success())
}

fn read_schedule(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .expect("parallel_schedule")
        .lines()
        .filter_map(|l| l.strip_prefix("test:"))
        .flat_map(|l| l.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Splitting a file into psql execution units

/// A psql-like lexer, line at a time: knows when psql's query buffer is empty, so a marker
/// comment can be inserted there. Follows psqlscan.l: quotes, dollar quotes, nested comments,
/// parentheses, BEGIN ... END in CREATE FUNCTION/PROCEDURE bodies, backslash commands,
/// COPY FROM STDIN data.
#[derive(Default)]
struct Lexer {
    squote: Option<bool>,
    dquote: bool,
    dollar: Option<String>,
    comment: usize,
    paren: i32,
    begin_depth: i32,
    idents: [u8; 4],
    ident_count: usize,
    nonempty: bool,
    stmt: String,
    copy_data: bool,
    no_std_strings: bool,
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

/// Backslash commands that send or discard the query buffer.
const EXECUTING: &[&str] = &[
    "g",
    "gx",
    "gset",
    "gexec",
    "gdesc",
    "crosstabview",
    "sendpipeline",
    "parse",
    "r",
    "reset",
    "watch",
];

impl Lexer {
    /// Feeds one line; true if it completed something and psql's buffer is empty after it.
    fn feed(&mut self, line: &str) -> bool {
        if self.copy_data {
            if line == "\\." {
                self.copy_data = false;
                return true;
            }
            return false;
        }
        let b = line.as_bytes();
        let mut completed = false;
        let mut seg = 0;
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            let next = b.get(i + 1).copied();
            if self.comment > 0 {
                if c == b'/' && next == Some(b'*') {
                    self.comment += 1;
                    i += 2;
                } else if c == b'*' && next == Some(b'/') {
                    self.comment -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            if let Some(esc) = self.squote {
                if esc && c == b'\\' {
                    i += 2;
                } else if c == b'\'' {
                    if next == Some(b'\'') {
                        i += 2;
                    } else {
                        self.squote = None;
                        i += 1;
                    }
                } else {
                    i += 1;
                }
                continue;
            }
            if self.dquote {
                if c == b'"' {
                    self.dquote = false;
                }
                i += 1;
                continue;
            }
            if let Some(tag) = &self.dollar {
                if line[i..].starts_with(tag.as_str()) {
                    i += tag.len();
                    self.dollar = None;
                } else {
                    i += 1;
                }
                continue;
            }
            match c {
                b' ' | b'\t' | b'\r' | b'\n' => i += 1,
                b'-' if next == Some(b'-') => break,
                b'/' if next == Some(b'*') => {
                    self.comment = 1;
                    self.nonempty = true;
                    i += 2;
                }
                b'\'' => {
                    let e_prefix = i >= 1
                        && (b[i - 1] == b'E' || b[i - 1] == b'e')
                        && (i < 2 || !is_ident(b[i - 2]));
                    self.squote = Some(self.no_std_strings || e_prefix);
                    self.nonempty = true;
                    i += 1;
                }
                b'"' => {
                    self.dquote = true;
                    self.nonempty = true;
                    i += 1;
                }
                b'$' => {
                    self.nonempty = true;
                    let prev_ident = i > 0 && is_ident(b[i - 1]);
                    let mut j = i + 1;
                    if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_' || b[j] >= 0x80)
                    {
                        while j < b.len()
                            && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80)
                        {
                            j += 1;
                        }
                    }
                    if !prev_ident && j < b.len() && b[j] == b'$' {
                        self.dollar = Some(line[i..=j].to_string());
                        i = j + 1;
                    } else {
                        i += 1;
                    }
                }
                b'(' => {
                    self.paren += 1;
                    self.nonempty = true;
                    i += 1;
                }
                b')' => {
                    if self.paren > 0 {
                        self.paren -= 1;
                    }
                    self.nonempty = true;
                    i += 1;
                }
                b';' => {
                    if self.paren == 0 && self.begin_depth == 0 {
                        self.stmt.push_str(&line[seg..i]);
                        self.end_statement();
                        seg = i + 1;
                        completed = true;
                    } else {
                        self.nonempty = true;
                    }
                    i += 1;
                }
                b'\\' => {
                    if matches!(next, Some(b';') | Some(b':')) {
                        self.nonempty = true;
                        i += 2;
                        continue;
                    }
                    let rest = &line[i..];
                    let names: Vec<String> = rest
                        .split_whitespace()
                        .filter_map(|t| t.strip_prefix('\\'))
                        .map(|t| {
                            t.chars()
                                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                                .collect::<String>()
                        })
                        .collect();
                    let was_empty = !self.nonempty;
                    let executes = names.iter().any(|n| EXECUTING.contains(&n.as_str()));
                    if executes {
                        self.stmt.clear();
                        self.reset();
                    }
                    if executes || was_empty {
                        completed = true;
                    }
                    if names.first().is_some_and(|n| n == "copy") && copy_from_stdin(rest) {
                        self.copy_data = true;
                    }
                    seg = b.len();
                    break;
                }
                c if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                    let start = i;
                    while i < b.len() && is_ident(b[i]) {
                        i += 1;
                    }
                    self.nonempty = true;
                    // E'...' is handled at the quote; here only the BEGIN ... END tracking.
                    self.identifier(&line[start..i]);
                }
                c if c.is_ascii_digit() => {
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.') {
                        i += 1;
                    }
                    self.nonempty = true;
                }
                _ => {
                    self.nonempty = true;
                    i += 1;
                }
            }
        }
        if seg < line.len() {
            self.stmt.push_str(&line[seg..]);
        }
        self.stmt.push('\n');
        if self.copy_data {
            return false;
        }
        completed && !self.nonempty && self.idle()
    }

    /// psql's buffer is empty and it's reading commands (not COPY data).
    fn at_rest(&self) -> bool {
        !self.nonempty && !self.copy_data && self.idle()
    }

    fn idle(&self) -> bool {
        self.squote.is_none() && !self.dquote && self.dollar.is_none() && self.comment == 0
    }

    fn identifier(&mut self, word: &str) {
        let w = word.to_ascii_lowercase();
        if self.ident_count == 0 {
            self.idents = [0; 4];
        }
        if matches!(
            w.as_str(),
            "create" | "function" | "procedure" | "or" | "replace"
        ) && self.ident_count < 4
        {
            self.idents[self.ident_count] = w.as_bytes()[0];
        }
        self.ident_count += 1;
        let id = self.idents;
        let in_fn = id[0] == b'c'
            && (id[1] == b'f'
                || id[1] == b'p'
                || (id[1] == b'o' && id[2] == b'r' && (id[3] == b'f' || id[3] == b'p')));
        if in_fn && self.paren == 0 {
            match w.as_str() {
                "begin" => self.begin_depth += 1,
                "case" if self.begin_depth >= 1 => self.begin_depth += 1,
                "end" if self.begin_depth > 0 => self.begin_depth -= 1,
                _ => {}
            }
        }
    }

    fn end_statement(&mut self) {
        let s = std::mem::take(&mut self.stmt);
        let words = words(&s);
        if words.first().is_some_and(|w| w == "copy") && copy_from_stdin(&s) {
            self.copy_data = true;
        }
        // psql follows standard_conforming_strings through ParameterStatus.
        let w: Vec<&str> = words.iter().map(String::as_str).collect();
        match w.as_slice() {
            ["set", .., "standard_conforming_strings", v] | ["set", .., "standard_conforming_strings", "to", v] => {
                self.no_std_strings = matches!(*v, "off" | "false" | "0");
            }
            ["reset", "standard_conforming_strings"] | ["reset", "all"] => {
                self.no_std_strings = false;
            }
            _ => {}
        }
        self.reset();
    }

    fn reset(&mut self) {
        self.nonempty = false;
        self.paren = 0;
        self.begin_depth = 0;
        self.ident_count = 0;
    }
}

/// Lowercased words of a statement, comments dropped.
fn words(s: &str) -> Vec<String> {
    let no_comments: String = s
        .lines()
        .map(|l| match l.find("--") {
            Some(p) => &l[..p],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    no_comments
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn copy_from_stdin(s: &str) -> bool {
    let w = words(s);
    w.windows(2)
        .any(|p| p[0] == "from" && (p[1] == "stdin" || p[1] == "pstdin"))
}

struct Prepared {
    /// The file with markers.
    text: String,
    /// The source lines of each unit and the line it starts on.
    units: Vec<(usize, String)>,
    /// (original line, replacement) for server-side COPY files turned into `\copy`.
    rewrites: Vec<(String, String)>,
}

fn marker(n: usize) -> String {
    format!("-- @@@ {n}")
}

fn prepare(src: &str, env: &HashMap<&str, String>) -> Prepared {
    let mut lx = Lexer::default();
    let mut text = String::new();
    let mut units = Vec::new();
    let mut rewrites = Vec::new();
    let mut vars: HashMap<String, String> = HashMap::new();
    let mut client_files: BTreeSet<String> = BTreeSet::new();
    let mut cur = String::new();
    let mut start = 1;
    for (no, line) in src.lines().enumerate() {
        let mut line = rename_db(line);
        if lx.at_rest() {
            track_vars(&line, env, &mut vars);
            if let Some(r) = copy_to_client(&line, &vars, &client_files) {
                rewrites.push((line.clone(), r.clone()));
                line = r;
            }
        }
        client_written(&line, &vars, &mut client_files);
        if cur.trim().is_empty() && !cur.contains("/*") {
            start = no + 1;
        }
        text.push_str(&line);
        text.push('\n');
        cur.push_str(&line);
        cur.push('\n');
        if lx.feed(&line) {
            text.push_str(&marker(units.len()));
            text.push('\n');
            units.push((start, std::mem::take(&mut cur)));
        }
    }
    if cur.lines().any(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with("--")
    }) {
        units.push((start, cur));
    }
    Prepared {
        text,
        units,
        rewrites,
    }
}

/// Follows `\getenv` and simple `\set` so file names in `:'filename'` are known.
fn track_vars(line: &str, env: &HashMap<&str, String>, vars: &mut HashMap<String, String>) {
    let t = line.trim();
    if let Some(rest) = t.strip_prefix("\\getenv ") {
        let mut it = rest.split_whitespace();
        if let (Some(v), Some(e)) = (it.next(), it.next()) {
            match env.get(e) {
                Some(val) => vars.insert(v.to_string(), val.clone()),
                None => vars.remove(v),
            };
        }
        return;
    }
    let Some(rest) = t.strip_prefix("\\set ") else {
        return;
    };
    let rest = rest.trim_start();
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let b: Vec<char> = args.chars().collect();
    let mut val = String::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == ':' && b.get(i + 1).is_some_and(|c| c.is_alphanumeric() || *c == '_') {
            let st = i + 1;
            i += 1;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            let v: String = b[st..i].iter().collect();
            match vars.get(&v) {
                Some(x) => val.push_str(x),
                None => {
                    vars.remove(name);
                    return;
                }
            }
        } else if c == '\'' {
            i += 1;
            while i < b.len() {
                if b[i] == '\'' {
                    if b.get(i + 1) == Some(&'\'') {
                        val.push('\'');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                val.push(b[i]);
                i += 1;
            }
        } else if c == ':' || c == '`' {
            vars.remove(name);
            return;
        } else {
            while i < b.len() && !b[i].is_whitespace() && b[i] != '\'' {
                val.push(b[i]);
                i += 1;
            }
        }
    }
    vars.insert(name.to_string(), val);
}

/// Files psql itself writes (`\o`, `\g`, `\w`, `\lo_export`, `\copy`): their names, as far
/// as they can be resolved (`:var`, `:'var'`, a literal).
fn client_written(line: &str, vars: &HashMap<String, String>, out: &mut BTreeSet<String>) {
    const WRITERS: &[&str] = &["o", "out", "g", "gx", "w", "write", "lo_export", "copy"];
    let mut rest = line;
    while let Some(i) = rest.find('\\') {
        let after = &rest[i + 1..];
        let cmd: String = after.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        rest = &after[cmd.len()..];
        if !WRITERS.contains(&cmd.as_str()) || !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let args = rest.split('\\').next().unwrap_or("");
        for tok in args.split_whitespace() {
            let name = tok.trim_end_matches(';');
            let val = if let Some(v) = name.strip_prefix(":'").and_then(|v| v.strip_suffix('\'')) {
                vars.get(v).cloned()
            } else if let Some(v) = name.strip_prefix(':') {
                vars.get(v).cloned()
            } else {
                name.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')).map(str::to_string)
            };
            out.extend(val);
        }
    }
}

/// One-line `COPY x FROM/TO :'file' ...;` statements on a file psql wrote earlier (`\o`,
/// `\g`, ...) become psql `\copy` on both sides: psql runs in the reference container, so
/// the engine's server would see a different file (the engine reaches server files in the
/// suite's directory on the host). The echoed line changes accordingly.
fn copy_to_client(
    line: &str,
    vars: &HashMap<String, String>,
    client_files: &BTreeSet<String>,
) -> Option<String> {
    let t = line.trim();
    if t.len() < 5 || !t[..5].eq_ignore_ascii_case("copy ") || !t.ends_with(';') {
        return None;
    }
    if t.matches(';').count() != 1 || t.contains("--") {
        return None;
    }
    let p = t.find(":'")?;
    let end = p + 2 + t[p + 2..].find('\'')?;
    let var = &t[p + 2..end];
    let val = vars.get(var).filter(|v| client_files.contains(*v))?;
    let before = t[5..p].trim_end();
    let lw = before.to_ascii_lowercase();
    if !(lw.ends_with(" from") || lw.ends_with(" to")) || t[end + 1..].contains(":'") {
        return None;
    }
    let after = t[end + 1..t.len() - 1].trim_end();
    Some(format!(
        "\\copy {before} '{}'{after}",
        val.replace('\'', "''")
    ))
}

/// `regression` is the database's name in a few statements (GRANT ... ON DATABASE regression,
/// psql's `\d host.regression.x` patterns); the database here is `gxg_regression`.
fn rename_db(line: &str) -> String {
    if contrib_mode() {
        return line.to_string();
    }
    if !(line.starts_with('\\') || line.to_ascii_lowercase().contains("database regression")) {
        return line.to_string();
    }
    let b = line.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < line.len() {
        if line[i..].starts_with("regression")
            && (i == 0 || !is_ident(b[i - 1]))
            && b.get(i + 10).is_none_or(|c| !is_ident(*c))
        {
            out.push_str(db_name());
            i += 10;
        } else {
            let ch = line[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Cuts psql's output at the echoed markers: one entry per unit, `None` where psql never got
/// there (it exited, crashed or timed out). `marked` units end in a marker; a last unit without
/// one (a statement at EOF without a semicolon) takes the rest of the output.
fn split_output(out: &str, marked: usize, n: usize) -> Vec<Option<Vec<String>>> {
    let mut blocks: Vec<Option<Vec<String>>> = vec![None; n];
    let mut cur: Vec<String> = Vec::new();
    for line in out.lines() {
        if let Some(k) = line
            .strip_prefix("-- @@@ ")
            .and_then(|k| k.parse::<usize>().ok())
        {
            if k < marked {
                blocks[k] = Some(std::mem::take(&mut cur));
            }
            continue;
        }
        cur.push(line.to_string());
    }
    if n > marked && (marked == 0 || blocks[marked - 1].is_some()) {
        blocks[marked] = Some(cur);
    }
    blocks
}

// ---------------------------------------------------------------------------------------------
// Running

struct EngineProc {
    child: Child,
    bin: PathBuf,
    data: PathBuf,
    log: PathBuf,
    port: u16,
    files: PathBuf,
}

impl EngineProc {
    /// `files`: the directory server-side files are allowed in.
    fn start(bin: &Path, data: &Path, log: &Path, port: u16, files: &Path) -> Self {
        let logf = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        let child = Command::new(bin)
            .args([
                "--data",
                data.to_str().unwrap(),
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--no-sync",
                "--idle-close-secs",
                "100000",
                "--allow-server-files",
                files.to_str().unwrap(),
            ])
            // (The contrib suites' reference preloads pg_stat_statements, as its
            // temp-config does: so does the engine; and takes its max_prepared_transactions.)
            .args(if contrib_mode() {
                &["--shared-preload-libraries", "pg_stat_statements", "--max-prepared-transactions", "5"][..]
            } else {
                &[][..]
            })
            // (dblink's and postgres_fdw's suites connect to a loopback server: the engine
            // itself, reached without a host as through PostgreSQL's Unix socket.)
            .args(if contrib_mode() { &["--allow-outbound", "local"][..] } else { &[][..] })
            .args(
                TEMP_CONFIG
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(k, _)| k != "shared_preload_libraries" && k != "max_prepared_transactions")
                    .flat_map(|(k, v)| ["-c".to_string(), format!("{k}={v}")]),
            )
            .env("RUST_LOG", "warn")
            .env("RUST_BACKTRACE", "0")
            .stdout(logf.try_clone().unwrap())
            .stderr(logf)
            .spawn()
            .expect("start roll-engine");
        // A loaded machine (parallel builds, a debug binary) can take well over 30s.
        let deadline = Instant::now() + Duration::from_secs(180);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "roll-engine did not start");
            std::thread::sleep(Duration::from_millis(100));
        }
        // What misc_functions reads of the data directory: the lock file Postgres keeps there
        // (its layout: pid, data directory, start time, port, socket directory, listen
        // address, shared memory key, status) and `base/`.
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        std::fs::write(
            files.join("postmaster.pid"),
            format!(
                "{}\n{}\n{started}\n{port}\n\n127.0.0.1\n  {port}001         0\nready   \n",
                child.id(),
                data.display()
            ),
        )
        .unwrap();
        std::fs::create_dir_all(files.join("base")).unwrap();
        EngineProc {
            child,
            bin: bin.to_path_buf(),
            data: data.to_path_buf(),
            log: log.to_path_buf(),
            port,
            files: files.to_path_buf(),
        }
    }

    fn restart(&mut self) {
        self.stop();
        *self = EngineProc::start(&self.bin, &self.data, &self.log, self.port, &self.files);
    }

    /// A clean shutdown (SIGTERM: sequences keep their positions, as after a Postgres
    /// shutdown, so a database kept with PGREGRESS_KEEP dumps the reference's values), killed
    /// if it doesn't end within 20 s.
    fn stop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for EngineProc {
    fn drop(&mut self) {
        self.stop();
    }
}

enum Target {
    Reference,
    Engine(u16),
}

/// Runs psql in the container on `input`, pg_regress style. Returns (output, status).
fn psql(
    cfg: &Config,
    target: &Target,
    db: &str,
    app: &str,
    input: &[u8],
    timeout: u64,
) -> (String, &'static str) {
    // (PGREGRESS_ENGINE_CONTAINER: psql for the engine runs in that container, on the host's
    // network; PGREGRESS_ENGINE_HOST: where the container reaches the engine, when not
    // host.docker.internal, e.g. a relay in its network namespace on a host whose firewall
    // drops the bridge.)
    let engine_container = std::env::var("PGREGRESS_ENGINE_CONTAINER").ok();
    let (host, port, container) = match target {
        Target::Reference => ("127.0.0.1".to_string(), 5432, &cfg.container),
        Target::Engine(p) => match &engine_container {
            Some(c) => ("127.0.0.1".to_string(), *p, c),
            None => (
                std::env::var("PGREGRESS_ENGINE_HOST").unwrap_or_else(|_| "host.docker.internal".into()),
                *p,
                &cfg.container,
            ),
        },
    };
    let build = cfg.cwd.to_str().unwrap().to_string();
    let mut envs = vec![
        format!("PGHOST={host}"),
        format!("PGPORT={port}"),
        "PGUSER=postgres".into(),
        "PGPASSWORD=pw".into(),
        format!("PGAPPNAME={app}"),
        "PGTZ=America/Los_Angeles".into(),
        "PGDATESTYLE=Postgres, MDY".into(),
        // (The contrib suites' servers preload pg_stat_statements, which turns query ids on:
        // the other modules' own test configurations don't load it, so their EXPLAIN
        // VERBOSE shows no Query Identifier.)
        if contrib_mode() && !cfg.src.ends_with("pg_stat_statements") {
            "PGOPTIONS=-c intervalstyle=postgres_verbose -c compute_query_id=off".into()
        } else {
            "PGOPTIONS=-c intervalstyle=postgres_verbose".into()
        },
        "LC_MESSAGES=C".into(),
    ];
    let mut regress: Vec<_> = regress_env(cfg).into_iter().collect();
    regress.sort();
    envs.extend(regress.into_iter().map(|(k, v)| format!("{k}={v}")));
    let mut cmd = Command::new("docker");
    cmd.args(["exec", "-i"]);
    for e in &envs {
        cmd.args(["-e", e]);
    }
    cmd.args([
        container,
        "sh",
        "-c",
        &format!(
            "cd {build} && exec timeout -s KILL {cap} psql -X -a -q -d {db} -v HIDE_TABLEAM=on -v HIDE_TOAST_COMPRESSION=on 2>&1",
            cap = file_timeout(timeout),
        ),
    ]);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("docker exec");
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let mut stdout = child.stdout.take().unwrap();
    // psql -a echoes and flushes every statement as it starts it, so output that stops
    // coming for `timeout` seconds is one statement stalling, however long the file runs.
    let started = Instant::now();
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let reader = {
        let progress = progress.clone();
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let mut buf = [0u8; 8192];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                v.extend_from_slice(&buf[..n]);
                progress.store(started.elapsed().as_secs(), std::sync::atomic::Ordering::Relaxed);
            }
            v
        })
    };
    let mut stalled = false;
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        let elapsed = started.elapsed().as_secs();
        let idle = elapsed.saturating_sub(progress.load(std::sync::atomic::Ordering::Relaxed));
        if idle > timeout || elapsed > file_timeout(timeout) + 30 {
            stalled = true;
            let _ = child.kill();
            break child.wait().unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    if stalled && matches!(target, Target::Reference) {
        // Killing `docker exec` leaves psql running in the container: end its session.
        psql(
            cfg,
            target,
            "postgres",
            "pg_regress",
            format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = '{app}';\n").as_bytes(),
            20,
        );
    }
    let _ = writer.join();
    let out = String::from_utf8_lossy(&reader.join().unwrap()).into_owned();
    let st = match status.code() {
        _ if stalled => "timeout",
        Some(0) => "ok",
        Some(124) | Some(137) | None => "timeout",
        Some(2) => "connection lost",
        Some(3) => "script error",
        Some(_) => "error",
    };
    (out, st)
}

/// The cap on a whole run of a file (`PGREGRESS_FILE_TIMEOUT`, default 3600 s, at least the
/// per-statement `timeout`): `join` alone takes about 400 s on a loaded machine.
fn file_timeout(timeout: u64) -> u64 {
    std::env::var("PGREGRESS_FILE_TIMEOUT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3600)
        .max(timeout)
}

/// The variables pg_regress exports for the scripts (`\getenv abs_srcdir PG_ABS_SRCDIR`).
fn regress_env(cfg: &Config) -> HashMap<&'static str, String> {
    HashMap::from([
        ("PG_ABS_SRCDIR", cfg.src.to_str().unwrap().to_string()),
        ("PG_ABS_BUILDDIR", cfg.builddir().to_str().unwrap().to_string()),
        ("PG_LIBDIR", "/usr/local/lib/postgresql".to_string()),
        ("PG_DLSUFFIX", ".so".to_string()),
    ])
}

fn psql_cmd(cfg: &Config, target: &Target, db: &str, sql: &str) -> String {
    psql(cfg, target, db, "pg_regress", sql.as_bytes(), 60).0
}

fn setup_db_sql() -> String {
    let db = db_name();
    let mut s = format!(
        "SET client_min_messages = warning;\nDROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);\nCREATE DATABASE \"{db}\" TEMPLATE=template0 ENCODING='UTF8' LOCALE='C';\n"
    );
    for set in [
        "lc_messages TO 'C'",
        "lc_monetary TO 'C'",
        "lc_numeric TO 'C'",
        "lc_time TO 'C'",
        "bytea_output TO 'hex'",
        "timezone_abbreviations TO 'Default'",
    ] {
        let _ = writeln!(s, "ALTER DATABASE \"{db}\" SET {set};");
    }
    s
}

fn globals(cfg: &Config) -> BTreeSet<String> {
    let q = "SELECT 'db:' || datname FROM pg_database UNION ALL SELECT 'role:' || rolname FROM pg_roles UNION ALL SELECT 'ts:' || spcname FROM pg_tablespace;";
    let out = psql(
        cfg,
        &Target::Reference,
        "postgres",
        "pg_regress",
        format!("\\pset tuples_only on\n\\pset format unaligned\n{q}\n").as_bytes(),
        60,
    )
    .0;
    out.lines()
        .filter(|l| l.starts_with("db:") || l.starts_with("role:") || l.starts_with("ts:"))
        .map(str::to_string)
        .collect()
}

/// The databases this run's own statements create (`CREATE DATABASE x`, `ALTER DATABASE ...
/// RENAME TO x`; names from psql variables or format strings aren't creations): the only
/// databases besides `PGREGRESS_DB` its cleanup drops. Other runs sharing the reference keep
/// theirs, whatever their names (the cleanup used to drop every `regress_*` database that
/// appeared during the run, other runs' `PGREGRESS_DB`s too: "connection lost" there).
static CREATED_DBS: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());

fn created_databases(sql: &str, out: &mut BTreeSet<String>) {
    let toks: Vec<&str> = sql
        .split(|c: char| c.is_whitespace() || c == ';' || c == '(' || c == ',')
        .filter(|t| !t.is_empty())
        .collect();
    let name = |t: &str| -> Option<String> {
        if let Some(q) = t.strip_prefix('"').and_then(|q| q.strip_suffix('"')) {
            return (!q.is_empty()).then(|| q.to_string());
        }
        t.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
            .then(|| t.to_ascii_lowercase())
    };
    for (i, w) in toks.windows(3).enumerate() {
        let renamed = w[0].eq_ignore_ascii_case("rename")
            && w[1].eq_ignore_ascii_case("to")
            && i >= 2
            && toks[i - 2].eq_ignore_ascii_case("database");
        if (w[0].eq_ignore_ascii_case("create") && w[1].eq_ignore_ascii_case("database")) || renamed {
            out.extend(name(w[2]));
        }
    }
}

#[test]
fn created_database_names() {
    let mut s = BTreeSet::new();
    created_databases(
        "CREATE DATABASE regression_tbd\n\tENCODING utf8;\nALTER DATABASE regression_tbd RENAME TO regression_utf8;\n\
         ALTER DATABASE :\"datname\" SET x = 1;\ncreate database \"OddName\";\nCREATE DATABASE regress_nosuch_db(x);\n\
         SELECT 'create database is';",
        &mut s,
    );
    let want = ["OddName", "regress_nosuch_db", "regression_tbd", "regression_utf8"];
    assert_eq!(s, want.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>());
}

fn cleanup_reference(cfg: &Config, before: &BTreeSet<String>) {
    let after = globals(cfg);
    let mut sql = String::from("SET client_min_messages = warning;\n");
    let db = db_name();
    let _ = writeln!(sql, "DROP DATABASE IF EXISTS \"{db}\" WITH (FORCE);");
    // Only the databases this run's statements created (`CREATED_DBS`): other runs sharing
    // the container (their `PGREGRESS_DB`, probe databases) stay.
    let created = CREATED_DBS.lock().unwrap().clone();
    for g in after.difference(before) {
        if let Some(d) = g.strip_prefix("db:")
            && created.contains(d)
        {
            let _ = writeln!(sql, "DROP DATABASE IF EXISTS \"{d}\" WITH (FORCE);");
        }
    }
    // Tablespaces and roles are cluster-global: while another run is still going (its
    // `PGREGRESS_DB`, `regression*`, is there) the ones that appeared may be its (test_setup's
    // `regress_tblspace`, which its tablespace and stats files then lack): the last run to end
    // drops them.
    let others = after.iter().any(|g| {
        g.strip_prefix("db:")
            .is_some_and(|d| {
                d != db
                    && !created.contains(d)
                    && (d.starts_with("regression") || d.starts_with("regress_") || d.starts_with("contrib_regression"))
            })
    });
    if others {
        eprintln!("cleanup: another run is using the reference; its tablespaces and roles stay");
    }
    for g in after.difference(before).filter(|_| !others) {
        if let Some(t) = g.strip_prefix("ts:") {
            let _ = writeln!(sql, "DROP TABLESPACE IF EXISTS \"{t}\";");
        }
    }
    for g in after.difference(before).filter(|_| !others) {
        if let Some(r) = g.strip_prefix("role:") {
            let _ = writeln!(
                sql,
                "DROP OWNED BY \"{r}\" CASCADE;\nDROP ROLE IF EXISTS \"{r}\";"
            );
        }
    }
    let out = psql_cmd(cfg, &Target::Reference, "postgres", &sql);
    if out.contains("ERROR") {
        eprintln!("cleanup on the reference reported:\n{out}");
    }
}

/// Tablespaces are cluster-global: when another run sharing the reference cluster already made
/// test_setup's `regress_tblspace`, the reference's `CREATE TABLESPACE` fails where a fresh
/// cluster's succeeds silently. Drop just that error line (the engine is still compared with the
/// fresh cluster's output); the run's tablespace is then the other run's.
fn shared_tablespace(test: &str, before: &BTreeSet<String>, out: String) -> String {
    const ERR: &str = "ERROR:  tablespace \"regress_tblspace\" already exists\n";
    if test != "test_setup" || !before.contains("ts:regress_tblspace") || !out.contains(ERR) {
        return out;
    }
    eprintln!("test_setup: regress_tblspace already existed on the reference (another run's)");
    out.replacen(ERR, "", 1)
}

fn last_marker(out: &str) -> Option<usize> {
    out.lines()
        .filter_map(|l| l.strip_prefix("-- @@@ ")?.parse::<usize>().ok())
        .max()
}

/// Runs one prepared file on the engine. If the engine crashes (or a statement hangs), psql
/// gives up; the server is restarted and the file resumes after the failing unit in a new
/// session, with the psql variables and simple session SETs replayed silently. Returns
/// (output, statuses, resumes, restarted).
fn run_engine_file(
    cfg: &Config,
    engine: &mut EngineProc,
    app: &str,
    text: &str,
    ref_out: &str,
) -> (String, String, usize, bool) {
    const MAX_RESUMES: usize = 25;
    const MAX_TIMEOUTS: usize = 2;
    let eng = Target::Engine(cfg.port);
    // Units, each with its marker line.
    let mut chunks: Vec<String> = vec![String::new()];
    for line in text.lines() {
        chunks.last_mut().unwrap().push_str(line);
        chunks.last_mut().unwrap().push('\n');
        if line.starts_with("-- @@@ ") {
            chunks.push(String::new());
        }
    }
    let marked = chunks.len() - 1;
    let ref_last = last_marker(ref_out);
    let mut out = String::new();
    let mut statuses: Vec<&str> = Vec::new();
    let mut restarted = false;
    let mut from = 0;
    loop {
        let input = if from == 0 {
            text.to_string()
        } else {
            let mut pro = String::from("\\set ECHO none\n");
            for l in chunks[..from].iter().flat_map(|c| c.lines()) {
                let t = l.trim_start();
                let lw = t.to_ascii_lowercase();
                let meta = ["\\getenv ", "\\set ", "\\unset ", "\\pset ", "\\c ", "\\connect "]
                    .iter()
                    .any(|p| t.starts_with(p));
                let set = (lw.starts_with("set ") || lw.starts_with("reset "))
                    && !lw.starts_with("set local")
                    && !lw.starts_with("set transaction")
                    && !lw.starts_with("set constraints")
                    && lw.trim_end().ends_with(';')
                    && lw.matches(';').count() == 1;
                if (meta && !t.starts_with("\\set ECHO")) || set {
                    pro.push_str(l);
                    pro.push('\n');
                }
            }
            pro.push_str("\\set ECHO all\n-- @@@ R\n");
            pro + &chunks[from..].concat()
        };
        let (o, st) = psql(cfg, &eng, db_name(), app, input.as_bytes(), cfg.timeout);
        let o = if from == 0 {
            o
        } else {
            match o.find("-- @@@ R\n") {
                Some(p) => o[p + "-- @@@ R\n".len()..].to_string(),
                None => String::new(),
            }
        };
        out.push_str(&o);
        statuses.push(st);
        // (Without running a statement in contrib mode: pg_stat_statements would count it.)
        let alive = engine.child.try_wait().ok().flatten().is_none()
            && if contrib_mode() {
                psql(cfg, &eng, db_name(), "pg_regress", b"\\echo alive\n", 20).1 == "ok"
            } else {
                psql(cfg, &eng, db_name(), "pg_regress", b"SELECT 1;\n", 20).0.contains("(1 row)")
            };
        if !alive || st == "timeout" {
            if !alive {
                *statuses.last_mut().unwrap() = "server died";
            }
            engine.restart();
            restarted = true;
        }
        // The unit psql stopped in.
        let failed = last_marker(&out).map_or(0, |k| k + 1);
        let resumable = matches!(st, "timeout" | "connection lost") || !alive;
        if !resumable
            || failed + 1 >= marked
            || ref_last.is_none_or(|r| r <= failed)
            || statuses.len() > MAX_RESUMES
            || statuses.iter().filter(|s| **s == "timeout").count() > MAX_TIMEOUTS
        {
            break;
        }
        // Close the failed unit and carry on after it.
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&marker(failed));
        out.push('\n');
        from = failed + 1;
    }
    let resumes = statuses.len() - 1;
    (out, statuses.join("+"), resumes, restarted)
}

fn run_all(cfg: &Config, schedule: &[String]) {
    for d in ["ref", "engine", "prepared", "meta"] {
        let p = cfg.out.join(d);
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
    }
    let data = cfg.root.join("enginedata");
    let _ = std::fs::remove_dir_all(&data);
    std::fs::create_dir_all(&data).unwrap();
    let log = cfg.out.join("engine.log");
    let _ = std::fs::remove_file(&log);
    // A private copy: other builds may relink target/debug/roll-engine during the run.
    // (PGREGRESS_ENGINE_BIN: another engine binary, e.g. one copied aside while the run
    // waits for a heavy slot.)
    let bin = cfg.root.join("roll-engine.bin");
    let src = std::env::var("PGREGRESS_ENGINE_BIN").unwrap_or_else(|_| env!("CARGO_BIN_EXE_roll-engine").to_string());
    std::fs::copy(&src, &bin).expect("copy roll-engine");
    let mut engine = EngineProc::start(&bin, &data, &log, cfg.port, &cfg.root);
    let eng = Target::Engine(cfg.port);

    let before = globals(cfg);
    assert!(
        !before.is_empty(),
        "cannot reach the reference container {}",
        cfg.container
    );
    let setup = setup_db_sql();
    eprintln!("{}", psql_cmd(cfg, &Target::Reference, "postgres", &setup));
    eprintln!("{}", psql_cmd(cfg, &eng, "postgres", &setup));
    write_run_info(cfg, &src, &before);

    for test in schedule {
        run_test(cfg, &mut engine, &log, test, &before);
    }
    engine.stop();
    if std::env::var("PGREGRESS_KEEP").is_ok_and(|v| v == "1") {
        eprintln!("PGREGRESS_KEEP: the reference's database, roles and tablespaces stay");
    } else {
        cleanup_reference(cfg, &before);
    }
}

/// What a run measured, for the report: `$PGREGRESS_DIR/out/run.txt` (kept with the outputs, so
/// `PGREGRESS_REPORT_ONLY=1` reports the run that made them). `commit` is the checkout's HEAD
/// (`+dirty` with uncommitted changes to tracked files), or `PGREGRESS_COMMIT` when set (say,
/// for a `PGREGRESS_ENGINE_BIN` built from another tree).
fn write_run_info(cfg: &Config, engine_bin: &str, before: &BTreeSet<String>) {
    let git = |args: &[&str]| -> Option<String> {
        let o = Command::new("git").current_dir(env!("CARGO_MANIFEST_DIR")).args(args).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let commit = std::env::var("PGREGRESS_COMMIT").ok().or_else(|| {
        let head = git(&["rev-parse", "--short=7", "HEAD"])?;
        let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        Some(if dirty { format!("{head}+dirty") } else { head })
    });
    let date = Command::new("date")
        .args(["-u", "+%Y-%m-%d %H:%M UTC"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    // Leftovers of other runs (or killed ones) that the reference already had: cluster-global
    // objects the suite makes, and other runs' databases.
    let preexisting: Vec<&str> = before
        .iter()
        .map(String::as_str)
        .filter(|g| {
            g.strip_prefix("role:").is_some_and(|r| r.starts_with("regress"))
                || g.strip_prefix("ts:").is_some_and(|t| t.starts_with("regress"))
                || g.strip_prefix("db:").is_some_and(|d| d.starts_with("regress") && d != db_name())
        })
        .collect();
    let s = format!(
        "commit={}\ndate={date}\nreference={}\nengine_bin={engine_bin}\nengine_build={}\nschedule={}\ntimeout={}\nreference_preexisting={}\n",
        commit.unwrap_or_else(|| "?".into()),
        cfg.container,
        engine_build(engine_bin),
        if cfg.only.is_some() { "subset" } else { "full" },
        cfg.timeout,
        preexisting.join(" "),
    );
    std::fs::write(cfg.out.join("run.txt"), s).unwrap();
}

/// The engine binary's build profile, `debug` or `release`, from `roll-engine --version`
/// (`unknown` for binaries older than that). Runs of the two profiles may differ where a limit
/// depends on frame sizes: release builds once lost 4 units (json / jsonb's "stack depth limit
/// exceeded" HINT) that debug builds matched, so compare runs of one profile.
fn engine_build(bin: &str) -> &'static str {
    let out = Command::new(bin).arg("--version").output();
    let v = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    if v.contains("(debug build)") {
        "debug"
    } else if v.contains("(release build)") {
        "release"
    } else {
        "unknown"
    }
}

/// Per file of a `results.tsv`: its units and its differing units.
type BaselineFiles = BTreeMap<String, (usize, BTreeSet<usize>)>;

/// A `results.tsv`'s header (`# key=value` lines) and its per-file units and differing units.
fn read_results_tsv(text: &str) -> (BTreeMap<String, String>, BaselineFiles) {
    let mut head = BTreeMap::new();
    let mut files = BTreeMap::new();
    for l in text.lines() {
        if let Some(h) = l.strip_prefix("# ") {
            if let Some((k, v)) = h.split_once('=') {
                head.insert(k.to_string(), v.to_string());
            }
            continue;
        }
        let f: Vec<&str> = l.split('\t').collect();
        let Some(units) = f.get(1).and_then(|u| u.parse().ok()) else { continue };
        let differing = f
            .get(7)
            .unwrap_or(&"")
            .split(',')
            .filter_map(|d| d.split(':').next()?.parse().ok())
            .collect();
        files.insert(f[0].to_string(), (units, differing));
    }
    (head, files)
}

/// The units that changed against an earlier run of the main suite: `PGREGRESS_BASELINE` (a
/// `results.tsv`; `none` for no comparison), by default the committed `tests/regress/results.tsv`
/// (the last full run). Files in both runs only; a subset's files see fewer earlier files'
/// catalogs than a full run's, so compare like for like. This is how a regression since the
/// last full run shows up, and how a run that differs from another of the same commit (a
/// reference set up differently, another build profile, a flaky unit) shows which units.
fn baseline_changes(cfg: &Config, results: &[FileResult]) -> Option<String> {
    if !cfg.src.ends_with("src/test/regress") {
        return None;
    }
    let path = match std::env::var("PGREGRESS_BASELINE") {
        Ok(p) if p == "none" => return None,
        Ok(p) => PathBuf::from(p),
        Err(_) => committed_report_dir().join("results.tsv"),
    };
    let shown = path.strip_prefix(env!("CARGO_MANIFEST_DIR")).unwrap_or(&path).display().to_string();
    let (head, base) = read_results_tsv(&std::fs::read_to_string(&path).ok()?);
    let run = read_run_info(cfg);
    let mut rows = Vec::new();
    let (mut newly, mut fixed) = (0, 0);
    for r in results {
        let Some((units, before)) = base.get(&r.name) else { continue };
        let now: BTreeSet<usize> = r.gaps.iter().map(|g| g.unit).collect();
        let list = |s: Vec<&usize>| s.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(", ");
        let worse: Vec<&usize> = now.difference(before).collect();
        let better: Vec<&usize> = before.difference(&now).collect();
        newly += worse.len();
        fixed += better.len();
        if !worse.is_empty() || !better.is_empty() || *units != r.units {
            let count = if *units != r.units { format!("{units} → {}", r.units) } else { r.units.to_string() };
            rows.push(format!("| {} | {count} | {} | {} |", r.name, list(worse), list(better)));
        }
    }
    let field = |m: &BTreeMap<String, String>, k: &str| m.get(k).filter(|v| !v.is_empty()).cloned().unwrap_or("?".into());
    let mut s = format!(
        "## Changes against the baseline run\n\nBaseline: `{}` ({} run of commit `{}`, {} build, {}, reference `{}`). This run: commit `{}`, {} build. Newly differing units: {newly}; newly matching: {fixed}.\n",
        shown,
        field(&head, "schedule"),
        field(&head, "commit"),
        field(&head, "engine_build"),
        field(&head, "date"),
        field(&head, "reference"),
        field(&run, "commit"),
        field(&run, "engine_build"),
    );
    if field(&head, "engine_build") != field(&run, "engine_build") {
        s.push_str("\nThe two runs' engines are builds of different (or unknown) profiles.\n");
    }
    if !rows.is_empty() {
        s.push_str("\n| file | units | newly differing | newly matching |\n|---|---:|---|---|\n");
        for r in rows {
            s.push_str(&r);
            s.push('\n');
        }
    }
    s.push('\n');
    Some(s)
}

fn read_run_info(cfg: &Config) -> BTreeMap<String, String> {
    let mut run: BTreeMap<String, String> = std::fs::read_to_string(cfg.out.join("run.txt"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect();
    // Outputs of a harness older than `engine_build`: ask the binary they name, if it's there.
    if !run.contains_key("engine_build")
        && let Some(bin) = run.get("engine_bin").filter(|b| Path::new(b).is_file())
    {
        let b = engine_build(bin).to_string();
        run.insert("engine_build".into(), b);
    }
    run
}

/// Runs one file of the suite on both sides and saves the outputs under `cfg.out`.
fn run_test(cfg: &Config, engine: &mut EngineProc, log: &Path, test: &str, before: &BTreeSet<String>) {
    {
        let path = cfg.src.join("sql").join(format!("{test}.sql"));
        let Ok(src) = std::fs::read_to_string(&path) else {
            eprintln!("{test}: no sql file");
            return;
        };
        let p = prepare(&src, &regress_env(cfg));
        created_databases(&p.text, &mut CREATED_DBS.lock().unwrap());
        std::fs::write(cfg.out.join("prepared").join(format!("{test}.sql")), &p.text).unwrap();
        let rw: String = p
            .rewrites
            .iter()
            .map(|(a, b)| format!("{a}\u{1f}{b}\n"))
            .collect();
        std::fs::write(cfg.out.join("prepared").join(format!("{test}.rewrites")), rw).unwrap();
        let app = format!("pg_regress/{test}");

        let t0 = Instant::now();
        let (ref_out, ref_st) = psql(cfg, &Target::Reference, db_name(), &app, p.text.as_bytes(), cfg.timeout);
        let ref_secs = t0.elapsed().as_secs_f64();
        let raw_len = ref_out.len();
        let ref_out = shared_tablespace(test, before, ref_out);
        let ref_edited = ref_out.len() != raw_len;
        std::fs::write(cfg.out.join("ref").join(format!("{test}.out")), &ref_out).unwrap();

        let log_start = std::fs::metadata(log).map(|m| m.len()).unwrap_or(0);
        let t0 = Instant::now();
        let (eng_out, eng_st, resumes, restarted) =
            run_engine_file(cfg, engine, &app, &p.text, &ref_out);
        let eng_secs = t0.elapsed().as_secs_f64();
        std::fs::write(cfg.out.join("engine").join(format!("{test}.out")), &eng_out).unwrap();
        let log_text = std::fs::read(log).unwrap_or_default();
        let new_log = String::from_utf8_lossy(&log_text[(log_start as usize).min(log_text.len())..]);
        let panics: Vec<&str> = new_log
            .lines()
            .filter(|l| l.contains("panicked at") || l.contains("stack overflow"))
            .collect();
        let mut meta = format!(
            "ref_status={ref_st}\nengine_status={eng_st}\nref_secs={ref_secs:.1}\nengine_secs={eng_secs:.1}\nrestarted={restarted}\nresumes={resumes}\n"
        );
        if ref_edited {
            meta.push_str("ref_edited=dropped the reference's \"tablespace regress_tblspace already exists\" error\n");
        }
        // The panic message is on the line after "panicked at".
        let lines: Vec<&str> = new_log.lines().collect();
        for (i, l) in lines.iter().enumerate() {
            if l.contains("panicked at") || l.contains("stack overflow") {
                let msg = lines.get(i + 1).copied().unwrap_or("");
                let _ = writeln!(meta, "panic={} :: {}", l.trim(), msg.trim());
            }
        }
        std::fs::write(cfg.out.join("meta").join(format!("{test}.txt")), meta).unwrap();
        eprintln!(
            "{test:<28} ref {ref_st} {ref_secs:>6.1}s  engine {eng_st} {eng_secs:>6.1}s{}",
            if panics.is_empty() {
                String::new()
            } else {
                format!("  ({} panics)", panics.len())
            }
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Comparing

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Class {
    FeatureMissing,
    MissingError,
    WrongResult,
    WrongErrorMessage,
    ErrorDetail,
    WrongExplain,
    Describe,
    Notice,
    Formatting,
    Aborted,
    KnockOn,
    ByDesign,
}

impl Class {
    fn title(self) -> &'static str {
        match self {
            Class::FeatureMissing => "feature missing (engine errors, reference succeeds)",
            Class::MissingError => "missing error (reference errors, engine succeeds)",
            Class::WrongResult => "wrong result (rows / command output differ)",
            Class::WrongErrorMessage => "wrong error message (both error, message differs)",
            Class::ErrorDetail => "error DETAIL/HINT/CONTEXT/position differs",
            Class::WrongExplain => "wrong EXPLAIN",
            Class::Describe => "psql \\d / \\s describe output differs",
            Class::Notice => "NOTICE/WARNING differs",
            Class::Formatting => "output formatting only (whitespace/alignment)",
            Class::Aborted => "not reached (engine crashed, timed out or psql gave up)",
            Class::KnockOn => {
                "knock-on (object whose CREATE failed on the engine earlier; aborted transaction)"
            }
            Class::ByDesign => "unsupported by design (regress.so C functions, tablespace dirs)",
        }
    }
}

struct Gap {
    class: Class,
    key: String,
    unit: usize,
    line: usize,
    stmt: String,
    hunk: String,
}

struct FileResult {
    name: String,
    units: usize,
    matched: usize,
    gaps: Vec<Gap>,
    meta: HashMap<String, String>,
    panics: Vec<String>,
    ref_expected: Option<bool>,
    engine_expected: Option<bool>,
    skipped: usize,
    rewrites: usize,
}

fn regress_c_functions(cfg: &Config) -> Vec<String> {
    let src = std::fs::read_to_string(cfg.src.join("regress.c")).unwrap_or_default();
    src.split("PG_FUNCTION_INFO_V1(")
        .skip(1)
        .filter_map(|s| s.split(')').next().map(str::to_string))
        .collect()
}

/// `failed` collects names of objects whose CREATE succeeded on the reference but failed on the
/// engine, across the run, to tell knock-on errors from new gaps.
fn analyze(cfg: &Config, test: &str, failed: &mut BTreeSet<String>) -> Option<FileResult> {
    let ref_out = std::fs::read_to_string(cfg.out.join("ref").join(format!("{test}.out"))).ok()?;
    let eng_out = std::fs::read_to_string(cfg.out.join("engine").join(format!("{test}.out")))
        .unwrap_or_default();
    let prepared =
        std::fs::read_to_string(cfg.out.join("prepared").join(format!("{test}.sql"))).ok()?;
    // Re-derive the units from the prepared file (markers are in it).
    let mut units: Vec<(usize, String)> = Vec::new();
    let mut cur = String::new();
    let mut src_line = 0;
    let mut start = 1;
    for line in prepared.lines() {
        if line.starts_with("-- @@@ ") {
            units.push((start, std::mem::take(&mut cur)));
            continue;
        }
        src_line += 1;
        if cur.trim().is_empty() {
            start = src_line;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if cur.lines().any(|l| {
        let t = l.trim();
        !t.is_empty() && !t.starts_with("--")
    }) {
        units.push((start, cur));
    }
    let n = units.len();
    let marked = prepared.lines().filter(|l| l.starts_with("-- @@@ ")).count();
    let rb = split_output(&ref_out, marked, n);
    let eb = split_output(&eng_out, marked, n);
    let meta: HashMap<String, String> = std::fs::read_to_string(
        cfg.out.join("meta").join(format!("{test}.txt")),
    )
    .unwrap_or_default()
    .lines()
    .filter(|l| !l.starts_with("panic="))
    .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
    .collect();
    let panics: Vec<String> =
        std::fs::read_to_string(cfg.out.join("meta").join(format!("{test}.txt")))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_prefix("panic="))
            .map(str::to_string)
            .collect();
    let cfuncs = regress_c_functions(cfg);
    let mut res = FileResult {
        name: test.to_string(),
        units: 0,
        matched: 0,
        gaps: Vec::new(),
        meta,
        panics,
        ref_expected: None,
        engine_expected: None,
        skipped: 0,
        rewrites: 0,
    };
    for (k, ((line, stmt), (r, e))) in units.iter().zip(rb.iter().zip(&eb)).enumerate() {
        let Some(r) = r else {
            res.skipped += 1;
            continue;
        };
        res.units += 1;
        if e.as_ref() == Some(r) {
            res.matched += 1;
            continue;
        }
        let (mut class, mut key) = classify(stmt, r, e.as_deref(), &cfuncs);
        if matches!(class, Class::FeatureMissing | Class::WrongErrorMessage) {
            let err = e.as_deref().and_then(first_error).unwrap_or("");
            if err.contains("current transaction is aborted") {
                (class, key) = (Class::KnockOn, "transaction already aborted by an earlier error".into());
            } else if mentioned(err, failed).is_some() {
                (class, key) = (Class::KnockOn, "uses an object whose CREATE failed earlier".into());
            } else if class == Class::FeatureMissing {
                failed.extend(created_name(stmt));
            }
        }
        let hunk = hunk(r, e.as_deref().unwrap_or(&[]));
        res.gaps.push(Gap {
            class,
            key,
            unit: k,
            line: *line,
            stmt: stmt.clone(),
            hunk,
        });
    }
    // Secondary: the reference (and the engine) against expected/<test>*.out.
    let rewrites: HashMap<String, String> =
        std::fs::read_to_string(cfg.out.join("prepared").join(format!("{test}.rewrites")))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('\u{1f}'))
            .map(|(a, b)| (b.to_string(), a.to_string()))
            .collect();
    res.rewrites = rewrites.len();
    let strip = |s: &str| -> String {
        s.lines()
            .filter(|l| !l.starts_with("-- @@@ "))
            .map(|l| rewrites.get(l).map(String::as_str).unwrap_or(l))
            .map(|l| if contrib_mode() { l.to_string() } else { l.replace(db_name(), "regression") })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut expected = Vec::new();
    for suffix in ["", "_1", "_2", "_3", "_4", "_5"] {
        if let Ok(s) = std::fs::read_to_string(cfg.src.join("expected").join(format!("{test}{suffix}.out"))) {
            expected.push(s.trim_end().to_string());
        }
    }
    if !expected.is_empty() {
        let r = strip(&ref_out);
        let e = strip(&eng_out);
        res.ref_expected = Some(expected.iter().any(|x| x == r.trim_end()));
        res.engine_expected = Some(expected.iter().any(|x| x == e.trim_end()));
    }
    Some(res)
}

/// The object a CREATE statement makes, lowercased, without schema.
fn created_name(stmt: &str) -> Option<String> {
    let w = words(stmt);
    if w.first().is_none_or(|w| w != "create") {
        return None;
    }
    const SKIP: &[&str] = &[
        "create", "or", "replace", "temp", "temporary", "unlogged", "materialized", "recursive",
        "unique", "global", "local", "default", "trusted", "procedural", "constraint", "if", "not",
        "exists", "concurrently",
    ];
    let mut it = w.iter().skip_while(|x| SKIP.contains(&x.as_str()));
    let kind = it.next()?;
    const KINDS: &[&str] = &[
        "table", "view", "type", "function", "procedure", "aggregate", "index", "sequence",
        "domain", "schema", "materialized", "collation", "server", "publication", "statistics",
        "role", "user", "group", "trigger", "rule", "policy", "foreign",
    ];
    if !KINDS.contains(&kind.as_str()) {
        return None;
    }
    let mut rest = it.skip_while(|x| SKIP.contains(&x.as_str()) || *x == "class" || *x == "family");
    let name = rest.next()?;
    if kind == "index" && name == "on" {
        return None;
    }
    // Schema-qualified: the part after the dot.
    if stmt.to_ascii_lowercase().contains(&format!("{name}."))
        && let Some(n) = rest.next()
    {
        return Some(n.clone());
    }
    Some(name.clone())
}

/// A failed object's name mentioned in an error, as `"name"`, `"schema.name"` or `name(`.
fn mentioned(err: &str, failed: &BTreeSet<String>) -> Option<String> {
    let l = err.to_ascii_lowercase();
    if !(l.contains("does not exist") || l.contains("not found") || l.contains("is not")) {
        return None;
    }
    failed
        .iter()
        .find(|n| {
            l.contains(&format!("\"{n}\""))
                || l.contains(&format!(".{n}\""))
                || l.contains(&format!(" {n}("))
                || l.contains(&format!(".{n}("))
        })
        .cloned()
}

fn is_error(l: &str) -> bool {
    l.starts_with("ERROR:") || l.starts_with("FATAL:") || l.starts_with("PANIC:")
}

fn first_error(lines: &[String]) -> Option<&str> {
    lines.iter().map(String::as_str).find(|l| is_error(l))
}

/// Masks quoted names and numbers so similar messages group together.
fn normalize(msg: &str) -> String {
    let mut out = String::new();
    let mut chars = msg.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                for d in chars.by_ref() {
                    if d == '"' {
                        break;
                    }
                }
                out.push_str("\"…\"");
            }
            c if c.is_ascii_digit() => {
                while chars.peek().is_some_and(|d| d.is_ascii_digit()) {
                    chars.next();
                }
                out.push('N');
            }
            _ => out.push(c),
        }
    }
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    out.chars().take(140).collect()
}

/// The statement's first keyword(s), comments dropped.
fn stmt_kind(stmt: &str) -> String {
    for l in stmt.lines() {
        let t = l.trim();
        if t.is_empty() || t.starts_with("--") {
            continue;
        }
        if let Some(cmd) = t.strip_prefix('\\') {
            return format!(
                "\\{}",
                cmd.split_whitespace().next().unwrap_or("").trim_end_matches('+')
            );
        }
        return words(t)
            .first()
            .cloned()
            .unwrap_or_default()
            .to_ascii_uppercase();
    }
    String::new()
}

fn classify(stmt: &str, r: &[String], e: Option<&[String]>, cfuncs: &[String]) -> (Class, String) {
    let Some(e) = e else {
        return (Class::Aborted, "engine output ends before this unit".into());
    };
    let rerr = first_error(r);
    let eerr = first_error(e);
    let lower = stmt.to_ascii_lowercase();
    // The reference has no regress.so either; anything touching it is out of scope.
    let mentions_c = |s: &str| {
        s.contains("regress.so")
            || s.contains("regresslib")
            || cfuncs.iter().any(|f| {
                s.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|w| w == f)
            })
    };
    if mentions_c(&lower) || rerr.is_some_and(mentions_c) || eerr.is_some_and(mentions_c) {
        return (Class::ByDesign, "regress.so C functions".into());
    }
    if lower.contains("tablespace") && lower.contains("location '/") {
        return (Class::ByDesign, "tablespace with a real directory".into());
    }
    if e.iter().any(|l| {
        l.contains("server closed the connection unexpectedly")
            || l.contains("connection to server was lost")
    }) {
        return (Class::Aborted, "engine connection lost (crash)".into());
    }
    match (rerr, eerr) {
        (None, Some(ee)) => return (Class::FeatureMissing, normalize(ee)),
        (Some(re), None) => return (Class::MissingError, normalize(re)),
        (Some(re), Some(ee)) if re != ee => {
            return (
                Class::WrongErrorMessage,
                format!("{}  ⟶  engine: {}", normalize(re), normalize(ee)),
            );
        }
        _ => {}
    }
    let kind = stmt_kind(stmt);
    let removed: Vec<&String> = r.iter().filter(|l| !e.contains(l)).collect();
    let added: Vec<&String> = e.iter().filter(|l| !r.contains(l)).collect();
    if rerr.is_some() {
        // Same first ERROR line; the rest of the report differs.
        let which = removed
            .iter()
            .chain(&added)
            .find_map(|l| {
                ["DETAIL:", "HINT:", "CONTEXT:", "LINE ", "WARNING:", "NOTICE:"]
                    .into_iter()
                    .find(|p| l.starts_with(p))
            })
            .unwrap_or("other");
        let which = which.trim_end_matches([':', ' ']);
        return (Class::ErrorDetail, format!("{which} differs"));
    }
    let is_explain = stmt
        .lines()
        .any(|l| l.trim_start().to_ascii_lowercase().starts_with("explain"));
    if is_explain {
        return (Class::WrongExplain, "EXPLAIN plan text differs".into());
    }
    if kind.starts_with('\\') && kind != "\\g" && kind != "\\gset" {
        return (Class::Describe, format!("{kind} output differs"));
    }
    let notice = |l: &str| {
        l.starts_with("NOTICE:") || l.starts_with("WARNING:") || l.starts_with("INFO:")
    };
    if removed.is_empty() && added.is_empty() {
        return (Class::WrongResult, format!("{kind}: row order differs"));
    }
    if removed.iter().chain(&added).all(|l| notice(l)) {
        let (side, l) = match removed.iter().find(|l| notice(l)) {
            Some(l) => ("missing", l.as_str()),
            None => (
                "extra",
                added.iter().find(|l| notice(l)).map(|l| l.as_str()).unwrap_or(""),
            ),
        };
        return (Class::Notice, format!("{side} {}", normalize(l)));
    }
    let squash = |v: &[String]| -> Vec<String> {
        v.iter()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|l| !l.chars().all(|c| c == '-' || c == '+' || c == ' '))
            .collect()
    };
    if squash(r) == squash(e) {
        return (Class::Formatting, format!("{kind}: alignment/width"));
    }
    let rows = |v: &[String]| v.iter().rev().find(|l| l.ends_with(" rows)") || l.ends_with(" row)")).cloned();
    let detail = if rows(r) != rows(e) {
        "row count differs"
    } else if removed.iter().any(|l| l.contains(" | ") || l.starts_with(' '))
        || added.iter().any(|l| l.contains(" | ") || l.starts_with(' '))
    {
        "values differ"
    } else {
        "command output differs"
    };
    (Class::WrongResult, format!("{kind}: {detail}"))
}

/// Line diff of one unit (LCS), `-` reference, `+` engine.
fn hunk(r: &[String], e: &[String]) -> String {
    const CAP: usize = 400;
    let r = &r[..r.len().min(CAP)];
    let e = &e[..e.len().min(CAP)];
    let (n, m) = (r.len(), e.len());
    let mut dp = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if r[i] == e[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut out = String::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && r[i] == e[j] {
            let _ = writeln!(out, "  {}", r[i]);
            i += 1;
            j += 1;
        } else if j < m && (i == n || dp[i][j + 1] >= dp[i + 1][j]) {
            let _ = writeln!(out, "+ {}", e[j]);
            j += 1;
        } else {
            let _ = writeln!(out, "- {}", r[i]);
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Report

fn pct(a: usize, b: usize) -> f64 {
    if b == 0 {
        100.0
    } else {
        100.0 * a as f64 / b as f64
    }
}

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('`', "'")
}

fn first_line(stmt: &str) -> String {
    let l = stmt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or("");
    let l: String = l.chars().take(90).collect();
    md_escape(&l)
}

fn write_report(cfg: &Config, results: &[FileResult]) {
    let hunks_dir = cfg.report_dir.join("hunks");
    let _ = std::fs::remove_dir_all(&hunks_dir);
    std::fs::create_dir_all(&hunks_dir).unwrap();

    let units: usize = results.iter().map(|r| r.units).sum();
    let matched: usize = results.iter().map(|r| r.matched).sum();
    // Read before this report replaces it (the committed `results.tsv` is the default).
    let changes = baseline_changes(cfg, results);
    let all_gaps: Vec<(&FileResult, &Gap)> = results
        .iter()
        .flat_map(|r| r.gaps.iter().map(move |g| (r, g)))
        .collect();
    let by_design = all_gaps
        .iter()
        .filter(|(_, g)| g.class == Class::ByDesign)
        .count();
    let full_files = results.iter().filter(|r| r.units == r.matched).count();

    let mut s = String::new();
    let _ = writeln!(s, "# PostgreSQL regression suite: engine parity report\n");
    let _ = writeln!(
        s,
        "Re-run (about an hour; needs the `roll-engine-ref` container, or a private reference: HANDOFF.md):\n\n```sh\nCARGO_BUILD_JOBS=2 cargo test -p pg-engine --test pg_regress -- --ignored --nocapture\n```\n\nRebuild only this report from the saved outputs in `$PGREGRESS_DIR/out`: add `PGREGRESS_REPORT_ONLY=1`. Run a subset (after `test_setup`): `PGREGRESS_ONLY=int4,join` (its report goes to `$PGREGRESS_DIR/report`). A full run writes this report, `results.tsv` and STATUS.md's generated blocks (`PGREGRESS_UPDATE_STATUS=1` when its report went elsewhere). The harness is `crates/pg-engine/tests/harness/regress.rs`; its header documents the method.\n"
    );
    let run = read_run_info(cfg);
    let info = |k: &str| run.get(k).map(String::as_str).filter(|v| !v.is_empty()).unwrap_or("?");
    if !run.is_empty() {
        let _ = writeln!(
            s,
            "Run: {} schedule, commit `{}` ({} build of the engine), {}, reference container `{}`, `PGREGRESS_TIMEOUT={}`; cluster-global objects the reference had before the run: {}.\n",
            info("schedule"),
            info("commit"),
            info("engine_build"),
            info("date"),
            info("reference"),
            info("timeout"),
            run.get("reference_preexisting").filter(|v| !v.is_empty()).map_or("none".into(), |v| format!("`{v}`")),
        );
    }
    for r in results {
        if let Some(e) = r.meta.get("ref_edited") {
            let _ = writeln!(s, "Reference output edited: {}: {e} (another run's tablespace).\n", r.name);
        }
    }
    let _ = writeln!(
        s,
        "Suite: `src/test/regress` at `{GIT_TAG}`, `parallel_schedule` run serially in one database. Reference: PostgreSQL 18.6 (`{}`). Both outputs come from the same `psql -X -a -q` with pg_regress's environment. A *unit* is one psql execution unit: a statement (or several on one line), a meta-command, or a COPY with its data. A unit matches when its whole psql output (rows, tags, notices, errors) is byte-identical to the reference's.\n",
        run.get("reference").map_or(cfg.container.as_str(), String::as_str)
    );
    let _ = writeln!(s, "## Summary\n");
    let _ = writeln!(s, "| | |\n|---|---|");
    let _ = writeln!(s, "| files | {} |", results.len());
    let _ = writeln!(
        s,
        "| units compared | {units} |\n| units matching | {matched} (**{:.1}%**) |",
        pct(matched, units)
    );
    let _ = writeln!(
        s,
        "| units matching, excluding by-design | {:.1}% ({matched}/{}) |",
        pct(matched, units - by_design),
        units - by_design
    );
    let _ = writeln!(s, "| files fully matching | {full_files} |");
    let _ = writeln!(
        s,
        "| one-line server-side `COPY ... :'file'` of a file psql wrote, run as `\\copy` on both sides | {} |",
        results.iter().map(|r| r.rewrites).sum::<usize>()
    );
    let _ = writeln!(
        s,
        "| reference output == expected/*.out | {} of {} files |",
        results.iter().filter(|r| r.ref_expected == Some(true)).count(),
        results.iter().filter(|r| r.ref_expected.is_some()).count()
    );
    let _ = writeln!(
        s,
        "| engine output == expected/*.out (pg_regress \"ok\") | {} files |",
        results.iter().filter(|r| r.engine_expected == Some(true)).count()
    );
    let crashed: Vec<&str> = results
        .iter()
        .filter(|r| r.meta.get("restarted").is_some_and(|v| v == "true"))
        .map(|r| r.name.as_str())
        .collect();
    let _ = writeln!(
        s,
        "| engine restarted after (crash/timeout) | {} |",
        if crashed.is_empty() {
            "none".to_string()
        } else {
            crashed.join(", ")
        }
    );
    let _ = writeln!(
        s,
        "| engine panics logged | {} |\n",
        results.iter().map(|r| r.panics.len()).sum::<usize>()
    );

    // Classes.
    let mut classes: BTreeMap<Class, usize> = BTreeMap::new();
    for (_, g) in &all_gaps {
        *classes.entry(g.class).or_default() += 1;
    }
    let mut classes: Vec<(Class, usize)> = classes.into_iter().collect();
    classes.sort_by_key(|c| std::cmp::Reverse(c.1));
    let _ = writeln!(s, "## Differences by class\n");
    let _ = writeln!(s, "| class | units | % of differing |\n|---|---:|---:|");
    for (c, n) in &classes {
        let _ = writeln!(s, "| {} | {n} | {:.1}% |", c.title(), pct(*n, all_gaps.len()));
    }
    s.push('\n');

    // Categories.
    let mut cats: HashMap<(Class, String), Vec<(&FileResult, &Gap)>> = HashMap::new();
    for (r, g) in &all_gaps {
        cats.entry((g.class, g.key.clone())).or_default().push((r, g));
    }
    let mut cats: Vec<_> = cats.into_iter().collect();
    cats.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    let _ = writeln!(
        s,
        "## Gap categories, ranked by units affected\n\nQuoted names and numbers are masked (`\"…\"`, `N`) so similar messages group together. *Feature missing* entries are the engine's error. Many `relation \"…\" does not exist` units are knock-on effects of an earlier failed CREATE.\n"
    );
    let _ = writeln!(
        s,
        "| # | units | files | class | category | example files | example statement |\n|---:|---:|---:|---|---|---|---|"
    );
    for (i, ((class, key), v)) in cats.iter().enumerate().take(150) {
        let mut files: Vec<(&str, usize)> = Vec::new();
        for (r, _) in v {
            match files.iter_mut().find(|(f, _)| *f == r.name) {
                Some(f) => f.1 += 1,
                None => files.push((&r.name, 1)),
            }
        }
        let nfiles = files.len();
        files.sort_by_key(|f| std::cmp::Reverse(f.1));
        let ex: Vec<String> = files
            .iter()
            .take(3)
            .map(|(f, n)| format!("{f} ({n})"))
            .collect();
        let (er, eg) = v[0];
        let _ = writeln!(
            s,
            "| {} | {} | {nfiles} | {} | `{}` | {} | {}:{} `{}` |",
            i + 1,
            v.len(),
            short_class(*class),
            md_escape(key),
            ex.join(", "),
            er.name,
            eg.line,
            first_line(&eg.stmt)
        );
    }
    s.push('\n');

    // Specific missing things (unmasked).
    let mut missing: HashMap<String, (usize, BTreeSet<String>)> = HashMap::new();
    for (r, g) in &all_gaps {
        if g.class != Class::FeatureMissing {
            continue;
        }
        let Some(err) = g
            .hunk
            .lines()
            .filter_map(|l| l.strip_prefix("+ "))
            .find(|l| is_error(l))
        else {
            continue;
        };
        let err = err.trim_start_matches("ERROR:").trim();
        if err.contains("does not exist")
            || err.contains("not supported")
            || err.contains("not implemented")
            || err.contains("syntax error")
            || err.contains("unrecognized")
            || err.contains("unsupported")
        {
            let k: String = err.chars().take(120).collect();
            let ent = missing.entry(k).or_default();
            ent.0 += 1;
            ent.1.insert(r.name.clone());
        }
    }
    let mut missing: Vec<_> = missing.into_iter().collect();
    missing.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(&b.0)));
    let _ = writeln!(
        s,
        "## Most frequent engine errors, unmasked (does not exist / not supported / syntax)\n"
    );
    let _ = writeln!(s, "| units | engine error | files |\n|---:|---|---|");
    for (k, (n, files)) in missing.iter().take(80) {
        let f: Vec<&str> = files.iter().map(String::as_str).take(4).collect();
        let _ = writeln!(s, "| {n} | `{}` | {} |", md_escape(k), f.join(", "));
    }
    s.push('\n');

    // Per file.
    let _ = writeln!(s, "## Per file\n");
    let _ = writeln!(
        s,
        "`ref=exp` / `eng=exp`: output identical to `expected/<name>[_N].out`. Hunks: `tests/regress/hunks/<name>.diff` (`-` reference, `+` engine).\n"
    );
    let _ = writeln!(
        s,
        "| file | units | match | rate | engine run | secs (ref/engine) | ref=exp | eng=exp | top gap |\n|---|---:|---:|---:|---|---|---|---|---|"
    );
    for r in results {
        let mut status = r
            .meta
            .get("engine_status")
            .cloned()
            .unwrap_or_else(|| "?".into());
        if !r.panics.is_empty() {
            let _ = write!(status, ", {} panic(s)", r.panics.len());
        }
        let _ = writeln!(
            s,
            "| {} | {} | {} | {:.0}% | {} | {}/{} | {} | {} | {} |",
            r.name,
            r.units,
            r.matched,
            pct(r.matched, r.units),
            status,
            r.meta.get("ref_secs").map(String::as_str).unwrap_or("?"),
            r.meta.get("engine_secs").map(String::as_str).unwrap_or("?"),
            yes_no(r.ref_expected),
            yes_no(r.engine_expected),
            top_gap(r)
        );
    }
    s.push('\n');

    // Every differing unit (the per-file unit counter of `hunks/`, STATUS.md's unit numbers).
    let _ = writeln!(s, "## Differing units\n");
    s.push_str(&differing_units_table(results));
    s.push('\n');

    if let Some(c) = &changes {
        s.push_str(c);
        eprint!("pg_regress: {c}");
    }

    // Panics.
    let panics: Vec<(&str, &String)> = results
        .iter()
        .flat_map(|r| r.panics.iter().map(move |p| (r.name.as_str(), p)))
        .collect();
    if !panics.is_empty() {
        let _ = writeln!(s, "## Engine panics\n");
        for (f, p) in panics.iter().take(60) {
            let _ = writeln!(s, "- {f}: `{}`", md_escape(p));
        }
        s.push('\n');
    }

    let _ = writeln!(
        s,
        "## Limits of the measurement\n\n\
- Files run serially; pg_regress runs most groups in parallel, so tests that look at concurrent sessions behave as in a serial run on both sides.\n\
- The reference container has no `regress.so`, so `CREATE FUNCTION ... AS :'regresslib'` fails on both sides and later statements using those functions (`widget_in`, `interpt_pp`, `binary_coercible`, `test_canonicalize_path`, ...) error on the reference too. Those units are classed *by design*; where both sides fail identically they count as matching.\n\
- Server-side files (`COPY ... TO :'filename'`, `pg_read_file`, `lo_export`) are on the host for the engine (in `PGREGRESS_DIR`, allowed with `--allow-server-files`) and in the container for the reference; psql-side files (`\\copy`, `\\o`, `\\lo_import`) are always in the container, so a `\\lo_import` of a file the server wrote reads the reference's copy (and one-line server-side COPYs of a file psql wrote run as `\\copy`).\n\
- The database is `gxg_regression` (not `regression`); `GRANT ... ON DATABASE regression` and psql's `\\d host.regression.x` patterns are rewritten. When comparing with `expected/*.out` the name is mapped back.\n\
- A unit's output includes everything psql printed until the next unit, so one wrong statement in a multi-statement line marks the whole line. Pipeline tests (`\\startpipeline`) print results at `\\endpipeline`, so differences land on that unit.\n\
- Units after a `\\quit` that the reference reached but the engine did not count as *not reached*; units neither side reached are not counted.\n"
    );
    std::fs::write(cfg.report_dir.join("REPORT.md"), s).unwrap();
    std::fs::write(cfg.report_dir.join("results.tsv"), results_tsv(cfg, results)).unwrap();

    // Hunks (PGREGRESS_HUNKS: how many differing units per file, default 12).
    let hunks = std::env::var("PGREGRESS_HUNKS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(HUNKS_PER_FILE);
    for r in results {
        if r.gaps.is_empty() {
            continue;
        }
        let mut h = format!(
            "# {}: {}/{} units match; first {} differing units (- reference, + engine)\n\n",
            r.name,
            r.matched,
            r.units,
            hunks.min(r.gaps.len())
        );
        for g in r.gaps.iter().take(hunks) {
            let _ = writeln!(
                h,
                "@@ unit {} (sql line {}) [{}] {}",
                g.unit,
                g.line,
                short_class(g.class),
                g.key
            );
            let lines: Vec<&str> = g.hunk.lines().collect();
            for l in lines.iter().take(60) {
                let _ = writeln!(h, "{l}");
            }
            if lines.len() > 60 {
                let _ = writeln!(h, "  ... {} more lines", lines.len() - 60);
            }
            h.push('\n');
        }
        std::fs::write(hunks_dir.join(format!("{}.diff", r.name)), h).unwrap();
    }
}

/// A file's most frequent gap category, `class `key` (n)`.
fn top_gap(r: &FileResult) -> String {
    let mut top: HashMap<(Class, &str), usize> = HashMap::new();
    for g in &r.gaps {
        *top.entry((g.class, &g.key)).or_default() += 1;
    }
    top.into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|((c, k), n)| {
            let k: String = k.chars().take(70).collect();
            format!("{} `{}` ({n})", short_class(c), md_escape(&k))
        })
        .unwrap_or_default()
}

fn yes_no(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "yes",
        Some(false) => "no",
        None => "-",
    }
}

/// Every differing unit by file, grouped by the harness's class.
fn differing_units_table(results: &[FileResult]) -> String {
    let mut s = String::from("| file | differing | units (harness class) |\n|---|---:|---|\n");
    let mut any = false;
    for r in results.iter().filter(|r| !r.gaps.is_empty()) {
        any = true;
        let mut by: BTreeMap<Class, Vec<usize>> = BTreeMap::new();
        for g in &r.gaps {
            by.entry(g.class).or_default().push(g.unit);
        }
        let parts: Vec<String> = by
            .iter()
            .map(|(c, u)| {
                let u: Vec<String> = u.iter().map(usize::to_string).collect();
                format!("{} ({})", u.join(", "), short_class(*c))
            })
            .collect();
        let _ = writeln!(s, "| {} | {} | {} |", r.name, r.gaps.len(), parts.join("; "));
    }
    if !any {
        s.push_str("| (none) | 0 | |\n");
    }
    s
}

/// `results.tsv`: the per-file numbers of the run, for scripts (STATUS.md's blocks are made from
/// the same numbers).
fn results_tsv(cfg: &Config, results: &[FileResult]) -> String {
    let run = read_run_info(cfg);
    let units: usize = results.iter().map(|r| r.units).sum();
    let matched: usize = results.iter().map(|r| r.matched).sum();
    let mut s = String::new();
    for k in ["schedule", "commit", "date", "reference", "timeout", "reference_preexisting", "engine_build"] {
        let _ = writeln!(s, "# {k}={}", run.get(k).map(String::as_str).unwrap_or(""));
    }
    let _ = writeln!(
        s,
        "# files={} units={units} matched={matched} differing={} files_matching={} ref_expected={} engine_expected={}",
        results.len(),
        units - matched,
        results.iter().filter(|r| r.units == r.matched).count(),
        results.iter().filter(|r| r.ref_expected == Some(true)).count(),
        results.iter().filter(|r| r.engine_expected == Some(true)).count(),
    );
    s.push_str("file\tunits\tmatch\tref_exp\teng_exp\tengine_status\tpanics\tdiffering\n");
    for r in results {
        let diff: Vec<String> = r.gaps.iter().map(|g| format!("{}:{}", g.unit, short_class(g.class))).collect();
        let _ = writeln!(
            s,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            r.name,
            r.units,
            r.matched,
            yes_no(r.ref_expected),
            yes_no(r.engine_expected),
            r.meta.get("engine_status").map(String::as_str).unwrap_or("?"),
            r.panics.len(),
            diff.join(",")
        );
    }
    s
}

/// STATUS.md's generated blocks, from a full run: the summary table, the differing units by
/// file, and the per-file table. Each sits between `<!-- generated: <name> -->` and
/// `<!-- end generated: <name> -->`; everything else in the file is prose, edited by hand.
fn status_blocks(cfg: &Config, results: &[FileResult]) -> Vec<(&'static str, String)> {
    let run = read_run_info(cfg);
    let info = |k: &str| run.get(k).map(String::as_str).filter(|v| !v.is_empty()).unwrap_or("?");
    let units: usize = results.iter().map(|r| r.units).sum();
    let matched: usize = results.iter().map(|r| r.matched).sum();
    let with_units = results.iter().filter(|r| r.units > 0).count();
    let empty: Vec<&str> = results.iter().filter(|r| r.units == 0).map(|r| r.name.as_str()).collect();
    let restarted: Vec<&str> = results
        .iter()
        .filter(|r| r.meta.get("restarted").is_some_and(|v| v == "true"))
        .map(|r| r.name.as_str())
        .collect();
    let panics: usize = results.iter().map(|r| r.panics.len()).sum();

    let mut sum = String::new();
    let _ = writeln!(
        sum,
        "From the full run of commit `{}` ({} build of the engine, {}, reference `{}`; cluster-global objects it had before: {}), written by the harness (HANDOFF.md, \"Regression suite\"); don't edit these numbers by hand.\n",
        info("commit"),
        info("engine_build"),
        info("date"),
        info("reference"),
        run.get("reference_preexisting").filter(|v| !v.is_empty()).map_or("none".into(), |v| format!("`{v}`")),
    );
    let _ = writeln!(sum, "| | |\n|---|---|");
    let _ = writeln!(
        sum,
        "| files | {with_units}{} |",
        if empty.is_empty() {
            String::new()
        } else {
            format!(" ({} in the schedule; no units: {})", results.len(), empty.join(", "))
        }
    );
    let _ = writeln!(sum, "| units compared | {units} |");
    let _ = writeln!(sum, "| units matching | {matched} (**{:.2}%**) |", pct(matched, units));
    let _ = writeln!(sum, "| units differing | {} |", units - matched);
    let _ = writeln!(
        sum,
        "| files fully matching | {} of {with_units} |",
        results.iter().filter(|r| r.units > 0 && r.units == r.matched).count()
    );
    let _ = writeln!(
        sum,
        "| reference output == expected/*.out | {} of {} files |",
        results.iter().filter(|r| r.ref_expected == Some(true)).count(),
        results.iter().filter(|r| r.ref_expected.is_some()).count()
    );
    let _ = writeln!(
        sum,
        "| engine output == expected/*.out (pg_regress \"ok\") | {} files |",
        results.iter().filter(|r| r.engine_expected == Some(true)).count()
    );
    let _ = writeln!(
        sum,
        "| engine restarted (crash / timeout) | {} |",
        if restarted.is_empty() { "none".to_string() } else { restarted.join(", ") }
    );
    let _ = writeln!(sum, "| engine panics logged | {} |", if panics == 0 { "none".to_string() } else { panics.to_string() });

    let mut diff = differing_units_table(results);
    let mut classes: BTreeMap<Class, usize> = BTreeMap::new();
    for g in results.iter().flat_map(|r| &r.gaps) {
        *classes.entry(g.class).or_default() += 1;
    }
    if !classes.is_empty() {
        diff.push_str("\n| harness class | units |\n|---|---:|\n");
        for (c, n) in &classes {
            let _ = writeln!(diff, "| {} | {n} |", short_class(*c));
        }
    }

    let mut per = String::from(
        "| file | units | match | rate | ref=exp | eng=exp | top gap (harness class) |\n|---|---:|---:|---:|---|---|---|\n",
    );
    for r in results {
        let rate = if r.units == 0 { "-".to_string() } else { format!("{:.0}%", pct(r.matched, r.units).floor()) };
        let _ = writeln!(
            per,
            "| {} | {} | {} | {rate} | {} | {} | {} |",
            r.name,
            r.units,
            r.matched,
            yes_no(r.ref_expected),
            yes_no(r.engine_expected),
            top_gap(r)
        );
    }
    vec![("summary", sum), ("differing units", diff), ("per file", per)]
}

/// Replaces STATUS.md's generated blocks with a full run's numbers.
fn update_status(cfg: &Config, path: &Path, results: &[FileResult]) {
    let mut text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    for (name, body) in status_blocks(cfg, results) {
        let begin = format!("<!-- generated: {name} -->\n");
        let end = format!("<!-- end generated: {name} -->");
        let (Some(b), Some(e)) = (text.find(&begin), text.find(&end)) else {
            panic!("{}: no `{begin}` ... `{end}` block to regenerate", path.display());
        };
        assert!(b < e, "{}: block `{name}` ends before it starts", path.display());
        text.replace_range(b + begin.len()..e, &body);
    }
    std::fs::write(path, text).unwrap();
}

fn short_class(c: Class) -> &'static str {
    match c {
        Class::FeatureMissing => "feature missing",
        Class::MissingError => "missing error",
        Class::WrongResult => "wrong result",
        Class::WrongErrorMessage => "wrong error message",
        Class::ErrorDetail => "error detail",
        Class::WrongExplain => "wrong EXPLAIN",
        Class::Describe => "psql describe",
        Class::Notice => "notice/warning",
        Class::Formatting => "formatting",
        Class::Aborted => "not reached",
        Class::KnockOn => "knock-on",
        Class::ByDesign => "by design",
    }
}

#[test]
fn lexer_units() {
    let src = "SELECT 1;\n-- c\nSELECT 'a;b',\n  $$x;y$$;\nCOPY t FROM stdin;\n1\n\\.\n\\d t\nCREATE FUNCTION f() RETURNS int BEGIN ATOMIC SELECT 1; END;\nselect 1 \\gset\n/* x */\nselect 2; select 3;\nselect (1;\n2);\n";
    let p = prepare(src, &HashMap::new());
    let firsts: Vec<String> = p.units.iter().map(|(_, u)| first_line(u)).collect();
    assert_eq!(
        firsts,
        [
            "SELECT 1;",
            "SELECT 'a;b',",
            "COPY t FROM stdin;",
            "\\d t",
            "CREATE FUNCTION f() RETURNS int BEGIN ATOMIC SELECT 1; END;",
            "select 1 \\gset",
            "/* x */",
            "select (1;"
        ]
    );
}
