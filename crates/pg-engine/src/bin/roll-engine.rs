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
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
    /// Block cache shared by all databases, in MiB.
    #[arg(long, default_value_t = 64)]
    cache_mb: usize,
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
    /// Libraries to preload (comma-separated, as `shared_preload_libraries`):
    /// `pg_stat_statements` collects statement statistics from server start.
    #[arg(long, value_name = "LIBS", value_delimiter = ',')]
    shared_preload_libraries: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    idle::setup();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let a = Args::parse();
    let auth = a.auth.unwrap_or(if a.password.is_some() {
        AuthMethod::ScramSha256
    } else {
        AuthMethod::Trust
    });
    let engine = Engine::open(EngineOptions {
        data_dir: a.data,
        cache_bytes: a.cache_mb << 20,
        sync: !a.no_sync,
        idle_close: Duration::from_secs(a.idle_close_secs),
        superuser: a.user,
        password: a.password,
        server_files: a.allow_server_files,
        shared_preload_libraries: a.shared_preload_libraries,
        ..EngineOptions::default()
    })?;
    // SIGTERM / SIGINT: a clean shutdown (pg_ctl stop's smart / fast modes end sessions the
    // same way here), so sequences keep their positions.
    {
        let engine = engine.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build();
            let Ok(rt) = rt else { return };
            rt.block_on(async {
                use tokio::signal::unix::{SignalKind, signal};
                let (Ok(mut term), Ok(mut int)) =
                    (signal(SignalKind::terminate()), signal(SignalKind::interrupt()))
                else {
                    return;
                };
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
            });
            tracing::info!("shutting down");
            engine.shutdown();
            std::process::exit(0);
        });
    }
    let listener = std::net::TcpListener::bind(&a.listen)?;
    tracing::info!(addr = %a.listen, ?auth, "listening");
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
