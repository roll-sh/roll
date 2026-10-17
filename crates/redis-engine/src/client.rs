//! Per-connection state the engine needs to execute commands.

use crate::acl::User;
use crate::hub::Sink;
use bytes::Bytes;
use std::collections::HashSet;
use std::sync::Arc;

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

/// Server-assisted client-side caching state (`CLIENT TRACKING`).
#[derive(Clone, Debug, Default)]
pub struct Tracking {
    pub on: bool,
    pub bcast: bool,
    pub optin: bool,
    pub optout: bool,
    pub noloop: bool,
    pub prefixes: Vec<Bytes>,
    /// Client id that receives invalidations instead (RESP2 redirect).
    pub redirect: Option<u64>,
    /// `CLIENT CACHING yes/no` for the next command.
    pub caching: Option<bool>,
}

#[derive(Clone)]
pub struct Client {
    pub id: u64,
    pub db: usize,
    pub resp: u8,
    pub name: Option<Bytes>,
    pub lib_name: Option<String>,
    pub lib_ver: Option<String>,
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
    pub tracking: Tracking,
    pub monitor: bool,
    /// Address strings for CLIENT INFO / MONITOR.
    pub addr: String,
    pub laddr: String,
    pub created_ms: i64,
    pub last_cmd: &'static str,
    /// The connection is a replication link or the log replayer (bypasses limits).
    pub internal: bool,
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
            tracking: Tracking::default(),
            monitor: false,
            addr: "127.0.0.1:0".into(),
            laddr: "127.0.0.1:6379".into(),
            created_ms: crate::util::now_ms(),
            last_cmd: "NULL",
            internal: false,
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
