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

/// Where the log tap sends `(log key, line)`.
pub type Tap = tokio::sync::mpsc::Sender<(Arc<str>, LogLine)>;

pub struct LogHub {
    logs: Mutex<HashMap<String, Log>>,
    objects: Option<Objects>,
    /// Runtime lines (`env:…` logs) for log drains, as they're written: bounded, so a slow drain
    /// drops lines (counted by the drainer) rather than holding memory or a writer.
    tap: Mutex<Option<Tap>>,
}

impl LogHub {
    pub fn new(objects: Option<Objects>) -> Arc<LogHub> {
        Arc::new(LogHub { logs: Mutex::new(HashMap::new()), objects, tap: Mutex::new(None) })
    }

    /// Every runtime line from now on, `(log key, line)`, at most `capacity` waiting.
    pub fn tap(&self, capacity: usize) -> tokio::sync::mpsc::Receiver<(Arc<str>, LogLine)> {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        *self.tap.lock() = Some(tx);
        rx
    }

    pub fn sink(self: &Arc<Self>, key: impl Into<String>, stream: Stream) -> LogSink {
        LogSink { hub: self.clone(), key: Arc::from(key.into()), stream, instance: None, deployment: None }
    }

    fn push(&self, key: &str, line: LogLine) {
        if key.starts_with("env:")
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

    /// Lines that contain `q` (any case), newest last, at most `limit`: the lines in memory, then
    /// the stored chunks newest first (those older than `since` skipped by name), until `limit`
    /// matches or [`SEARCH_BYTES`] read. The flag says whether it stopped before reading all.
    pub async fn search(&self, key: &str, q: &str, since: Option<i64>, limit: usize) -> (Vec<LogLine>, bool) {
        let q = q.to_lowercase();
        let hit = |l: &LogLine| l.text.to_lowercase().contains(&q) && since.is_none_or(|s| l.t >= s);
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
            for (chunk, _) in chunks.iter().rev() {
                // `<first ms>-<seq>.ndjson`: a chunk that starts before `since` may still end after it.
                if found.len() >= limit || read > SEARCH_BYTES {
                    cut = true;
                    break;
                }
                let Ok(Some(b)) = objects.get(chunk).await else { continue };
                read += b.len();
                let mut part: Vec<LogLine> = b.split(|c| *c == b'\n').filter_map(|l| serde_json::from_slice(l).ok()).collect();
                part.retain(|l: &LogLine| first_mem.is_none_or(|f| l.t < f) && hit(l));
                found.extend(part.into_iter().rev().take(limit - found.len()));
                if since.is_some_and(|s| chunk.rsplit('/').next().and_then(|n| n.split('-').next()).and_then(|n| n.parse::<i64>().ok()).is_some_and(|t| t < s)) {
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
                        lines.push(LogLine { t, stream: Stream::System, instance: None, deployment: None, text });
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
        }
    }

    /// Forget a log in memory (a deleted environment); its chunks expire with the bucket's rule.
    pub fn forget(&self, key: &str) {
        self.logs.lock().remove(key);
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
}

impl LogSink {
    pub fn stream(&self, stream: Stream) -> LogSink {
        LogSink { stream, ..self.clone() }
    }

    pub fn instance(&self, instance: &str, deployment: &str) -> LogSink {
        LogSink { instance: Some(instance.into()), deployment: Some(deployment.into()), ..self.clone() }
    }

    pub fn deployment(&self, deployment: &str) -> LogSink {
        LogSink { deployment: Some(deployment.into()), ..self.clone() }
    }

    pub fn line(&self, text: impl AsRef<str>) {
        let mut text = text.as_ref().trim_end_matches(['\r', '\n']).to_string();
        if text.len() > LINE_MAX {
            let mut cut = LINE_MAX;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push('…');
        }
        self.hub.push(
            &self.key,
            LogLine {
                t: code_sql::now_ms(),
                stream: self.stream,
                instance: self.instance.as_deref().map(str::to_string),
                deployment: self.deployment.as_deref().map(str::to_string),
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
