//! LISTEN / NOTIFY. Each database has a hub of listening sessions. A NOTIFY is queued in its
//! transaction and, once that commits, reaches every session listening on the channel: written
//! to the client at once when that session is idle (waiting for a command, outside a
//! transaction block), otherwise when it next goes idle. Nothing polls.

use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    /// The notifying session's process id.
    pub pid: i32,
    pub channel: String,
    pub payload: String,
}

/// Writes notifications to a session's client; an error drops them (the client is gone).
pub type Sink = Arc<dyn Fn(&[Notification]) -> std::io::Result<()> + Send + Sync>;

#[derive(Default)]
struct Listener {
    channels: HashSet<String>,
    pending: VecDeque<Notification>,
    /// Waiting for the client outside a transaction block: deliver straight away.
    idle: bool,
    sink: Option<Sink>,
}

#[derive(Default)]
pub struct Hub {
    listeners: Mutex<HashMap<i32, Arc<Mutex<Listener>>>>,
    /// The queue's head (QUEUE_HEAD): its page and the offset in it, as committed
    /// notifications advance it (what pg_stat_slru counts the notify SLRU's new pages by).
    head: Mutex<(u64, usize)>,
}

/// The notify queue's page size (QUEUE_PAGESIZE, BLCKSZ).
const QUEUE_PAGESIZE: usize = 8192;
/// AsyncQueueEntryEmptySize, QUEUEALIGNed: a header and two string terminators.
const ENTRY_EMPTY: usize = 20;

/// An AsyncQueueEntry's length: the 16-byte header, the channel and payload with their
/// terminators, 4-aligned.
fn entry_len(channel: &str, payload: &str) -> usize {
    (16 + channel.len() + 1 + payload.len() + 1).div_ceil(4) * 4
}

impl Hub {
    fn entry(&self, pid: i32) -> Arc<Mutex<Listener>> {
        self.listeners.lock().entry(pid).or_default().clone()
    }

    fn get(&self, pid: i32) -> Option<Arc<Mutex<Listener>>> {
        self.listeners.lock().get(&pid).cloned()
    }

    /// Deliver to `pid`'s client through `sink` while it is idle.
    pub fn attach(&self, pid: i32, sink: Sink) {
        self.entry(pid).lock().sink = Some(sink);
    }

    /// The session ended.
    pub fn detach(&self, pid: i32) {
        self.listeners.lock().remove(&pid);
    }

    /// The channels `pid` listens on (committed LISTENs), sorted.
    pub fn channels(&self, pid: i32) -> Vec<String> {
        let mut v: Vec<String> = self
            .get(pid)
            .map(|l| l.lock().channels.iter().cloned().collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    pub fn listen(&self, pid: i32, channel: &str) {
        self.entry(pid).lock().channels.insert(channel.to_string());
    }

    /// UNLISTEN `channel`, or every channel (`None`).
    pub fn unlisten(&self, pid: i32, channel: Option<&str>) {
        if let Some(l) = self.get(pid) {
            let mut l = l.lock();
            match channel {
                Some(c) => {
                    l.channels.remove(c);
                }
                None => l.channels.clear(),
            }
        }
    }

    /// Append a committing transaction's notifications to the queue
    /// (asyncQueueAddEntries): the pages it zeroed, the first page ever and each page the
    /// head moves onto (an entry not fitting in what is left of a page fills it with a
    /// dummy entry first).
    pub fn queue_entries(&self, notes: &[(String, String)]) -> i64 {
        if notes.is_empty() {
            return 0;
        }
        let mut head = self.head.lock();
        let (mut page, mut offset) = *head;
        let mut zeroed = i64::from(page == 0 && offset == 0);
        let mut it = notes.iter().peekable();
        while let Some((c, p)) = it.peek() {
            let mut len = entry_len(c, p);
            if offset + len <= QUEUE_PAGESIZE {
                it.next();
            } else {
                len = QUEUE_PAGESIZE - offset;
            }
            // asyncQueueAdvance.
            offset += len;
            if offset + ENTRY_EMPTY > QUEUE_PAGESIZE {
                page += 1;
                offset = 0;
                zeroed += 1;
            }
        }
        *head = (page, offset);
        zeroed
    }

    /// A transaction from `pid` committed these (channel, payload) notifications.
    pub fn notify(&self, pid: i32, notes: &[(String, String)]) {
        if notes.is_empty() {
            return;
        }
        let listeners: Vec<Arc<Mutex<Listener>>> =
            self.listeners.lock().values().cloned().collect();
        for l in listeners {
            let mut l = l.lock();
            let mine: Vec<Notification> = notes
                .iter()
                .filter(|(c, _)| l.channels.contains(c))
                .map(|(c, p)| Notification {
                    pid,
                    channel: c.clone(),
                    payload: p.clone(),
                })
                .collect();
            if mine.is_empty() {
                continue;
            }
            match (&l.sink, l.idle) {
                (Some(sink), true) => {
                    let _ = sink(&mine);
                }
                _ => l.pending.extend(mine),
            }
        }
    }

    /// pg_notification_queue_usage: the share of the queue (max_notify_queue_pages of
    /// 8 kB) the notifications not yet read by the slowest listener take, in whole pages
    /// beyond the one the queue's tail is on (AsyncQueueEntry: a 16-byte header, the
    /// channel and payload with their terminators, 4-aligned).
    pub fn queue_usage(&self) -> f64 {
        let listeners: Vec<Arc<Mutex<Listener>>> =
            self.listeners.lock().values().cloned().collect();
        let bytes = listeners
            .iter()
            .map(|l| {
                l.lock()
                    .pending
                    .iter()
                    .map(|n| entry_len(&n.channel, &n.payload))
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(0);
        (bytes / 8192) as f64 / 1_048_576.0
    }

    /// Notifications waiting for `pid` (the session sends them itself).
    pub fn take(&self, pid: i32) -> Vec<Notification> {
        self.get(pid)
            .map(|l| l.lock().pending.drain(..).collect())
            .unwrap_or_default()
    }

    /// `pid` now waits for its client outside a transaction block: send what's pending and
    /// deliver further notifications directly until `busy`.
    pub fn idle(&self, pid: i32) {
        let Some(l) = self.get(pid) else { return };
        let mut l = l.lock();
        if !l.pending.is_empty()
            && let Some(sink) = l.sink.clone()
        {
            let notes: Vec<Notification> = l.pending.drain(..).collect();
            let _ = sink(&notes);
        }
        l.idle = true;
    }

    /// `pid` is handling a message: hold notifications for it.
    pub fn busy(&self, pid: i32) {
        if let Some(l) = self.get(pid) {
            l.lock().idle = false;
        }
    }
}

/// Check a NOTIFY's channel and payload as Postgres does.
pub fn check(channel: &str, payload: &str) -> crate::error::Result<()> {
    use crate::error::{Error, code};
    if channel.is_empty() {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "channel name cannot be empty",
        ));
    }
    if channel.len() >= 64 {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "channel name too long",
        ));
    }
    if payload.len() >= 8000 {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "payload string too long",
        ));
    }
    Ok(())
}
