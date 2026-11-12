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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Stdout,
    Stderr,
    Build,
    System,
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
    /// Lines not written to object storage yet.
    unflushed: Vec<LogLine>,
    tx: broadcast::Sender<LogLine>,
    seq: u64,
}

impl Log {
    fn new() -> Log {
        Log { ring: VecDeque::new(), unflushed: Vec::new(), tx: broadcast::channel(1024).0, seq: 0 }
    }
}

pub struct LogHub {
    logs: Mutex<HashMap<String, Log>>,
    objects: Option<Objects>,
}

impl LogHub {
    pub fn new(objects: Option<Objects>) -> Arc<LogHub> {
        Arc::new(LogHub { logs: Mutex::new(HashMap::new()), objects })
    }

    pub fn sink(self: &Arc<Self>, key: impl Into<String>, stream: Stream) -> LogSink {
        LogSink { hub: self.clone(), key: Arc::from(key.into()), stream, instance: None, deployment: None }
    }

    fn push(&self, key: &str, line: LogLine) {
        let mut logs = self.logs.lock();
        let log = logs.entry(key.to_string()).or_insert_with(Log::new);
        if log.ring.len() == RING {
            log.ring.pop_front();
        }
        log.ring.push_back(line.clone());
        log.unflushed.push(line.clone());
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

    /// Write every log's new lines to object storage (each key a chunk of its own).
    pub async fn flush(&self) {
        let Some(objects) = &self.objects else {
            self.logs.lock().values_mut().for_each(|l| l.unflushed.clear());
            return;
        };
        let batches: Vec<(String, Vec<LogLine>, u64)> = {
            let mut logs = self.logs.lock();
            logs.iter_mut()
                .filter(|(_, l)| !l.unflushed.is_empty())
                .map(|(k, l)| {
                    l.seq += 1;
                    (k.clone(), std::mem::take(&mut l.unflushed), l.seq)
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
                tracing::warn!(key, error = %e, "apps: writing a log chunk failed; the lines stay in memory only");
            }
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
