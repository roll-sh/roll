//! Run the engine as a standalone Postgres-compatible server:
//!
//!     roll-engine --data .roll/engine --listen 127.0.0.1:5433 --password secret
//!     psql "postgres://postgres:secret@127.0.0.1:5433/postgres"

use clap::Parser;
use pg_engine::engine::{Engine, EngineOptions};
use pg_engine::server::{AuthMethod, ServerOptions};
use std::path::PathBuf;
use std::time::Duration;

// The engine allocates per row and per value; mimalloc does that markedly faster than the
// system allocator, from many sessions' threads at once (BENCH.md). Embedders choose their
// own global allocator.
#[cfg(not(feature = "heapprof"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// A sampling heap profile of where memory goes (`ROLL_HEAPPROF`, see the module).
#[cfg(feature = "heapprof")]
#[path = "roll-engine/heapprof.rs"]
mod heapprof;
#[cfg(feature = "heapprof")]
#[global_allocator]
static GLOBAL: heapprof::Prof = heapprof::Prof;

/// `--version`: the build profile too, which the regress harness records with a run (a few
/// limits depend on frame sizes, so on the profile).
const VERSION: &str = if cfg!(debug_assertions) {
    concat!(env!("CARGO_PKG_VERSION"), " (debug build)")
} else {
    concat!(env!("CARGO_PKG_VERSION"), " (release build)")
};

#[derive(Parser)]
#[command(about = "roll's Postgres-compatible database server", version = VERSION)]
struct Args {
    /// Data directory (one subdirectory per database).
    #[arg(long, default_value = ".roll/engine")]
    data: PathBuf,
    #[arg(long, default_value = "127.0.0.1:5433")]
    listen: String,
    /// Superuser name.
    #[arg(long, default_value = "postgres")]
    user: String,
    /// Superuser password (SCRAM-SHA-256). Without it, connections are trusted.
    #[arg(long, env = "ROLL_ENGINE_PASSWORD")]
    password: Option<String>,
    /// How clients authenticate, as a pg_hba method: trust, password, md5 or scram-sha-256
    /// (default: scram-sha-256 with --password, else trust).
    #[arg(long, value_parser = |s: &str| AuthMethod::parse(s).ok_or("trust, password, md5 or scram-sha-256"))]
    auth: Option<AuthMethod>,
    /// Block cache shared by all databases, in MiB (default 64, or three quarters of
    /// --memory-budget-mb).
    #[arg(long)]
    cache_mb: Option<usize>,
    /// Parse trees and PL/pgSQL compiles shared by all sessions, in MiB (default 36, or a
    /// quarter of --memory-budget-mb).
    #[arg(long)]
    plan_cache_mb: Option<usize>,
    /// What the caches shared by all sessions may hold together, in MiB: the block cache and
    /// the plan caches, 3 : 1 (--cache-mb or --plan-cache-mb, when given, set that part).
    #[arg(long)]
    memory_budget_mb: Option<usize>,
    /// Close a database this many seconds after its last session ends.
    #[arg(long, default_value_t = 60)]
    idle_close_secs: u64,
    /// Don't fsync the WAL on commit (faster, loses the last commits on a crash).
    #[arg(long)]
    no_sync: bool,
    /// Let the roles PostgreSQL allows read and write server files (pg_read_file, pg_ls_dir,
    /// server-side lo_import/lo_export, COPY to or from a file), inside this directory only:
    /// relative paths are resolved against it, others must be in it. Off by default (refused
    /// for every role). For development and testing.
    #[arg(long, value_name = "DIR")]
    allow_server_files: Option<PathBuf>,
    /// Let the roles PostgreSQL allows (superusers, pg_execute_server_program) run programs
    /// on the server: COPY ... FROM / TO PROGRAM and file_fdw's `program` option, through
    /// the shell, in the --allow-server-files directory (else the data directory). Off by
    /// default (refused for every role). For development and testing.
    #[arg(long)]
    allow_server_programs: bool,
    /// Libraries to preload (comma-separated, as `shared_preload_libraries`):
    /// `pg_stat_statements` collects statement statistics from server start.
    #[arg(long, value_name = "LIBS", value_delimiter = ',')]
    shared_preload_libraries: Vec<String>,
    /// initdb's locale options, used when the data directory is new (an existing one keeps
    /// its own). The default is initdb's `--locale=C --encoding=UTF8`, the reference's.
    #[command(flatten)]
    initdb: InitdbArgs,
    /// Let dblink and postgres_fdw connect to these destinations (comma-separated): `local`
    /// (this server, reached without a host as through PostgreSQL's Unix socket),
    /// `host[:port]`, `*[:port]` (any host), `addr/bits` (a network). PostgreSQL's own rules
    /// (password_required, non-superusers authenticating with a password) still apply. Off
    /// by default: every outbound connection is refused.
    #[arg(long, value_name = "LIST", value_delimiter = ',')]
    allow_outbound: Vec<String>,
    /// max_prepared_transactions: how many transactions may be prepared at once for
    /// two-phase commit (PREPARE TRANSACTION). 0, the default as in PostgreSQL, disables it.
    #[arg(long, value_name = "N", default_value_t = 0)]
    max_prepared_transactions: usize,
    /// Store every database's catalog as builds of catalog format 1 (before round 19's
    /// comment records) read it, then exit: run it after a clean shutdown, before starting an
    /// older build on the data directory.
    #[arg(long)]
    downgrade_catalog: bool,
    /// A run-time parameter, as `postgres -c name=value` (repeatable): above
    /// postgresql.conf and ALTER SYSTEM's postgresql.auto.conf, pg_settings' "command line".
    #[arg(short = 'c', long = "set", value_name = "NAME=VALUE", value_parser = parse_setting)]
    settings: Vec<(String, String)>,
    /// Client authentication rules in pg_hba.conf's format, read again by pg_reload_conf
    /// and SIGHUP (shown by pg_hba_file_rules). Without it (or --hba), every connection
    /// authenticates with --auth.
    #[arg(long, value_name = "FILE")]
    hba_file: Option<PathBuf>,
    /// A pg_hba.conf line (repeatable; when there's no --hba-file), e.g.
    /// `--hba "host all all 127.0.0.1/32 trust" --hba "host all all all scram-sha-256"`.
    #[arg(long = "hba", value_name = "RULE")]
    hba_lines: Vec<String>,
    /// User name maps in pg_ident.conf's format (pg_ident_file_mappings).
    #[arg(long, value_name = "FILE")]
    ident_file: Option<PathBuf>,
}

fn parse_setting(s: &str) -> Result<(String, String), String> {
    let (k, v) = s.split_once('=').ok_or("NAME=VALUE")?;
    Ok((k.trim().replace('-', "_"), v.to_string()))
}

/// initdb's locale and encoding options.
#[derive(clap::Args)]
struct InitdbArgs {
    /// Default locale of new databases (each --lc-* not given, and the ICU or builtin locale).
    #[arg(long, default_value = "C")]
    locale: String,
    #[arg(long)]
    lc_collate: Option<String>,
    #[arg(long)]
    lc_ctype: Option<String>,
    #[arg(long)]
    lc_messages: Option<String>,
    #[arg(long)]
    lc_monetary: Option<String>,
    #[arg(long)]
    lc_numeric: Option<String>,
    #[arg(long)]
    lc_time: Option<String>,
    /// libc, icu or builtin.
    #[arg(long)]
    locale_provider: Option<String>,
    #[arg(long)]
    icu_locale: Option<String>,
    /// C, C.UTF-8 or PG_UNICODE_FAST.
    #[arg(long)]
    builtin_locale: Option<String>,
    #[arg(long)]
    icu_rules: Option<String>,
    /// Server encoding (UTF8 only).
    #[arg(long, short = 'E', default_value = "UTF8")]
    encoding: String,
}

impl InitdbArgs {
    fn options(self) -> pg_engine::types::collation::dblocale::InitdbLocale {
        pg_engine::types::collation::dblocale::InitdbLocale {
            locale: Some(self.locale),
            lc_collate: self.lc_collate,
            lc_ctype: self.lc_ctype,
            lc_messages: self.lc_messages,
            lc_monetary: self.lc_monetary,
            lc_numeric: self.lc_numeric,
            lc_time: self.lc_time,
            provider: self.locale_provider,
            icu_locale: self.icu_locale,
            builtin_locale: self.builtin_locale,
            icu_rules: self.icu_rules,
            encoding: Some(self.encoding),
        }
    }
}

fn main() -> anyhow::Result<()> {
    #[cfg(feature = "heapprof")]
    heapprof::start();
    idle::setup();
    tracing_subscriber::fmt()
        .with_env_filter(
            // (stderr is the server log's, PostgreSQL's lines: the engine's own tracing shows
            // warnings unless RUST_LOG asks for more.)
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let a = Args::parse();
    // --listen is `postgres -h host -p port`: listen_addresses is the host as given (the port
    // is the one bound, which sessions read from the listener).
    if a.settings.iter().any(|(k, _)| k == "port" || k == "listen_addresses") {
        anyhow::bail!("port and listen_addresses are set by --listen");
    }
    let mut settings = a.settings;
    if let Some((host, _)) = a.listen.rsplit_once(':') {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        settings.push(("listen_addresses".into(), host.to_string()));
    }
    let auth = a.auth.unwrap_or(if a.password.is_some() {
        AuthMethod::ScramSha256
    } else {
        AuthMethod::Trust
    });
    let data_dir = a.data.clone();
    let plan_mb = a.plan_cache_mb.unwrap_or(a.memory_budget_mb.map_or(pg_engine::lru::PLAN_CACHE_BYTES >> 20, |b| b / 4));
    pg_engine::lru::set_plan_cache_bytes(plan_mb << 20);
    let engine = Engine::open(EngineOptions {
        data_dir: a.data,
        cache_bytes: a.cache_mb.unwrap_or(a.memory_budget_mb.map_or(64, |b| b * 3 / 4)) << 20,
        sync: !a.no_sync,
        idle_close: Duration::from_secs(a.idle_close_secs),
        superuser: a.user,
        password: a.password,
        server_files: a.allow_server_files,
        server_programs: a.allow_server_programs,
        shared_preload_libraries: a.shared_preload_libraries,
        locale: a.initdb.options(),
        outbound: a.allow_outbound,
        max_prepared_transactions: a.max_prepared_transactions,
        settings,
        auth_files: pg_engine::sysconf::AuthFiles {
            hba_file: a.hba_file,
            hba_lines: a.hba_lines,
            ident_file: a.ident_file,
        },
        ..EngineOptions::default()
    })?;
    if a.downgrade_catalog {
        for name in engine.downgrade_catalogs()? {
            println!("{name}: catalog stored in format 1");
        }
        engine.shutdown();
        return Ok(());
    }
    // The postmaster's signals: SIGTERM a smart shutdown (sessions run to their end, new
    // connections are refused), SIGINT a fast one (sessions end now), both with a shutdown
    // checkpoint (sequences keep their positions); SIGQUIT an immediate one (no checkpoint:
    // the next start recovers, as after a crash). A later, more urgent signal takes over.
    // SIGHUP reloads the configuration files.
    {
        let engine = engine.clone();
        std::thread::spawn(move || {
            use pg_engine::log::lifecycle::{self, Shutdown};
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
            let Ok(rt) = rt else { return };
            let mode = rt.block_on(async {
                use tokio::signal::unix::{SignalKind, signal};
                let (Ok(mut term), Ok(mut int), Ok(mut quit), Ok(mut hup)) = (
                    signal(SignalKind::terminate()),
                    signal(SignalKind::interrupt()),
                    signal(SignalKind::quit()),
                    signal(SignalKind::hangup()),
                ) else {
                    return None;
                };
                let mut mode: Option<Shutdown> = None;
                let mut tick = tokio::time::interval(Duration::from_millis(20));
                loop {
                    let asked = tokio::select! {
                        _ = term.recv() => Some(Shutdown::Smart),
                        _ = int.recv() => Some(Shutdown::Fast),
                        _ = quit.recv() => Some(Shutdown::Immediate),
                        // SIGHUP: the configuration files again, as pg_reload_conf().
                        _ = hup.recv() => {
                            let conf = engine.server_conf();
                            let logs = conf.reload();
                            let files = conf.loaded();
                            for m in logs {
                                pg_engine::log::postmaster(Some(&files), pg_engine::error::Severity::Log, &m);
                            }
                            pg_engine::log::collector::reload(engine.command_line_values(), &files);
                            None
                        }
                        _ = tick.tick(), if mode.is_some() => None,
                    };
                    if let Some(m) = asked.filter(|m| mode.is_none_or(|cur| *m > cur)) {
                        mode = Some(m);
                        m.log_request(&engine);
                        engine.refuse_connections();
                        match m {
                            Shutdown::Smart => {}
                            Shutdown::Fast => engine.terminate_client_backends(),
                            Shutdown::Immediate => return Some(m),
                        }
                    }
                    if mode.is_some() && engine.client_backends() == 0 {
                        return mode;
                    }
                }
            });
            match mode {
                Some(Shutdown::Immediate) => {
                    // (Sessions end with the process, without a shutdown checkpoint.)
                    pg_engine::server::quit_clients(&engine);
                    lifecycle::shut_down_immediately(&engine);
                    pg_engine::log::flush();
                    std::process::exit(0);
                }
                Some(_) => {
                    for (cache, f) in pg_engine::lru::footprints() {
                        tracing::info!(
                            cache,
                            entries = f.entries,
                            bytes = f.bytes,
                            capacity = f.capacity,
                            hits = f.hits,
                            misses = f.misses,
                            evictions = f.evictions,
                            "shared cache"
                        );
                    }
                    tracing::info!("shutting down");
                    lifecycle::shutdown(&engine);
                    pg_engine::log::flush();
                    std::process::exit(0);
                }
                None => {}
            }
        });
    }
    let listener = std::net::TcpListener::bind(&a.listen)?;
    tracing::info!(addr = %a.listen, ?auth, "listening");
    // The server log (PostgreSQL's): stderr, or the logging collector's files.
    pg_engine::log::set_sink(Some(pg_engine::log::stderr_sink()));
    let files = engine.server_conf().loaded();
    let argv = engine.command_line_values().to_vec();
    pg_engine::log::set_command_line(&argv);
    if pg_engine::log::collector::wanted_at_start(&argv, &files) {
        let cfg = pg_engine::log::collector::Config::of_server(data_dir.clone(), &argv, &files);
        let f = pg_engine::log::Format::from_files(Some(&files));
        let hint = format!("Future log output will appear in directory \"{}\".", cfg.directory);
        pg_engine::log::write(&f, &pg_engine::log::Who::postmaster(), &pg_engine::log::Message {
            hint: Some(&hint),
            ..pg_engine::log::Message::log("redirecting log output to logging collector process")
        });
        pg_engine::log::flush();
        pg_engine::log::collector::start(cfg)?;
    } else {
        // (A previous run's collector's list of files is stale.)
        let _ = std::fs::remove_file(data_dir.join("current_logfiles"));
    }
    pg_engine::log::lifecycle::starting(&engine);
    pg_engine::log::postmaster(
        Some(&files),
        pg_engine::error::Severity::Log,
        &format!("listening on {} address \"{}\", port {}",
            if listener.local_addr()?.is_ipv4() { "IPv4" } else { "IPv6" },
            listener.local_addr()?.ip(),
            listener.local_addr()?.port()),
    );
    pg_engine::log::lifecycle::startup(&engine);
    pg_engine::log::postmaster(Some(&files), pg_engine::error::Severity::Log, "database system is ready to accept connections");
    pg_engine::server::serve(engine, listener, ServerOptions { auth })?;
    Ok(())
}

/// Idle sessions keep little memory (each connection is a thread of its own). Transparent
/// huge pages are off for the process: with the system's `always` mode every thread's first
/// heap pages took a 2 MB page, 2-3 MB per idle session (BENCH.md). And a connection idle for
/// a while (`server::set_idle_hook`) gives its thread's free heap pages and the stack its
/// statements touched but no longer use back to the system.
#[cfg(target_os = "linux")]
mod idle {
    use std::ffi::{c_int, c_ulong, c_void};

    unsafe extern "C" {
        fn prctl(option: c_int, ...) -> c_int;
        fn madvise(addr: *mut c_void, len: usize, advice: c_int) -> c_int;
        // (mimalloc's: the thread's free pages go back to the system.)
        fn mi_collect(force: bool);
    }

    const PR_SET_THP_DISABLE: c_int = 41;
    const MADV_DONTNEED: c_int = 4;

    pub fn setup() {
        // SAFETY: prctl with PR_SET_THP_DISABLE only sets a flag of this process.
        unsafe { prctl(PR_SET_THP_DISABLE, 1 as c_ulong, 0 as c_ulong, 0 as c_ulong, 0 as c_ulong) };
        pg_engine::server::set_idle_hook(trim);
        // Memory freed by busy threads, or by threads that ended, goes back to the system
        // only when some thread runs the allocator's collection: after a burst of sessions
        // nothing would, and the process would keep its peak (BENCH.md "Memory under
        // load"). A normal collection returns what was freed over a second ago.
        let _ = std::thread::Builder::new()
            .name("roll-trim".into())
            .stack_size(64 << 10)
            .spawn(|| {
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    // SAFETY: as in `trim`.
                    unsafe { mi_collect(false) };
                }
            });
    }

    fn trim() {
        // SAFETY: mi_collect only returns free memory of the global allocator (mimalloc).
        unsafe { mi_collect(true) };
        if let Some((lo, hi)) = pg_engine::stack::unused_range() {
            // SAFETY: the range is this thread's stack below every live frame (and a margin),
            // which reads as zeros when touched again.
            unsafe { madvise(lo as *mut c_void, hi - lo, MADV_DONTNEED) };
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod idle {
    pub fn setup() {}
}
