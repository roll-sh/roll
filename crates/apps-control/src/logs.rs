//! Build and runtime logs (docs/apps/README.md, "Observability and cost").
//!
//! Each log is a key: `build:<deployment>` for a build, `env:<service>/<env>` for an
//! environment's instances. A key has a ring of its last [`RING`] lines in memory and a broadcast
//! channel for followers, and its lines are written to object storage in chunks
//! (`apps/logs/<key>/<first ms>-<seq>.ndjson`), so they outlive the process and the node.

use parking_lot::Mutex;
use pg_storage::Objects;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Lines kept in memory per log.
pub const RING: usize = 5_000;
/// The longest line kept; the rest is cut.
pub const LINE_MAX: usize = 16 << 10;
/// Bytes a log holds between two flushes (about 10 s); a chattier process's further lines are
/// counted and dropped, and a line in their place says how many.
pub const UNFLUSHED_MAX: usize = 8 << 20;
/// What a search reads from stored chunks at most (about 7 days of a busy service's logs are
/// many times this: it says when it stopped).
pub const SEARCH_BYTES: usize = 64 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
    Build,
    System,
    /// The proxy's access log (`http:<service>/<env>`): a line a request, its text JSON.
    Http,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogLine {
    pub t: i64,
    pub stream: Stream,
    pub instance: Option<String>,
    pub deployment: Option<String>,
    pub text: String,
    /// The level the app's own line says (JSON's `level`/`severity`, logfmt's `level=`, a leading
    /// `ERROR`): read once as it's written; absent when it says none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<Level>,
    /// Part of a stack trace in the app's output (`errors::occurrences`' joining, as it's
    /// written): `start` on its first line, `more` on the rest from the same instance and stream,
    /// so a view shows the trace as one entry however other instances' lines fall between.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<Trace>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trace {
    Start,
    More,
}

/// A line's level, least to most severe (Cloud Run's severities, folded).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        Some(match s.to_ascii_lowercase().as_str() {
            "trace" | "verbose" => Level::Trace,
            "debug" | "dbg" => Level::Debug,
            "info" | "information" | "notice" | "log" => Level::Info,
            "warn" | "warning" => Level::Warn,
            "error" | "err" => Level::Error,
            "fatal" | "critical" | "crit" | "panic" | "alert" | "emergency" => Level::Fatal,
            _ => return None,
        })
    }

    /// pino's and bunyan's numbers (10 trace … 60 fatal).
    fn of_number(n: f64) -> Option<Level> {
        Some(match n as i64 {
            ..=10 => Level::Trace,
            11..=20 => Level::Debug,
            21..=30 => Level::Info,
            31..=40 => Level::Warn,
            41..=50 => Level::Error,
            _ => Level::Fatal,
        })
    }
}

/// What an app's line says its level is: a JSON object's `level`, `severity`, `lvl` or
/// `log.level` (a name or pino's number), logfmt's `level=`/`lvl=`, or an upper-case level word
/// among its first three (`ERROR`, `[WARN]`, `level:INFO` aside, a timestamp before it).
/// Anything else (prose that mentions an error) says none.
pub fn level_of(text: &str) -> Option<Level> {
    let t = text.trim_start();
    if t.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(t).ok()?;
        let o = v.as_object()?;
        let field = ["level", "severity", "lvl"].iter().find_map(|k| o.get(*k)).or_else(|| o.get("log").and_then(|l| l.get("level")))?;
        return match field {
            serde_json::Value::String(s) => Level::parse(s),
            serde_json::Value::Number(n) => n.as_f64().and_then(Level::of_number),
            _ => None,
        };
    }
    for token in t.split_whitespace().take(12) {
        if let Some(v) = token.strip_prefix("level=").or_else(|| token.strip_prefix("lvl=")) {
            return Level::parse(v.trim_matches('"'));
        }
    }
    t.split_whitespace().take(3).find_map(|w| {
        // `[notice]`, `[error]` (nginx's, Apache's): a bracketed level word is a level in any case.
        let bracketed = w.starts_with('[') && w.trim_end_matches(':').ends_with(']');
        let w = w.trim_matches(|c: char| matches!(c, '[' | ']' | '(' | ')' | ':' | '|'));
        (bracketed || (w.len() >= 3 && w.bytes().all(|b| b.is_ascii_uppercase()))).then(|| Level::parse(w)).flatten()
    })
}

struct Log {
    ring: VecDeque<LogLine>,
    /// Lines not written to object storage yet, their bytes, and how many more were dropped.
    unflushed: Vec<LogLine>,
    unflushed_bytes: usize,
    dropped: u64,
    tx: broadcast::Sender<LogLine>,
    seq: u64,
}

impl Log {
    fn new() -> Log {
        Log { ring: VecDeque::new(), unflushed: Vec::new(), unflushed_bytes: 0, dropped: 0, tx: broadcast::channel(1024).0, seq: 0 }
    }
}

/// The shortest value redacted: anything shorter (`1`, `true`, a port) is too common in a log
/// to hide without hiding what the log says.
const MIN_REDACT: usize = 6;
/// The most values one log redacts (each line of a multi-line value counts).
const MAX_REDACT: usize = 2000;
/// The deployments' values a log keeps redacting (the live one, a canary, a few before).
const REDACT_DEPLOYMENTS: usize = 8;

/// Secret values taken out of a log's lines as they're written (GitHub Actions' masking): an
/// environment's sealed and sensitive variables and its links' credentials, wherever a build
/// step, the app, a job or roll prints one. Each becomes `***`.
pub struct Redact(aho_corasick::AhoCorasick);

impl Redact {
    /// What redacts `values` (each, and each line of one over several), if any is long enough.
    pub fn of<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<Arc<Redact>> {
        let mut v: Vec<&str> = values.into_iter().flat_map(|v| std::iter::once(v).chain(v.lines().filter(move |_| v.contains('\n')))).map(str::trim).filter(|v| v.len() >= MIN_REDACT).collect();
        v.sort_unstable();
        v.dedup();
        v.truncate(MAX_REDACT);
        if v.is_empty() {
            return None;
        }
        aho_corasick::AhoCorasick::builder().match_kind(aho_corasick::MatchKind::LeftmostLongest).build(&v).ok().map(|a| Arc::new(Redact(a)))
    }

    /// The line with every value in it as `***`, or none when it holds none.
    pub fn apply(&self, text: &str) -> Option<String> {
        if !self.0.is_match(text) {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        self.0.replace_all_with(text, &mut out, |_, _, dst| {
            dst.push_str("***");
            true
        });
        Some(out)
    }
}

/// A log's redactions: each deployment's values (newest last), and all of them together.
#[derive(Default)]
struct Redactions {
    by: Vec<(String, Vec<String>)>,
    all: Option<Arc<Redact>>,
}

/// A log, an instance and a stream: where a trace is open.
type TraceKey = (Arc<str>, Option<Arc<str>>, u8);

/// Where the log tap sends `(log key, line)`.
pub type Tap = tokio::sync::mpsc::Sender<(Arc<str>, LogLine)>;

pub struct LogHub {
    logs: Mutex<HashMap<String, Log>>,
    objects: Option<Objects>,
    /// Runtime lines (`env:…` logs) for log drains, as they're written: bounded, so a slow drain
    /// drops lines (counted by the drainer) rather than holding memory or a writer.
    tap: Mutex<Option<Tap>>,
    /// Whether some drain asks for request (`http:`) and build (`build:`) lines.
    tap_http: std::sync::atomic::AtomicBool,
    tap_build: std::sync::atomic::AtomicBool,
    tap_traces: std::sync::atomic::AtomicBool,
    tap_metrics: std::sync::atomic::AtomicBool,
    /// The traces open in each log's instance and stream: their kind and lines so far.
    traces: Mutex<HashMap<TraceKey, (crate::errors::Kind, usize)>>,
    /// What each log takes out of its lines ([`Redact`]), by log key.
    redactions: Mutex<HashMap<String, Redactions>>,
}

impl LogHub {
    pub fn new(objects: Option<Objects>) -> Arc<LogHub> {
        Arc::new(LogHub { logs: Mutex::new(HashMap::new()), objects, tap: Mutex::new(None), tap_http: Default::default(), tap_build: Default::default(), tap_traces: Default::default(), tap_metrics: Default::default(), traces: Mutex::new(HashMap::new()), redactions: Mutex::new(HashMap::new()) })
    }

    /// Every runtime line from now on, `(log key, line)`, at most `capacity` waiting.
    pub fn tap(&self, capacity: usize) -> tokio::sync::mpsc::Receiver<(Arc<str>, LogLine)> {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        *self.tap.lock() = Some(tx);
        rx
    }

    /// A line for the drains alone (`audit:<service>/<env>`, an audit entry): kept in no log,
    /// dropped when no drainer runs or it's behind.
    pub fn to_drains(&self, key: &str, line: LogLine) {
        if let Some(tap) = self.tap.lock().as_ref() {
            let _ = tap.try_send((Arc::from(key), line));
        }
    }

    /// Which sources beyond runtime output some drain asks for (the drainer's, from the store).
    pub fn set_tapped(&self, http: bool, build: bool, traces: bool, metrics: bool) {
        self.tap_http.store(http, std::sync::atomic::Ordering::Relaxed);
        self.tap_build.store(build, std::sync::atomic::Ordering::Relaxed);
        self.tap_traces.store(traces, std::sync::atomic::Ordering::Relaxed);
        self.tap_metrics.store(metrics, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn sink(self: &Arc<Self>, key: impl Into<String>, stream: Stream) -> LogSink {
        LogSink { hub: self.clone(), key: Arc::from(key.into()), stream, instance: None, deployment: None, redact: None }
    }

    /// From now on `key`'s lines have `deployment`'s secret `values` taken out, with the values of
    /// the few deployments before it (an old instance still draining prints too).
    pub fn redact(&self, key: &str, deployment: &str, mut values: Vec<String>) {
        // Each as a JSON string holds it too (an OTLP record, a span's attribute, written as JSON).
        let escaped: Vec<String> = values.iter().filter_map(|v| serde_json::to_string(v).ok().map(|j| j[1..j.len() - 1].to_string()).filter(|j| j != v)).collect();
        values.extend(escaped);
        let mut all = self.redactions.lock();
        let r = all.entry(key.to_string()).or_default();
        r.by.retain(|(d, _)| d != deployment);
        r.by.push((deployment.to_string(), values));
        if r.by.len() > REDACT_DEPLOYMENTS {
            r.by.remove(0);
        }
        r.all = Redact::of(r.by.iter().flat_map(|(_, v)| v.iter().map(String::as_str)));
    }

    /// `to` redacts what `from` does (a job's log, its environment's values).
    pub fn redact_like(&self, from: &str, to: &str) {
        let mut all = self.redactions.lock();
        if let Some(r) = all.get(from) {
            let copy = Redactions { by: r.by.clone(), all: r.all.clone() };
            all.insert(to.to_string(), copy);
        }
    }

    fn redaction(&self, key: &str) -> Option<Arc<Redact>> {
        self.redactions.lock().get(key).and_then(|r| r.all.clone())
    }

    /// Whether an app's line starts a stack trace, goes on with the one open in its instance and
    /// stream, or neither (which closes any open there).
    fn trace_of(&self, key: &Arc<str>, instance: &Option<Arc<str>>, stream: Stream, text: &str) -> Option<Trace> {
        let k = (key.clone(), instance.clone(), stream as u8);
        let mut open = self.traces.lock();
        if let Some((kind, n)) = open.get(&k).copied() {
            match crate::errors::continues(kind, text, n) {
                Some(end) if n < crate::errors::MAX_TRACE_LINES => {
                    if end {
                        open.remove(&k);
                    } else {
                        open.insert(k, (kind, n + 1));
                    }
                    return Some(Trace::More);
                }
                _ => {
                    open.remove(&k);
                }
            }
        }
        let kind = crate::errors::starts(text)?;
        // Instances that ended mid-trace leave theirs open: bounded, whatever comes.
        if open.len() >= 10_000 {
            open.clear();
        }
        open.insert(k, (kind, 1));
        Some(Trace::Start)
    }

    fn push(&self, key: &str, line: LogLine) {
        // The drains' sources: an environment's output, and its requests and builds while a drain
        // asks for them (a request a line: nothing on the proxy's path otherwise).
        use std::sync::atomic::Ordering::Relaxed;
        let tapped = key.starts_with("env:")
            || (key.starts_with("http:") && self.tap_http.load(Relaxed))
            || (key.starts_with("build:") && self.tap_build.load(Relaxed))
            || (key.starts_with("trace:") && self.tap_traces.load(Relaxed))
            || (key.starts_with("metric:") && self.tap_metrics.load(Relaxed));
        if tapped
            && let Some(tap) = self.tap.lock().as_ref()
        {
            let _ = tap.try_send((Arc::from(key), line.clone()));
        }
        let mut logs = self.logs.lock();
        let log = logs.entry(key.to_string()).or_insert_with(Log::new);
        if log.ring.len() == RING {
            log.ring.pop_front();
        }
        log.ring.push_back(line.clone());
        if log.unflushed_bytes + line.text.len() > UNFLUSHED_MAX {
            log.dropped += 1;
        } else {
            log.unflushed_bytes += line.text.len();
            log.unflushed.push(line.clone());
        }
        let _ = log.tx.send(line);
    }

    /// The last `tail` lines in memory, and a receiver for what comes next (no line is in both,
    /// none is missed between them).
    pub fn follow(&self, key: &str, tail: usize) -> (Vec<LogLine>, broadcast::Receiver<LogLine>) {
        let mut logs = self.logs.lock();
        let log = logs.entry(key.to_string()).or_insert_with(Log::new);
        let skip = log.ring.len().saturating_sub(tail);
        (log.ring.iter().skip(skip).cloned().collect(), log.tx.subscribe())
    }

    /// The last `tail` lines, from memory or, for a log this process hasn't seen, from object
    /// storage.
    pub async fn tail(&self, key: &str, tail: usize, since: Option<i64>) -> Vec<LogLine> {
        let mem: Vec<LogLine> = {
            let logs = self.logs.lock();
            logs.get(key).map(|l| l.ring.iter().cloned().collect()).unwrap_or_default()
        };
        let mut lines = mem;
        if lines.len() < tail
            && let Some(objects) = &self.objects
        {
            let first_mem = lines.first().map(|l| l.t);
            let mut chunks = objects.list(&prefix(key)).await.unwrap_or_default();
            chunks.sort();
            let mut older: Vec<LogLine> = Vec::new();
            for (chunk, _) in chunks.iter().rev() {
                if older.len() + lines.len() >= tail {
                    break;
                }
                if let Ok(Some(b)) = objects.get(chunk).await {
                    let mut part: Vec<LogLine> = b.split(|c| *c == b'\n').filter_map(|l| serde_json::from_slice(l).ok()).collect();
                    part.retain(|l: &LogLine| first_mem.is_none_or(|f| l.t < f));
                    part.extend(older);
                    older = part;
                }
            }
            older.extend(lines);
            lines = older;
        }
        if let Some(s) = since {
            lines.retain(|l| l.t >= s);
        }
        let skip = lines.len().saturating_sub(tail);
        lines.split_off(skip)
    }

    /// How many of the lines still in memory since `since` contain `q` (any case) and came from
    /// the app (stdout, stderr): cheap, for an alert judged every minute (nothing stored is read).
    pub fn count_recent(&self, key: &str, q: &str, since: i64) -> usize {
        let q = q.to_lowercase();
        let logs = self.logs.lock();
        let Some(l) = logs.get(key) else { return 0 };
        l.ring.iter().rev().take_while(|ln| ln.t >= since).filter(|ln| matches!(ln.stream, Stream::Stdout | Stream::Stderr) && ln.text.to_lowercase().contains(&q)).count()
    }

    /// Lines that contain `q` (any case), newest last, at most `limit` ([`Self::read`]).
    pub async fn search(&self, key: &str, q: &str, since: Option<i64>, limit: usize) -> (Vec<LogLine>, bool) {
        let q = q.to_lowercase();
        self.read(key, move |l| l.text.to_lowercase().contains(&q), since, None, limit).await
    }

    /// Lines `keep` keeps, made at `since` or later and before `before`, newest last, at most
    /// `limit`: the lines in memory, then the stored chunks newest first (a chunk that starts
    /// at `before` or later skipped by its name, and none read past one that starts before
    /// `since`), until `limit` lines or [`SEARCH_BYTES`] read. The flag says whether it stopped
    /// before reading all.
    pub async fn read(&self, key: &str, keep: impl Fn(&LogLine) -> bool, since: Option<i64>, before: Option<i64>, limit: usize) -> (Vec<LogLine>, bool) {
        let hit = |l: &LogLine| since.is_none_or(|s| l.t >= s) && before.is_none_or(|b| l.t < b) && keep(l);
        let mem: Vec<LogLine> = self.logs.lock().get(key).map(|l| l.ring.iter().cloned().collect()).unwrap_or_default();
        let first_mem = mem.first().map(|l| l.t);
        let mut found: Vec<LogLine> = mem.into_iter().rev().filter(|l| hit(l)).take(limit).collect();
        let mut cut = false;
        if found.len() < limit
            && let Some(objects) = &self.objects
        {
            let mut chunks = objects.list(&prefix(key)).await.unwrap_or_default();
            chunks.sort();
            let mut read = 0usize;
            let starts = |chunk: &str| chunk.rsplit('/').next().and_then(|n| n.split('-').next()).and_then(|n| n.parse::<i64>().ok());
            for (chunk, _) in chunks.iter().rev() {
                if found.len() >= limit || read > SEARCH_BYTES {
                    cut = true;
                    break;
                }
                // `<first ms>-<seq>.ndjson`: every line of one that starts at `before` is later.
                if before.is_some_and(|b| starts(chunk).is_some_and(|t| t >= b)) {
                    continue;
                }
                let Ok(Some(b)) = objects.get(chunk).await else { continue };
                read += b.len();
                let mut part: Vec<LogLine> = b.split(|c| *c == b'\n').filter_map(|l| serde_json::from_slice(l).ok()).collect();
                part.retain(|l: &LogLine| first_mem.is_none_or(|f| l.t < f) && hit(l));
                found.extend(part.into_iter().rev().take(limit - found.len()));
                // One that starts before `since` may still end after it; none older can.
                if since.is_some_and(|s| starts(chunk).is_some_and(|t| t < s)) {
                    break;
                }
            }
        }
        found.reverse();
        (found, cut)
    }

    /// Write every log's new lines to object storage (each key a chunk of its own).
    pub async fn flush(&self) {
        let Some(objects) = &self.objects else {
            self.logs.lock().values_mut().for_each(|l| {
                l.unflushed.clear();
                l.unflushed_bytes = 0;
                l.dropped = 0;
            });
            return;
        };
        let batches: Vec<(String, Vec<LogLine>, u64)> = {
            let mut logs = self.logs.lock();
            logs.iter_mut()
                .filter(|(_, l)| !l.unflushed.is_empty() || l.dropped > 0)
                .map(|(k, l)| {
                    l.seq += 1;
                    let mut lines = std::mem::take(&mut l.unflushed);
                    if l.dropped > 0 {
                        let t = lines.last().map_or_else(code_sql::now_ms, |x| x.t);
                        let text = format!("… {} lines dropped: more than {} MiB of output in a few seconds", l.dropped, UNFLUSHED_MAX >> 20);
                        lines.push(LogLine { t, stream: Stream::System, instance: None, deployment: None, text, level: Some(Level::Warn), trace: None });
                    }
                    l.unflushed_bytes = 0;
                    l.dropped = 0;
                    (k.clone(), lines, l.seq)
                })
                .collect()
        };
        for (key, lines, seq) in batches {
            let mut body = Vec::new();
            for l in &lines {
                serde_json::to_writer(&mut body, l).expect("JSON");
                body.push(b'\n');
            }
            let name = format!("{}{:016}-{seq:06}.ndjson", prefix(&key), lines[0].t);
            if let Err(e) = objects.put(&name, body.into()).await {
                tracing::warn!(key, error = %e, "apps: writing a log chunk failed; it's tried again at the next flush");
                // Back in front of what came since, within the cap.
                if let Some(log) = self.logs.lock().get_mut(&key) {
                    let room = UNFLUSHED_MAX.saturating_sub(log.unflushed_bytes);
                    let mut kept: Vec<LogLine> = Vec::new();
                    let mut bytes = 0;
                    for l in lines {
                        if bytes + l.text.len() > room {
                            log.dropped += 1;
                            continue;
                        }
                        bytes += l.text.len();
                        kept.push(l);
                    }
                    log.unflushed_bytes += bytes;
                    kept.append(&mut log.unflushed);
                    log.unflushed = kept;
                }
            }
        }
    }

    /// Drop a finished log (a build's) from memory once object storage has all of it: reads come
    /// from there after. Without object storage, memory is the only copy and it stays.
    pub fn retire(&self, key: &str) {
        if self.objects.is_none() {
            return;
        }
        let mut logs = self.logs.lock();
        if logs.get(key).is_some_and(|l| l.unflushed.is_empty() && l.dropped == 0) {
            logs.remove(key);
            self.redactions.lock().remove(key);
        }
    }

    /// Forget a log in memory (a deleted environment); its chunks expire with the bucket's rule.
    pub fn forget(&self, key: &str) {
        self.logs.lock().remove(key);
        self.redactions.lock().remove(key);
    }

    pub async fn delete(&self, key: &str) {
        self.forget(key);
        if let Some(o) = &self.objects {
            let _ = o.delete_prefix(&prefix(key)).await;
        }
    }
}

fn prefix(key: &str) -> String {
    format!("apps/logs/{}/", key.replace(':', "/"))
}

/// Where a build's or an instance's output goes: one log, one stream.
#[derive(Clone)]
pub struct LogSink {
    hub: Arc<LogHub>,
    key: Arc<str>,
    stream: Stream,
    instance: Option<Arc<str>>,
    deployment: Option<Arc<str>>,
    /// Never written: a secret the lines may hold (a build's cache namespace), shown as `…`.
    redact: Option<Arc<str>>,
}

impl LogSink {
    pub fn stream(&self, stream: Stream) -> LogSink {
        LogSink { stream, ..self.clone() }
    }

    pub fn instance(&self, instance: &str, deployment: &str) -> LogSink {
        LogSink { instance: Some(instance.into()), deployment: Some(deployment.into()), ..self.clone() }
    }

    /// The same instance's deployment under another name (a sidecar's container).
    pub fn renamed(&self, instance: &str) -> LogSink {
        LogSink { instance: Some(instance.into()), ..self.clone() }
    }

    pub fn deployment(&self, deployment: &str) -> LogSink {
        LogSink { deployment: Some(deployment.into()), ..self.clone() }
    }

    /// The same sink, with `secret` taken out of every line it writes.
    pub fn redacting(&self, secret: &str) -> LogSink {
        LogSink { redact: (!secret.is_empty()).then(|| secret.into()), ..self.clone() }
    }

    pub fn line(&self, text: impl AsRef<str>) {
        let mut text = text.as_ref().trim_end_matches(['\r', '\n']).to_string();
        if let Some(r) = &self.redact {
            text = text.replace(&**r, "…");
        }
        if let Some(t) = self.hub.redaction(&self.key).and_then(|r| r.apply(&text)) {
            text = t;
        }
        if text.len() > LINE_MAX {
            let mut cut = LINE_MAX;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push('…');
        }
        let app = matches!(self.stream, Stream::Stdout | Stream::Stderr);
        let trace = if app { self.hub.trace_of(&self.key, &self.instance, self.stream, &text) } else { None };
        self.hub.push(
            &self.key,
            LogLine {
                t: code_sql::now_ms(),
                stream: self.stream,
                instance: self.instance.as_deref().map(str::to_string),
                deployment: self.deployment.as_deref().map(str::to_string),
                // The app's own lines say their level; roll's and the access log don't.
                level: app.then(|| level_of(&text)).flatten(),
                trace,
                text,
            },
        );
    }

    /// Feed raw output, split into lines (a partial last line waits for the next call).
    pub fn writer(&self) -> LineWriter {
        LineWriter { sink: self.clone(), buf: Vec::new() }
    }
}

pub struct LineWriter {
    sink: LogSink,
    buf: Vec<u8>,
}

impl LineWriter {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            self.sink.line(String::from_utf8_lossy(&line));
        }
        if self.buf.len() > LINE_MAX {
            let line = std::mem::take(&mut self.buf);
            self.sink.line(String::from_utf8_lossy(&line));
        }
    }

    pub fn finish(mut self) {
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.sink.line(String::from_utf8_lossy(&line));
        }
    }
}

/// Copy an async reader's lines into a sink until it ends.
pub async fn pump(mut r: impl tokio::io::AsyncRead + Unpin, sink: LogSink) {
    use tokio::io::AsyncReadExt;
    let mut w = sink.writer();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => w.push(&buf[..n]),
        }
    }
    w.finish();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log's secret values are `***` wherever a line holds them, a multi-line value's lines too,
    /// and a value too short to hide is left.
    #[tokio::test]
    async fn secret_values_are_taken_out_of_a_logs_lines() {
        let hub = LogHub::new(None);
        hub.redact("env:svc_x/production", "dep_a", vec!["s3cr3t-token".into(), "abc".into(), "-----BEGIN KEY-----\nMIIBOgIBAAJBAKj34GkxFhD9\n-----END KEY-----".into()]);
        let sink = hub.sink("env:svc_x/production", Stream::Stdout);
        sink.line("auth with s3cr3t-token then abc");
        sink.line("MIIBOgIBAAJBAKj34GkxFhD9");
        hub.redact_like("env:svc_x/production", "job:job_1");
        hub.sink("job:job_1", Stream::Stdout).line("s3cr3t-token");
        let got: Vec<String> = hub.tail("env:svc_x/production", 10, None).await.into_iter().map(|l| l.text).collect();
        assert_eq!(got, ["auth with *** then abc", "***"]);
        assert_eq!(hub.tail("job:job_1", 10, None).await[0].text, "***");
        // Written as JSON (an OTLP record), a value with a quote in it is still found.
        hub.redact("trace:svc_x/production", "dep_a", vec!["pa\"ss-word".into()]);
        hub.sink("trace:svc_x/production", Stream::System).line(serde_json::json!({ "attr": "pa\"ss-word" }).to_string());
        assert_eq!(hub.tail("trace:svc_x/production", 10, None).await[0].text, r#"{"attr":"***"}"#);
    }

    /// A trace's lines are marked as one however another instance's lines fall between them,
    /// and the first line that doesn't go on ends it.
    #[tokio::test]
    async fn a_stack_traces_lines_are_marked_as_one_trace() {
        let hub = LogHub::new(None);
        let base = hub.sink("env:svc_x/production", Stream::Stderr);
        let (a, b) = (base.instance("a", "d"), base.instance("b", "d"));
        a.line("Traceback (most recent call last):");
        b.line("GET / 200");
        a.line("  File \"/app/x.py\", line 3, in f");
        a.line("ZeroDivisionError: division by zero");
        a.line("after");
        let marks: Vec<(String, Option<Trace>)> = hub.tail("env:svc_x/production", 10, None).await.into_iter().map(|l| (l.text, l.trace)).collect();
        let want = [Some(Trace::Start), None, Some(Trace::More), Some(Trace::More), None];
        assert_eq!(marks.iter().map(|(_, t)| *t).collect::<Vec<_>>(), want, "{marks:?}");
    }

    #[tokio::test]
    async fn a_search_finds_lines_in_stored_chunks_and_in_memory_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let hub = LogHub::new(Some(Objects::new(&pg_storage::StorageConfig::Local(dir.path().to_path_buf())).unwrap()));
        let sink = hub.sink("env:svc_x/production", Stream::Stdout);
        for l in ["GET / 200", "ERROR db timeout", "GET /a 200"] {
            sink.line(l);
        }
        hub.flush().await;
        hub.forget("env:svc_x/production");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        for l in ["GET /b 200", "error: retry"] {
            sink.line(l);
        }
        let (found, cut) = hub.search("env:svc_x/production", "Error", None, 100).await;
        assert_eq!(found.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["ERROR db timeout", "error: retry"]);
        assert!(!cut);
        // A window back in time (`before` the newer lines: only the stored ones), filtered.
        let newer = hub.tail("env:svc_x/production", 2, None).await[0].t;
        let (old, _) = hub.read("env:svc_x/production", |l| l.text.starts_with("GET"), None, Some(newer), 100).await;
        assert_eq!(old.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["GET / 200", "GET /a 200"]);
    }

    #[tokio::test]
    async fn a_flood_of_output_is_capped_between_flushes_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let hub = LogHub::new(Some(Objects::new(&pg_storage::StorageConfig::Local(dir.path().to_path_buf())).unwrap()));
        let sink = hub.sink("env:svc_x/production", Stream::Stdout);
        let line = "x".repeat(LINE_MAX - 1);
        for _ in 0..(UNFLUSHED_MAX / LINE_MAX + 100) {
            sink.line(&line);
        }
        assert!(hub.logs.lock()["env:svc_x/production"].unflushed_bytes <= UNFLUSHED_MAX);
        hub.flush().await;
        hub.forget("env:svc_x/production");
        let stored = hub.tail("env:svc_x/production", 1, None).await;
        assert!(stored[0].text.contains("lines dropped"), "{:?}", stored[0].text);
    }
}

#[cfg(test)]
mod level_tests {
    use super::{Level, level_of};

    #[test]
    fn a_lines_level_is_what_the_app_says_and_prose_says_none() {
        for (line, want) in [
            (r#"{"level":"error","msg":"db down"}"#, Some(Level::Error)),
            (r#"{"severity":"WARNING","message":"slow"}"#, Some(Level::Warn)),
            (r#"{"level":50,"msg":"pino"}"#, Some(Level::Error)),
            (r#"{"log":{"level":"debug"}}"#, Some(Level::Debug)),
            ("time=2026-11-14T10:00:00Z level=warn msg=\"retrying\"", Some(Level::Warn)),
            ("2026-11-14 10:00:00,123 ERROR app.db: connection refused", Some(Level::Error)),
            ("[INFO] listening on :8080", Some(Level::Info)),
            ("FATAL: out of memory", Some(Level::Fatal)),
            ("2026/11/14 23:57:32 [notice] 7#7: start worker process 51", Some(Level::Info)),
            ("2026/11/14 23:57:32 [error] 7#7: open() failed", Some(Level::Error)),
            ("[here] is a note", None),
            ("an error happened while retrying", None),
            ("Error: something", None),
            (r#"{"msg":"no level"}"#, None),
        ] {
            assert_eq!(level_of(line), want, "{line}");
        }
        assert!(Level::Warn < Level::Error && Level::Info < Level::Warn);
    }
}
