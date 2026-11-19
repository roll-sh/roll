//! Each cluster's security audit log ([`search_es::xpack::security::audit`] decides what is
//! recorded and writes the entries; this keeps them).
//!
//! Entries are kept in object storage next to the cluster's data
//! ([`search_store::Store::write_audit`]): a node batches a cluster's entries in memory and
//! writes them as one object every [`AuditConfig::flush_every`] (and when it shuts down), so a
//! burst of failed logins costs one write, and a cluster nothing happens to costs nothing (no
//! object, no timer: the flusher runs only while entries wait). On a fleet the cluster's owner
//! records and writes them (requests for a cluster run there); keys are unique per write, so two
//! nodes never overwrite each other. Entries older than [`AuditConfig::retention`] are left out
//! of reads, and their objects are deleted when the cluster's log is next written (at most
//! every ten minutes per cluster), as are the oldest ones past [`AuditConfig::max_bytes`]. A
//! deleted cluster's entries go with its storage ([`AuditLogs::forget`] drops what is still
//! waiting first). A node without object storage keeps each cluster's entries in memory, within
//! the same window and size.
//!
//! Entries never hold a credential or a request body: see the module above.

use parking_lot::Mutex;
use search_es::xpack::security::audit::{Record, Sink, Snapshot};
use serde_json::Value;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

/// How long, and how much, a cluster's audit log keeps.
#[derive(Clone, Debug)]
pub struct AuditConfig {
    /// Entries older than this are dropped (default 7 days).
    pub retention: Duration,
    /// Stored bytes per cluster (compressed; in memory, the entries' size); past it the oldest
    /// go (default 32 MiB).
    pub max_bytes: u64,
    /// How long entries wait before they are written (default 5 s).
    pub flush_every: Duration,
}

impl Default for AuditConfig {
    fn default() -> Self {
        AuditConfig { retention: Duration::from_secs(7 * 86_400), max_bytes: 32 << 20, flush_every: Duration::from_secs(5) }
    }
}

/// Entries waiting to be written, past which new ones are dropped (object storage failing).
const MAX_PENDING_BYTES: usize = 8 << 20;
/// How often a cluster's stored entries are checked against the window and size.
const SWEEP_EVERY_MS: i64 = 10 * 60_000;
/// The most entries one read returns.
pub const MAX_READ: usize = 1000;

struct Line {
    at_ms: i64,
    /// `{"at_ms":…,"event":…,"entry":{…}}`.
    json: String,
}

#[derive(Default)]
struct Pending {
    lines: VecDeque<Line>,
    bytes: usize,
    dropped: u64,
}

#[derive(Default)]
struct ClusterLog {
    pending: Mutex<Pending>,
    /// Held while the log's objects are written, swept or forgotten.
    io: tokio::sync::Mutex<()>,
    last_sweep_ms: AtomicI64,
}

/// What a read asks for.
#[derive(Clone, Debug, Default)]
pub struct AuditQuery {
    /// Unix milliseconds, inclusive.
    pub from_ms: Option<i64>,
    /// Unix milliseconds, exclusive.
    pub to_ms: Option<i64>,
    /// Event names or categories (Elasticsearch's `event.action` too), any case; empty: all.
    pub events: Vec<String>,
    /// At most this many, newest first (up to [`MAX_READ`]).
    pub size: usize,
}

/// A read's answer: entries newest first, and whether older ones matched past `size`.
#[derive(Debug, Default)]
pub struct AuditPage {
    pub entries: Vec<Value>,
    pub more: bool,
}

/// The node's audit logs, one per cluster with entries.
pub struct AuditLogs {
    store: Option<search_store::Store>,
    cfg: AuditConfig,
    logs: Mutex<HashMap<String, Arc<ClusterLog>>>,
    /// What each sleeping cluster recorded when it was last loaded here (its filter), for the
    /// failed logins the node refuses without waking it.
    remembered: Mutex<HashMap<String, Arc<Snapshot>>>,
    flusher: AtomicBool,
    me: Weak<AuditLogs>,
}

fn now_ms() -> i64 {
    search_es::time::now_millis()
}

impl AuditLogs {
    pub fn new(store: Option<search_store::Store>, cfg: AuditConfig) -> Arc<AuditLogs> {
        Arc::new_cyclic(|me| AuditLogs { store, cfg, logs: Mutex::new(HashMap::new()), remembered: Mutex::new(HashMap::new()), flusher: AtomicBool::new(false), me: me.clone() })
    }

    pub fn config(&self) -> &AuditConfig {
        &self.cfg
    }

    fn log(&self, cluster: &str) -> Arc<ClusterLog> {
        self.logs.lock().entry(cluster.to_string()).or_default().clone()
    }

    /// Remembers what a cluster records, as it unloads.
    pub fn remember(&self, cluster: &str, snapshot: Arc<Snapshot>) {
        self.remembered.lock().insert(cluster.to_string(), snapshot);
    }

    /// What a sleeping cluster recorded when it was last loaded here.
    pub fn remembered(&self, cluster: &str) -> Option<Arc<Snapshot>> {
        self.remembered.lock().get(cluster).cloned()
    }

    /// Drops a deleted cluster's log: entries still waiting are written first (a deleted cluster
    /// kept for restoring takes them along), and a write in progress finishes before this
    /// returns, so nothing lands after its storage is deleted or moved.
    pub async fn forget(&self, cluster: &str) {
        self.remembered.lock().remove(cluster);
        let log = self.logs.lock().remove(cluster);
        if let Some(log) = log {
            self.flush(cluster, &log).await;
            let _io = log.io.lock().await;
            *log.pending.lock() = Pending::default();
        }
    }

    fn start_flusher(&self) {
        if self.store.is_none() || self.flusher.swap(true, Ordering::SeqCst) {
            return;
        }
        let (Some(me), Ok(rt)) = (self.me.upgrade(), tokio::runtime::Handle::try_current()) else {
            self.flusher.store(false, Ordering::SeqCst);
            return;
        };
        let me = Arc::downgrade(&me);
        rt.spawn(async move {
            loop {
                let every = match me.upgrade() {
                    Some(l) => l.cfg.flush_every,
                    None => return,
                };
                tokio::time::sleep(every).await;
                let Some(logs) = me.upgrade() else { return };
                logs.flush_all().await;
                // Stop once nothing waits; a record that raced the check starts it again.
                if !logs.waiting() {
                    logs.flusher.store(false, Ordering::SeqCst);
                    if !logs.waiting() || logs.flusher.swap(true, Ordering::SeqCst) {
                        return;
                    }
                }
            }
        });
    }

    fn waiting(&self) -> bool {
        self.logs.lock().values().any(|l| !l.pending.lock().lines.is_empty())
    }

    /// Writes every cluster's waiting entries (also at shutdown).
    pub async fn flush_all(&self) {
        let logs: Vec<(String, Arc<ClusterLog>)> = self.logs.lock().iter().map(|(n, l)| (n.clone(), l.clone())).collect();
        for (name, log) in logs {
            self.flush(&name, &log).await;
        }
        // A cluster with nothing waiting and no write in progress needs no entry (only with
        // object storage: in memory, the waiting entries are the log).
        if self.store.is_some() {
            self.logs.lock().retain(|_, l| Arc::strong_count(l) > 1 || !l.pending.lock().lines.is_empty());
        }
    }

    async fn flush(&self, cluster: &str, log: &ClusterLog) {
        let Some(store) = &self.store else { return };
        let _io = log.io.lock().await;
        let (lines, dropped) = {
            let mut p = log.pending.lock();
            p.bytes = 0;
            (std::mem::take(&mut p.lines), std::mem::take(&mut p.dropped))
        };
        if dropped > 0 {
            tracing::warn!(cluster, dropped, "audit entries dropped: object storage kept refusing them");
        }
        if lines.is_empty() {
            return;
        }
        let first = lines.iter().map(|l| l.at_ms).min().unwrap_or(0);
        let last = lines.iter().map(|l| l.at_ms).max().unwrap_or(0);
        let mut buf = Vec::with_capacity(lines.iter().map(|l| l.json.len() + 1).sum());
        for l in &lines {
            buf.extend_from_slice(l.json.as_bytes());
            buf.push(b'\n');
        }
        if let Err(e) = store.write_audit(cluster, first, last, &buf).await {
            tracing::warn!(cluster, error = %format!("{e:#}"), "writing audit entries failed; retrying with the next batch");
            let mut p = log.pending.lock();
            let mut restored: VecDeque<Line> = lines;
            restored.extend(p.lines.drain(..));
            p.bytes = restored.iter().map(|l| l.json.len()).sum();
            p.lines = restored;
            while p.bytes > MAX_PENDING_BYTES {
                let Some(l) = p.lines.pop_front() else { break };
                p.bytes -= l.json.len();
                p.dropped += 1;
            }
            return;
        }
        let now = now_ms();
        if now - log.last_sweep_ms.load(Ordering::Relaxed) >= SWEEP_EVERY_MS {
            log.last_sweep_ms.store(now, Ordering::Relaxed);
            if let Err(e) = self.sweep(store, cluster, now).await {
                tracing::debug!(cluster, error = %format!("{e:#}"), "sweeping old audit entries failed");
            }
        }
    }

    /// Deletes a cluster's objects past the window, then its oldest past the size cap.
    async fn sweep(&self, store: &search_store::Store, cluster: &str, now: i64) -> anyhow::Result<()> {
        let objects = store.list_audit(cluster).await?;
        let cutoff = now - self.cfg.retention.as_millis() as i64;
        let (old, mut kept): (Vec<_>, Vec<_>) = objects.into_iter().partition(|o| o.last_ms < cutoff);
        kept.sort_by_key(|o| o.last_ms);
        let mut total: u64 = kept.iter().map(|o| o.bytes).sum();
        let mut over = Vec::new();
        for o in &kept {
            if total <= self.cfg.max_bytes {
                break;
            }
            total -= o.bytes;
            over.push(o.clone());
        }
        store.delete_audit(&old).await?;
        store.delete_audit(&over).await
    }

    /// A cluster's entries matching `q`, newest first. This node's entries still waiting are
    /// written first, so the owner reads what it just recorded.
    pub async fn read(&self, cluster: &str, q: &AuditQuery) -> anyhow::Result<AuditPage> {
        let floor = now_ms() - self.cfg.retention.as_millis() as i64;
        let from = q.from_ms.unwrap_or(i64::MIN).max(floor);
        let to = q.to_ms.unwrap_or(i64::MAX);
        let mut top = Top { size: q.size.clamp(1, MAX_READ), heap: BinaryHeap::new(), more: false, events: &q.events };
        let log = self.logs.lock().get(cluster).cloned();
        let Some(store) = &self.store else {
            if let Some(log) = log {
                for l in log.pending.lock().lines.iter() {
                    top.offer(&l.json, from, to);
                }
            }
            return Ok(top.finish());
        };
        if let Some(log) = log {
            self.flush(cluster, &log).await;
        }
        let mut objects: Vec<_> = store.list_audit(cluster).await?.into_iter().filter(|o| o.last_ms >= from && o.first_ms < to).collect();
        objects.sort_by_key(|o| std::cmp::Reverse(o.last_ms));
        for o in objects {
            // This object's entries, and those of every one after it, are older than the
            // oldest kept.
            if top.full() && top.oldest().is_some_and(|t| o.last_ms < t) {
                top.more = true;
                break;
            }
            let Some(raw) = store.read_audit(&o).await? else { continue };
            for line in raw.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
                if let Ok(s) = std::str::from_utf8(line) {
                    top.offer(s, from, to);
                }
            }
        }
        Ok(top.finish())
    }
}

/// The newest `size` matching entries.
struct Top<'a> {
    size: usize,
    /// Oldest on top.
    heap: BinaryHeap<std::cmp::Reverse<(i64, u64, String)>>,
    more: bool,
    events: &'a [String],
}

impl Top<'_> {
    fn full(&self) -> bool {
        self.heap.len() >= self.size
    }

    fn oldest(&self) -> Option<i64> {
        self.heap.peek().map(|r| r.0.0)
    }

    fn offer(&mut self, json: &str, from: i64, to: i64) {
        let Ok(v) = serde_json::from_str::<Value>(json) else { return };
        let at = v["at_ms"].as_i64().unwrap_or(0);
        if at < from || at >= to {
            return;
        }
        if !self.events.is_empty() {
            let names = [v["event"].as_str(), v["entry"]["event.action"].as_str()];
            if !names.iter().flatten().any(|n| self.events.iter().any(|e| e.eq_ignore_ascii_case(n))) {
                return;
            }
        }
        let seq = self.heap.len() as u64;
        self.heap.push(std::cmp::Reverse((at, seq, json.to_string())));
        if self.heap.len() > self.size {
            self.heap.pop();
            self.more = true;
        }
    }

    fn finish(self) -> AuditPage {
        let mut all: Vec<(i64, u64, String)> = self.heap.into_iter().map(|r| r.0).collect();
        all.sort_by_key(|a| std::cmp::Reverse((a.0, a.1)));
        AuditPage { entries: all.into_iter().filter_map(|(_, _, j)| serde_json::from_str(&j).ok()).collect(), more: self.more }
    }
}

impl Sink for AuditLogs {
    fn record(&self, cluster: &str, r: Record) {
        // `event` is one of search-es's names (ASCII letters and `_`), `entry` its JSON object.
        let json = format!(r#"{{"at_ms":{},"event":"{}","entry":{}}}"#, r.at_ms, r.event, r.entry);
        let log = self.log(cluster);
        {
            let mut p = log.pending.lock();
            if self.store.is_none() {
                // In memory the waiting entries are the log: kept within the window and size.
                let floor = r.at_ms - self.cfg.retention.as_millis() as i64;
                while p.lines.front().is_some_and(|l| l.at_ms < floor) || (p.bytes + json.len()) as u64 > self.cfg.max_bytes {
                    let Some(l) = p.lines.pop_front() else { break };
                    p.bytes -= l.json.len();
                }
            } else if p.bytes + json.len() > MAX_PENDING_BYTES {
                p.dropped += 1;
                return;
            }
            p.bytes += json.len();
            p.lines.push_back(Line { at_ms: r.at_ms, json });
        }
        self.start_flusher();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(at_ms: i64, event: &'static str) -> Record {
        Record { at_ms, event, entry: format!(r#"{{"event.action":"{event}","n":{at_ms}}}"#) }
    }

    #[tokio::test]
    async fn reads_are_newest_first_within_the_window_and_filter() {
        let logs = AuditLogs::new(None, AuditConfig::default());
        let now = now_ms();
        logs.record("c1", rec(now - 8 * 86_400_000, "access_denied"));
        for i in 0..5 {
            logs.record("c1", rec(now - 1000 + i, if i % 2 == 0 { "access_denied" } else { "authentication_failed" }));
        }
        logs.record("c2", rec(now, "access_denied"));
        let page = logs.read("c1", &AuditQuery { size: 10, ..Default::default() }).await.unwrap();
        let ats: Vec<i64> = page.entries.iter().map(|e| e["at_ms"].as_i64().unwrap()).collect();
        assert_eq!(ats, (0..5).rev().map(|i| now - 1000 + i).collect::<Vec<_>>(), "outside the window or another cluster's");
        let page = logs.read("c1", &AuditQuery { size: 2, events: vec!["ACCESS_DENIED".into()], ..Default::default() }).await.unwrap();
        assert_eq!(page.entries.len(), 2);
        assert!(page.more);
        assert!(page.entries.iter().all(|e| e["event"] == "access_denied"));
        let page = logs.read("c1", &AuditQuery { size: 10, from_ms: Some(now - 998), to_ms: Some(now - 997), ..Default::default() }).await.unwrap();
        assert_eq!(page.entries.len(), 1);
        logs.forget("c1").await;
        assert!(logs.read("c1", &AuditQuery { size: 10, ..Default::default() }).await.unwrap().entries.is_empty());
    }
}
