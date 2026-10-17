//! Per-connection state the engine needs to execute commands.

use crate::acl::User;
use crate::hub::Sink;
use bytes::Bytes;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};

#[derive(Clone, Debug, Default)]
pub struct Multi {
    pub queued: Vec<Vec<Bytes>>,
    /// A command failed to queue (unknown, bad arity, OOM…): EXEC aborts.
    pub error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReplyMode {
    #[default]
    On,
    Off,
    /// Skip the reply of the next command only.
    Skip,
}

/// Live counters of a connection, shared by its task (which updates them as it reads and
/// writes) and the engine (CLIENT LIST, CLIENT KILL, INFO).
#[derive(Debug, Default)]
pub struct ConnStats {
    /// The socket's file descriptor (-1 for connections without one).
    pub fd: AtomicI64,
    pub net_in: AtomicU64,
    pub net_out: AtomicU64,
    /// Unparsed input buffered, and the buffer's capacity.
    pub qbuf: AtomicU64,
    pub qbuf_cap: AtomicU64,
    /// Parsed commands waiting to run (and the one running), as their protocol bytes: still
    /// part of Redis' query buffer.
    pub pending_input: AtomicU64,
    /// Memory last measured by the cron, which is how Redis tracks clients that can't be
    /// evicted (CLIENT NO-EVICT).
    pub cron_mem: AtomicU64,
    /// Output waiting to be written, measured like Redis' `omem` (reply blocks allocated past
    /// the static buffer), and the static buffer's fill (`obl`) and block count (`oll`).
    pub obuf: AtomicU64,
    pub obl: AtomicU64,
    pub oll: AtomicU64,
    /// The static reply buffer's size (`rbs`, 0: not resized yet, 16KB), its peak use since the
    /// last reset (`rbp`) and when that was (Unix ms): the cron resizes it like Redis'
    /// `clientsCronResizeOutputBuffer`.
    pub rbs: AtomicU64,
    pub rbp: AtomicU64,
    pub rbp_reset_ms: AtomicI64,
    /// Commands processed (CLIENT LIST `tot-cmds`).
    pub cmds: AtomicU64,
    pub read_events: AtomicU64,
    /// Commands parsed per read event, summed, and the number of read events with commands.
    pub pipeline_sum: AtomicU64,
    pub pipeline_cnt: AtomicU64,
    /// Unix seconds of the last command or read.
    pub last_interaction: AtomicI64,
    /// Unix ms of the last command (INFO `active_clients`).
    pub last_cmd_ms: AtomicI64,
    /// The connection is a replica's replication link (CLIENT LIST flag `S`).
    pub replica: AtomicBool,
    /// CLIENT KILL: close the connection (after the current reply).
    pub killed: AtomicBool,
    /// Waiting for CLIENT PAUSE to end (shown blocked).
    pub postponed: AtomicBool,
}

#[derive(Clone)]
pub struct Client {
    pub id: u64,
    pub db: usize,
    pub resp: u8,
    pub name: Option<Bytes>,
    pub lib_name: Option<Arc<str>>,
    pub lib_ver: Option<Arc<str>>,
    pub user: Arc<User>,
    pub authenticated: bool,
    pub sink: Option<Arc<dyn Sink>>,
    pub multi: Option<Multi>,
    pub subs: HashSet<Bytes>,
    pub psubs: HashSet<Bytes>,
    pub ssubs: HashSet<Bytes>,
    pub no_touch: bool,
    pub no_evict: bool,
    /// `READONLY` (cluster replicas).
    pub readonly: bool,
    pub reply_mode: ReplyMode,
    pub monitor: bool,
    /// Address strings for CLIENT INFO / MONITOR.
    pub addr: String,
    pub laddr: String,
    pub created_ms: i64,
    /// Full name of the last command (`client|list`), as CLIENT LIST shows it.
    pub last_cmd: &'static str,
    /// The connection is a replication link or the log replayer (bypasses limits).
    pub internal: bool,
    /// The connection's live counters, when it is a registered network client.
    pub conn: Option<Arc<ConnStats>>,
    /// Pushes for this client produced after its reply (its own tracking invalidations) are
    /// collected with [`crate::Engine::take_after_reply`] instead of going through the sink.
    pub defer_pushes: bool,
    /// While EXEC runs: the transaction's command count and argument bytes (Redis keeps the
    /// MULTI state until EXEC returns, so CLIENT LIST inside it still shows `x`).
    pub executing: Option<(i64, usize)>,
    /// Reply bytes the connection can take before its output buffer limit closes it
    /// (`u64::MAX`: no limit). Commands with unbounded replies (KEYS, *RANDMEMBER with a
    /// negative count) stop producing once past it, like Redis stops on `CLIENT_CLOSE_ASAP`.
    pub reply_budget: u64,
    /// `HIMPORT PREPARE` fieldsets.
    pub himport: Option<Box<crate::types::template::Fieldsets>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client").field("id", &self.id).field("db", &self.db).field("resp", &self.resp).finish()
    }
}

impl Client {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            db: 0,
            resp: 2,
            name: None,
            lib_name: None,
            lib_ver: None,
            user: User::default_user(),
            authenticated: true,
            sink: None,
            multi: None,
            subs: HashSet::new(),
            psubs: HashSet::new(),
            ssubs: HashSet::new(),
            no_touch: false,
            no_evict: false,
            readonly: false,
            reply_mode: ReplyMode::On,
            monitor: false,
            addr: "127.0.0.1:0".into(),
            laddr: "127.0.0.1:6379".into(),
            created_ms: crate::util::now_ms(),
            last_cmd: "NULL",
            internal: false,
            conn: None,
            defer_pushes: false,
            executing: None,
            reply_budget: u64::MAX,
            himport: None,
        }
    }

    pub fn with_sink(id: u64, sink: Arc<dyn Sink>) -> Self {
        Self { sink: Some(sink), ..Self::new(id) }
    }

    /// Subscribed to anything (RESP2 clients are then restricted to pub/sub commands).
    pub fn subscriptions(&self) -> usize {
        self.subs.len() + self.psubs.len() + self.ssubs.len()
    }

    pub fn in_multi(&self) -> bool {
        self.multi.is_some()
    }
}

/// Size of Redis' static reply buffer (`PROTO_REPLY_CHUNK_BYTES`).
pub const REPLY_CHUNK: usize = 16 * 1024;
/// `sizeof(clientReplyBlock)`.
const BLOCK_HEADER: usize = 24;

/// jemalloc's size class for an allocation of `n` bytes (four classes per doubling).
pub fn jemalloc_size(n: usize) -> usize {
    if n <= 8 {
        return 8;
    }
    let lg = usize::BITS - (n - 1).leading_zeros() - 1;
    let step = (1usize << lg.saturating_sub(2)).max(16);
    n.div_ceil(step) * step
}

/// A connection's output laid out like Redis' (a 16KB static buffer, then a list of reply
/// blocks of at least 16KB sized by the allocator), to measure it the way
/// `client-output-buffer-limit` and CLIENT LIST `omem` do. Blocks go away as the socket
/// takes them.
#[derive(Debug, Default)]
pub struct ReplyBuf {
    /// The static buffer's size when last appended to.
    cap: usize,
    bufpos: usize,
    buf_sent: usize,
    /// (size, used, sent) per block.
    blocks: std::collections::VecDeque<(usize, usize, usize)>,
    mem: u64,
}

impl ReplyBuf {
    /// Queue `n` bytes; `stats` has the static buffer's current size and records its peak.
    pub fn append(&mut self, mut n: usize, stats: &ConnStats) {
        use std::sync::atomic::Ordering::Relaxed;
        self.cap = stats.reply_buffer_size() as usize;
        if self.blocks.is_empty() {
            let take = n.min(self.cap.saturating_sub(self.bufpos));
            self.bufpos += take;
            n -= take;
            stats.rbp.fetch_max(self.bufpos as u64, Relaxed);
        }
        if let Some((size, used, _)) = self.blocks.back_mut() {
            let take = n.min(*size - *used);
            *used += take;
            n -= take;
        }
        if n > 0 {
            let size = jemalloc_size(n.max(REPLY_CHUNK) + BLOCK_HEADER) - BLOCK_HEADER;
            self.blocks.push_back((size, n, 0));
            self.mem += size as u64;
        }
    }

    /// The socket took `n` bytes.
    pub fn sent(&mut self, mut n: usize) {
        let take = n.min(self.bufpos - self.buf_sent);
        self.buf_sent += take;
        n -= take;
        if self.buf_sent == self.bufpos {
            self.bufpos = 0;
            self.buf_sent = 0;
        }
        while n > 0 {
            let Some((size, used, sent)) = self.blocks.front_mut() else { break };
            let take = n.min(*used - *sent);
            *sent += take;
            n -= take;
            if *sent == *used {
                self.mem -= *size as u64;
                self.blocks.pop_front();
            }
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Room left where the next append goes (static buffer, or the last block).
    pub fn free(&self) -> usize {
        match self.blocks.back() {
            Some((size, used, _)) => size - used,
            None => self.cap.saturating_sub(self.bufpos),
        }
    }

    /// Reply block bytes (`omem`): 0 while everything fits the static buffer.
    pub fn mem(&self) -> u64 {
        self.mem
    }

    pub fn publish(&self, stats: &ConnStats) {
        use std::sync::atomic::Ordering::Relaxed;
        stats.obuf.store(self.mem, Relaxed);
        stats.obl.store(self.bufpos as u64, Relaxed);
        stats.oll.store(self.blocks.len() as u64, Relaxed);
    }
}

impl ConnStats {
    /// The static reply buffer's size (`rbs`).
    pub fn reply_buffer_size(&self) -> u64 {
        match self.rbs.load(std::sync::atomic::Ordering::Relaxed) {
            0 => REPLY_CHUNK as u64,
            n => n,
        }
    }

    /// `clientsCronResizeOutputBuffer`: shrink the static reply buffer to its peak use when that
    /// stayed under half of it, double it when the peak filled it (up to 16KB), and reset the
    /// peak every 5 seconds. Returns (shrunk, expanded).
    pub fn resize_reply_buffer(&self, now_ms: i64) -> (bool, bool) {
        use std::sync::atomic::Ordering::Relaxed;
        const MIN: u64 = 1024;
        const PEAK_RESET_MS: i64 = 5000;
        let fresh = self.rbs.load(Relaxed) == 0;
        let size = self.reply_buffer_size();
        // A new client's peak is its whole buffer.
        let peak = if fresh { size } else { self.rbp.load(Relaxed) };
        let mut new = 0;
        let (mut shrunk, mut expanded) = (false, false);
        if size / 2 >= MIN && peak < size / 2 {
            new = MIN.max(peak + 1);
            shrunk = true;
        } else if size * 2 < 2 * REPLY_CHUNK as u64 && peak == size {
            new = (REPLY_CHUNK as u64).min(size * 2);
            expanded = true;
        }
        if now_ms - self.rbp_reset_ms.load(Relaxed) >= PEAK_RESET_MS {
            self.rbp.store(self.obl.load(Relaxed), Relaxed);
            self.rbp_reset_ms.store(now_ms, Relaxed);
        }
        if new > 0 {
            self.rbs.store(jemalloc_size(new as usize) as u64, Relaxed);
        } else if fresh {
            self.rbs.store(size, Relaxed);
        }
        (shrunk, expanded)
    }
}

/// Output buffer limit classes, as indexed in `client-output-buffer-limit`.
pub const OBUF_NORMAL: usize = 0;
pub const OBUF_REPLICA: usize = 1;
pub const OBUF_PUBSUB: usize = 2;

/// `checkClientOutputBufferLimits`: whether `mem` bytes of reply blocks break the class's hard
/// limit, or its soft limit for longer than its seconds (`soft_since` tracks when the soft limit
/// was first seen, in Unix seconds). Clients that still have to authenticate get 1024 bytes.
pub fn obuf_limit_reached(mem: u64, limit: &crate::config::ObufLimit, unauthenticated: bool, soft_since: &mut i64, now_s: i64) -> bool {
    if mem > 1024 && unauthenticated {
        return true;
    }
    let hard = limit.hard != 0 && mem >= limit.hard;
    let mut soft = limit.soft != 0 && mem >= limit.soft;
    if soft {
        if *soft_since == 0 {
            *soft_since = now_s;
            soft = false;
        } else if now_s - *soft_since <= limit.soft_seconds {
            soft = false;
        }
    } else {
        *soft_since = 0;
    }
    hard || soft
}

#[cfg(test)]
mod reply_buf_tests {
    use super::*;

    #[test]
    fn size_classes() {
        assert_eq!(jemalloc_size(16384 + 24), 20480);
        assert_eq!(jemalloc_size(183_625 + 24), 196_608);
        assert_eq!(jemalloc_size(16384), 16384);
        assert_eq!(jemalloc_size(17), 32);
        assert_eq!(jemalloc_size(100), 112);
    }

    #[test]
    fn blocks_fill_then_drain() {
        let mut b = ReplyBuf::default();
        let s = ConnStats::default();
        b.append(10_000, &s);
        assert_eq!(b.mem(), 0);
        b.append(10_000, &s);
        assert_eq!(b.mem(), 20456);
        b.append(200_000, &s);
        assert_eq!(b.mem(), 20456 + jemalloc_size(200_000 - (20456 - 3616) + 24) as u64 - 24);
        b.sent(16384);
        assert_eq!(b.blocks.len(), 2);
        b.sent(20456);
        assert_eq!(b.blocks.len(), 1);
        b.sent(1 << 30);
        assert_eq!(b.mem(), 0);
    }

    #[test]
    fn reply_buffer_shrinks_when_idle_and_grows_back() {
        let s = ConnStats::default();
        // First pass: a new client's peak is its whole buffer; the peak resets.
        assert_eq!(s.resize_reply_buffer(10_000), (false, false));
        assert_eq!((s.reply_buffer_size(), s.rbp.load(std::sync::atomic::Ordering::Relaxed)), (16384, 0));
        assert_eq!(s.resize_reply_buffer(10_100), (true, false));
        assert_eq!(s.reply_buffer_size(), 1024);
        // A reply filling it doubles it on the next pass.
        let mut b = ReplyBuf::default();
        b.append(3000, &s);
        assert_eq!(b.mem(), jemalloc_size(16384 + 24) as u64 - 24);
        assert_eq!(s.resize_reply_buffer(10_200), (false, true));
        assert_eq!(s.reply_buffer_size(), 2048);
    }
}
