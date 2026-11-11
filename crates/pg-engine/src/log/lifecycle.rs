//! The server's start and end as PostgreSQL's postmaster, startup process and checkpointer
//! log them: StartupXLOG's lines about how the last server ended (a clean shutdown, or a
//! crash and its recovery with the end-of-recovery checkpoint), and a smart or fast
//! shutdown's checkpoint. The engine has no WAL of PostgreSQL's format: the positions are its
//! own (`pgstat::Fixed`), the recovery's CPU and time those of opening the data directory.

use super::{Format, Lines, Message, Who, postmaster, stamp, write};
use crate::engine::Engine;
use crate::error::Severity;
use std::sync::{Arc, OnceLock};

fn lsn(p: u64) -> String {
    format!("{:X}/{:X}", p >> 32, p & 0xffff_ffff)
}

/// pg_rusage_show's seconds (centiseconds).
fn secs(us: i64) -> String {
    format!("{}.{:02}", us / 1_000_000, us % 1_000_000 / 10_000)
}

/// A setting of the server's (its command line, else the configuration files').
fn server_setting(engine: &Engine, name: &str) -> Option<String> {
    engine
        .command_line_value(name)
        .map(str::to_string)
        .or_else(|| engine.server_conf().loaded().values.get(name).map(|v| v.value.clone()))
}

fn log_checkpoints(engine: &Engine) -> bool {
    server_setting(engine, "log_checkpoints")
        .and_then(|v| crate::session::settings::parse_bool(&v))
        .unwrap_or(true)
}

/// "starting PostgreSQL ...": the postmaster's first line.
pub fn starting(engine: &Engine) {
    let files = engine.server_conf().loaded();
    postmaster(Some(&files), Severity::Log, &format!("starting {}", crate::sql::func::system::VERSION));
}

/// The startup process's lines, once the server listens: how the last server ended, and
/// after a crash the recovery and its checkpoint.
pub fn startup(engine: &Engine) {
    let Some(r) = engine.startup_report() else { return };
    if !super::active() {
        return;
    }
    static LINES: OnceLock<Arc<Lines>> = OnceLock::new();
    let files = engine.server_conf().loaded();
    let f = Format::from_files(Some(&files));
    let who = Who::aux(32, "startup", &LINES);
    let log = |t: &str| write(&f, &who, &Message::log(t));
    // (str_time: log_timezone's "%Y-%m-%d %H:%M:%S %Z".)
    let time = stamp(r.last_time, &f.tz, false);
    if !r.crashed {
        if r.last_time != 0 {
            log(&format!("database system was shut down at {time}"));
        }
        return;
    }
    log(&format!("database system was interrupted; last known up at {time}"));
    log("database system was not properly shut down; automatic recovery in progress");
    log(&format!("redo starts at {}", lsn(r.redo_start)));
    log(&format!("invalid record length at {}: expected at least 24, got 0", lsn(r.resumed)));
    log(&format!(
        "redo done at {} system usage: CPU: user: {} s, system: {} s, elapsed: {} s",
        lsn(r.resumed),
        secs(r.cpu.0),
        secs(r.cpu.1),
        secs(r.elapsed.as_micros() as i64)
    ));
    let flags = "end-of-recovery immediate wait";
    let _ = crate::exec::pgstat::checkpoint(&engine.cluster_stats(), flags, log_checkpoints(engine), || Ok(()));
}

/// How a shutdown was asked for (SIGTERM, SIGINT, SIGQUIT), in increasing urgency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Shutdown {
    Smart,
    Fast,
    Immediate,
}

impl Shutdown {
    /// The postmaster's lines for the request ("received smart shutdown request", ...).
    pub fn log_request(self, engine: &Engine) {
        let files = engine.server_conf().loaded();
        let name = match self {
            Shutdown::Smart => "smart",
            Shutdown::Fast => "fast",
            Shutdown::Immediate => "immediate",
        };
        postmaster(Some(&files), Severity::Log, &format!("received {name} shutdown request"));
        if self == Shutdown::Fast {
            postmaster(Some(&files), Severity::Log, "aborting any active transactions");
        }
    }
}

/// A smart or fast shutdown's end, once no session is left: the background workers stop,
/// the checkpointer writes the shutdown checkpoint (`Engine::shutdown` makes everything
/// durable), and the postmaster exits.
pub fn shutdown(engine: &Engine) {
    let files = engine.server_conf().loaded();
    postmaster(
        Some(&files),
        Severity::Log,
        "background worker \"logical replication launcher\" (PID 35) exited with exit code 1",
    );
    if super::active() {
        crate::exec::pgstat::checkpointer_log("shutting down".into());
    }
    let flags = "shutdown immediate";
    let _ = crate::exec::pgstat::checkpoint(&engine.cluster_stats(), flags, log_checkpoints(engine), || {
        engine.shutdown();
        Ok(())
    });
    postmaster(Some(&files), Severity::Log, "database system is shut down");
}

/// An immediate shutdown's last line: no checkpoint (the next start recovers, as after a
/// crash).
pub fn shut_down_immediately(engine: &Engine) {
    let files = engine.server_conf().loaded();
    postmaster(Some(&files), Severity::Log, "database system is shut down");
}
