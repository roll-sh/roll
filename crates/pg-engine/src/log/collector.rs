//! The logging collector (syslogger.c) and the stderr writer. Sessions hand finished lines to
//! a background thread over a channel; it writes them through a buffer and flushes whenever
//! the channel is empty, so a query never waits on the disk or a terminal (only on a full
//! channel, as PostgreSQL's backends wait on a full pipe to the syslogger).
//!
//! With logging_collector on, lines go to log_directory's files named by log_filename
//! (strftime in log_timezone; ".csv" / ".json" for csvlog / jsonlog), rotated by
//! log_rotation_age / log_rotation_size and pg_rotate_logfile, with current_logfiles in the
//! data directory saying which files are being written (pg_current_logfile).

use super::{Dest, Kind};
use crate::types::datetime::{Clock, TimeZone, timestamp2tm};
use parking_lot::Mutex;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::time::Duration;

/// Lines waiting to be written before a writer blocks (a log storm's backpressure).
const QUEUE: usize = 16 * 1024;

enum Msg {
    Line(Kind, String),
    Flush(SyncSender<()>),
    Rotate,
    Config(Config),
}

/// The background stderr writer (logging_collector off).
pub struct Writer {
    tx: SyncSender<Msg>,
}

impl Writer {
    pub fn send(&self, s: &str) {
        let _ = self.tx.send(Msg::Line(Kind::Stderr, s.to_string()));
    }
}

static STDERR: Mutex<Option<SyncSender<Msg>>> = Mutex::new(None);

/// The process's one stderr writer thread.
pub fn stderr_writer() -> Writer {
    let mut g = STDERR.lock();
    let tx = g.get_or_insert_with(|| {
        let (tx, rx) = sync_channel::<Msg>(QUEUE);
        let _ = std::thread::Builder::new().name("roll-log".into()).spawn(move || {
            let mut out = std::io::BufWriter::with_capacity(64 << 10, std::io::stderr());
            while let Ok(m) = rx.recv() {
                let mut next = Some(m);
                while let Some(m) = next {
                    match m {
                        Msg::Line(_, s) => drop(out.write_all(s.as_bytes())),
                        Msg::Flush(ack) => {
                            let _ = out.flush();
                            let _ = ack.send(());
                        }
                        Msg::Rotate | Msg::Config(_) => {}
                    }
                    next = rx.try_recv().ok();
                }
                let _ = out.flush();
            }
        });
        tx
    });
    Writer { tx: tx.clone() }
}

/// What the collector writes where (the server's settings, as of its start or last reload).
#[derive(Clone, Debug)]
pub struct Config {
    /// The data directory a relative log_directory is in.
    pub data_dir: PathBuf,
    pub directory: String,
    pub filename: String,
    /// log_rotation_age in minutes, log_rotation_size in kB (0: never).
    pub rotation_age: i64,
    pub rotation_size: i64,
    pub truncate_on_rotation: bool,
    pub file_mode: u32,
    pub dest: Dest,
    pub tz: TimeZone,
    /// logging_collector (read at the start only).
    pub collector: bool,
}

impl Config {
    /// The collector's settings from the configuration files' values `get` (None: default).
    pub fn from_values(data_dir: PathBuf, get: impl Fn(&str) -> Option<String>) -> Config {
        let int = |n: &str, d: i64| get(n).and_then(|v| v.parse().ok()).unwrap_or(d);
        Config {
            data_dir,
            directory: get("log_directory").unwrap_or_else(|| "log".into()),
            filename: get("log_filename").unwrap_or_else(|| "postgresql-%Y-%m-%d_%H%M%S.log".into()),
            rotation_age: int("log_rotation_age", 1440),
            rotation_size: int("log_rotation_size", 10240),
            truncate_on_rotation: get("log_truncate_on_rotation").is_some_and(|v| v == "on"),
            file_mode: get("log_file_mode").and_then(|v| u32::from_str_radix(&v, 8).ok()).unwrap_or(0o600),
            dest: Dest::parse(&get("log_destination").unwrap_or_else(|| "stderr".into())),
            tz: get("log_timezone").and_then(|v| TimeZone::parse(&v)).unwrap_or_else(TimeZone::gmt),
            collector: get("logging_collector").is_some_and(|v| v == "on"),
        }
    }

    /// log_directory as the server sees it.
    fn dir(&self) -> PathBuf {
        self.data_dir.join(&self.directory)
    }

    /// The server's values: the command line's (`argv`), else the configuration files'
    /// (normalized as pg_settings shows them).
    pub fn of_server(data_dir: PathBuf, argv: &[(String, String)], files: &crate::sysconf::Loaded) -> Config {
        use crate::session::settings::{FileParam, file_param};
        Config::from_values(data_dir, |n| {
            let raw = argv.iter().rev().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
            match raw.map(|r| file_param(n, r)) {
                Some(FileParam::Known { value: Some(v), .. }) => Some(v),
                _ => files.values.get(n).map(|v| v.value.clone()),
            }
        })
    }
}

/// logging_collector as the server starts: the command line's, else the files'.
pub fn wanted_at_start(argv: &[(String, String)], files: &crate::sysconf::Loaded) -> bool {
    Config::of_server(PathBuf::new(), argv, files).collector
}

/// A reload: the running collector takes the new values (SIGHUP to the syslogger).
pub fn reload(argv: &[(String, String)], files: &crate::sysconf::Loaded) {
    let dir = COLLECTOR.read().as_ref().map(|h| h.data_dir.clone());
    if let Some(d) = dir {
        reconfigure(Config::of_server(d, argv, files));
    }
}

struct Handle {
    tx: SyncSender<Msg>,
    /// current_logfiles' lines: each format's file, as named relative to the data directory.
    current: std::sync::Arc<Mutex<Vec<(Kind, String)>>>,
    dir: std::sync::Arc<Mutex<PathBuf>>,
    data_dir: PathBuf,
    thread: Option<std::thread::JoinHandle<()>>,
}

static COLLECTOR: parking_lot::RwLock<Option<Handle>> = parking_lot::RwLock::new(None);
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether the logging collector runs.
pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

pub(super) fn send(kind: Kind, s: String) {
    let tx = COLLECTOR.read().as_ref().map(|h| h.tx.clone());
    if let Some(tx) = tx {
        let _ = tx.send(Msg::Line(kind, s));
    }
}

/// Start the collector (logging_collector = on at the server's start).
pub fn start(cfg: Config) -> std::io::Result<()> {
    let mut g = COLLECTOR.write();
    if g.is_some() {
        return Ok(());
    }
    let current = std::sync::Arc::new(Mutex::new(Vec::new()));
    let data_dir = cfg.data_dir.clone();
    let dir = std::sync::Arc::new(Mutex::new(cfg.dir()));
    let mut files = Files::open(cfg, current.clone(), dir.clone())?;
    let (tx, rx) = sync_channel::<Msg>(QUEUE);
    let thread = std::thread::Builder::new().name("roll-logger".into()).spawn(move || files.run(rx))?;
    *g = Some(Handle { tx, current, dir, data_dir, thread: Some(thread) });
    RUNNING.store(true, Ordering::Relaxed);
    drop(g);
    super::refresh_active();
    Ok(())
}

/// Stop the collector, after it has written everything sent to it.
pub fn stop() {
    let h = COLLECTOR.write().take();
    RUNNING.store(false, Ordering::Relaxed);
    super::refresh_active();
    if let Some(mut h) = h {
        let t = h.thread.take();
        drop(h);
        if let Some(t) = t {
            let _ = t.join();
        }
    }
}

/// Wait until everything sent so far is written.
pub fn flush() {
    let txs: Vec<SyncSender<Msg>> =
        [COLLECTOR.read().as_ref().map(|h| h.tx.clone()), STDERR.lock().clone()].into_iter().flatten().collect();
    for tx in txs {
        let (ack, done) = sync_channel(1);
        if tx.send(Msg::Flush(ack)).is_ok() {
            let _ = done.recv_timeout(Duration::from_secs(10));
        }
    }
}

/// pg_rotate_logfile: false when there is no collector.
pub fn rotate() -> bool {
    let tx = COLLECTOR.read().as_ref().map(|h| h.tx.clone());
    tx.is_some_and(|tx| tx.send(Msg::Rotate).is_ok())
}

/// A reload's settings (SIGHUP to the syslogger).
pub fn reconfigure(cfg: Config) {
    let tx = COLLECTOR.read().as_ref().map(|h| h.tx.clone());
    if let Some(tx) = tx {
        let _ = tx.send(Msg::Config(cfg));
    }
}

/// pg_current_logfile: the file `kind` is written to (None: the first one listed).
pub fn current_file(kind: Option<Kind>) -> Option<String> {
    let g = COLLECTOR.read();
    let cur = g.as_ref()?.current.lock();
    cur.iter().find(|(k, _)| kind.is_none_or(|w| *k == w)).map(|(_, f)| f.clone())
}

/// The log directory the collector writes to (pg_ls_logdir; None: no collector).
pub fn log_dir() -> Option<PathBuf> {
    COLLECTOR.read().as_ref().map(|h| h.dir.lock().clone())
}

/// One format's open file.
struct Out {
    /// The name as current_logfiles lists it (log_directory/file).
    name: String,
    file: std::io::BufWriter<std::fs::File>,
    size: u64,
}

/// The collector thread's state.
struct Files {
    cfg: Config,
    /// stderr's file is always open (everything else written to stderr goes there too).
    out: [Option<Out>; 3],
    next_rotation: Option<i64>,
    current: std::sync::Arc<Mutex<Vec<(Kind, String)>>>,
    dir: std::sync::Arc<Mutex<PathBuf>>,
}

const KINDS: [Kind; 3] = [Kind::Stderr, Kind::Csv, Kind::Json];

fn slot(k: Kind) -> usize {
    match k {
        Kind::Stderr => 0,
        Kind::Csv => 1,
        Kind::Json => 2,
    }
}

impl Files {
    fn open(
        cfg: Config,
        current: std::sync::Arc<Mutex<Vec<(Kind, String)>>>,
        dir: std::sync::Arc<Mutex<PathBuf>>,
    ) -> std::io::Result<Files> {
        let mut f = Files { cfg, out: [None, None, None], next_rotation: None, current, dir };
        f.mkdir();
        let now = Clock::system().now;
        for k in KINDS {
            if f.wanted(k) {
                f.out[slot(k)] = Some(f.open_one(k, now, false)?);
            }
        }
        f.set_next_rotation();
        f.update_metainfo();
        Ok(f)
    }

    fn wanted(&self, k: Kind) -> bool {
        match k {
            Kind::Stderr => true,
            Kind::Csv => self.cfg.dest.csv,
            Kind::Json => self.cfg.dest.json,
        }
    }

    fn mkdir(&self) {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(self.cfg.dir());
        *self.dir.lock() = self.cfg.dir();
    }

    /// logfile_getname: log_directory/strftime(log_filename), ".log" swapped for the suffix.
    fn name(&self, k: Kind, at: i64) -> String {
        let mut n = format!("{}/{}", self.cfg.directory, strftime(&self.cfg.filename, at, &self.cfg.tz));
        let suffix = match k {
            Kind::Stderr => return n,
            Kind::Csv => ".csv",
            Kind::Json => ".json",
        };
        if n.len() > 4 && n.ends_with(".log") {
            n.truncate(n.len() - 4);
        }
        n + suffix
    }

    fn open_one(&self, k: Kind, at: i64, truncate: bool) -> std::io::Result<Out> {
        use std::os::unix::fs::OpenOptionsExt;
        let name = self.name(k, at);
        let path = self.cfg.data_dir.join(&name);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(!truncate)
            .write(true)
            .truncate(truncate)
            .mode(self.cfg.file_mode)
            .open(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Out { name, file: std::io::BufWriter::with_capacity(64 << 10, file), size })
    }

    /// set_next_rotation_time: the next multiple of log_rotation_age in log_timezone.
    fn set_next_rotation(&mut self) {
        let age = self.cfg.rotation_age * 60;
        if age <= 0 {
            self.next_rotation = None;
            return;
        }
        let now = Clock::system().now.div_euclid(1_000_000);
        let off = timestamp2tm(now * 1_000_000, Some(&self.cfg.tz)).map_or(0, |(t, _)| t.gmtoff);
        let local = now + off;
        self.next_rotation = Some((local - local.rem_euclid(age) + age - off) * 1_000_000);
    }

    /// logfile_rotate: new files for every format (`kinds`), named for `at`.
    fn rotate(&mut self, time_based: bool, kinds: [bool; 3]) {
        let at = match (time_based, self.next_rotation) {
            (true, Some(t)) => t,
            _ => Clock::system().now,
        };
        for k in KINDS {
            let i = slot(k);
            if !self.wanted(k) {
                if let Some(mut o) = self.out[i].take() {
                    let _ = o.file.flush();
                }
                continue;
            }
            if !kinds[i] && self.out[i].is_some() {
                continue;
            }
            let name = self.name(k, at);
            let truncate = self.cfg.truncate_on_rotation
                && time_based
                && self.out[i].as_ref().is_some_and(|o| o.name != name);
            match self.open_one(k, at, truncate) {
                Ok(o) => {
                    if let Some(mut old) = self.out[i].replace(o) {
                        let _ = old.file.flush();
                    }
                }
                Err(e) => {
                    // (PostgreSQL logs this and keeps writing to the old file.)
                    tracing::error!(file = name, error = %e, "could not open log file");
                }
            }
        }
        if time_based {
            self.set_next_rotation();
        }
        self.update_metainfo();
    }

    /// update_metainfo_datafile: current_logfiles, written to a temporary file and renamed.
    fn update_metainfo(&self) {
        let mut cur = Vec::new();
        for k in KINDS {
            let listed = match k {
                Kind::Stderr => self.cfg.dest.stderr,
                _ => self.wanted(k),
            };
            if let Some(o) = self.out[slot(k)].as_ref().filter(|_| listed) {
                cur.push((k, o.name.clone()));
            }
        }
        let path = self.cfg.data_dir.join("current_logfiles");
        if cur.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            let label = |k: Kind| match k {
                Kind::Stderr => "stderr",
                Kind::Csv => "csvlog",
                Kind::Json => "jsonlog",
            };
            let text: String = cur.iter().map(|(k, n)| format!("{} {n}\n", label(*k))).collect();
            let tmp = self.cfg.data_dir.join("current_logfiles.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
        *self.current.lock() = cur;
    }

    fn reconfigure(&mut self, cfg: Config) {
        let dir_changed = cfg.directory != self.cfg.directory || cfg.data_dir != self.cfg.data_dir;
        let mut rotate = dir_changed || cfg.filename != self.cfg.filename;
        rotate |= cfg.dest.csv != self.out[1].is_some() || cfg.dest.json != self.out[2].is_some();
        let age_changed = cfg.rotation_age != self.cfg.rotation_age;
        self.cfg = cfg;
        if dir_changed {
            self.mkdir();
        }
        if age_changed {
            self.set_next_rotation();
        }
        if rotate {
            self.rotate(false, [true; 3]);
        } else {
            self.update_metainfo();
        }
    }

    fn handle(&mut self, m: Msg) {
        match m {
            Msg::Line(k, s) => {
                // A format whose file isn't open (log_destination changed under it) goes to
                // stderr's, as PostgreSQL's does.
                let i = if self.out[slot(k)].is_some() { slot(k) } else { 0 };
                if let Some(o) = self.out[i].as_mut() {
                    let _ = o.file.write_all(s.as_bytes());
                    o.size += s.len() as u64;
                }
            }
            Msg::Flush(ack) => {
                self.flush();
                let _ = ack.send(());
            }
            Msg::Rotate => self.rotate(false, [true; 3]),
            Msg::Config(c) => self.reconfigure(c),
        }
    }

    fn flush(&mut self) {
        for o in self.out.iter_mut().flatten() {
            let _ = o.file.flush();
        }
    }

    fn run(&mut self, rx: Receiver<Msg>) {
        loop {
            let wait = self.next_rotation.map_or(Duration::from_secs(3600), |t| {
                Duration::from_micros((t - Clock::system().now).max(0) as u64)
            });
            match rx.recv_timeout(wait) {
                Ok(m) => {
                    let mut next = Some(m);
                    while let Some(m) = next {
                        self.handle(m);
                        next = rx.try_recv().ok();
                    }
                    self.flush();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.next_rotation.is_some_and(|t| Clock::system().now >= t) {
                self.rotate(true, [true; 3]);
            }
            let limit = self.cfg.rotation_size.max(0) as u64 * 1024;
            if limit > 0 {
                let full = [0, 1, 2].map(|i| self.out[i].as_ref().is_some_and(|o| o.size >= limit));
                if full.contains(&true) {
                    self.rotate(false, full);
                }
            }
        }
        self.flush();
    }
}

/// pg_strftime for log_filename, at `at` (microseconds since 2000-01-01) in `tz`.
pub fn strftime(fmt: &str, at: i64, tz: &TimeZone) -> String {
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const MONTHS: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
        "November", "December",
    ];
    let Some((t, _)) = timestamp2tm(at, Some(tz)) else { return fmt.to_string() };
    use crate::types::datetime::{date2j, j2day};
    let jd = date2j(t.year, t.mon, t.mday);
    let (wday, yday) = (j2day(jd), jd - date2j(t.year, 1, 1) + 1);
    let (wd, mo) = (DAYS[wday.rem_euclid(7) as usize], MONTHS[(t.mon - 1).clamp(0, 11) as usize]);
    let mut out = String::new();
    let mut it = fmt.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(e) = it.next() else {
            out.push('%');
            break;
        };
        let h12 = if t.hour % 12 == 0 { 12 } else { t.hour % 12 };
        let s = match e {
            'Y' => t.year.to_string(),
            'y' => format!("{:02}", t.year.rem_euclid(100)),
            'C' => format!("{:02}", t.year.div_euclid(100)),
            'm' => format!("{:02}", t.mon),
            'd' => format!("{:02}", t.mday),
            'e' => format!("{:2}", t.mday),
            'H' => format!("{:02}", t.hour),
            'I' => format!("{h12:02}"),
            'M' => format!("{:02}", t.min),
            'S' => format!("{:02}", t.sec),
            'j' => format!("{yday:03}"),
            'a' => wd[..3].to_string(),
            'A' => wd.to_string(),
            'b' | 'h' => mo[..3].to_string(),
            'B' => mo.to_string(),
            'p' => (if t.hour < 12 { "AM" } else { "PM" }).to_string(),
            'u' => (if wday == 0 { 7 } else { wday }).to_string(),
            'w' => wday.to_string(),
            'F' => format!("{}-{:02}-{:02}", t.year, t.mon, t.mday),
            'T' => format!("{:02}:{:02}:{:02}", t.hour, t.min, t.sec),
            'R' => format!("{:02}:{:02}", t.hour, t.min),
            'D' => format!("{:02}/{:02}/{:02}", t.mon, t.mday, t.year.rem_euclid(100)),
            's' => (at.div_euclid(1_000_000) + 946_684_800).to_string(),
            'Z' => t.zone.clone().unwrap_or_default(),
            'z' => {
                let o = t.gmtoff / 60;
                format!("{}{:02}{:02}", if o < 0 { '-' } else { '+' }, o.abs() / 60, o.abs() % 60)
            }
            'n' => "\n".into(),
            't' => "\t".into(),
            '%' => "%".into(),
            e => format!("%{e}"),
        };
        out.push_str(&s);
    }
    out
}
