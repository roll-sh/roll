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
    /// Output waiting to be written.
    pub obuf: AtomicU64,
    /// Commands processed (CLIENT LIST `tot-cmds`).
    pub cmds: AtomicU64,
    pub read_events: AtomicU64,
    /// Commands parsed per read event, summed, and the number of read events with commands.
    pub pipeline_sum: AtomicU64,
    pub pipeline_cnt: AtomicU64,
    /// Unix seconds of the last command or read.
    pub last_interaction: AtomicI64,
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
