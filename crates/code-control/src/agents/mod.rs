//! roll code agents: the relay (docs/code/agents.md). Daemons (`roll agent up`) connect out over
//! one WebSocket each (`link`), and their records are mirrored into control.db (`store`) under an
//! owner-wide cursor, so browsers load a snapshot and stream what changes (`api`). Commands go
//! to the daemon over its link and wait for its reply. The relay never decides anything about a
//! thread: the daemon owns the truth.
//!
//! Everything live (links, the per-owner event channels, commands waiting for a reply) is on the
//! node that serves the owner: in a cluster every `/v1/code/agents/{owner}/…` request and every
//! link of the owner's machines is routed by the lease `agents:<owner id>` (docs/code/cluster.md).

pub mod api;
pub mod automations;
pub mod linear;
pub mod docs;
mod code;
mod link;
mod policy;
mod pull;
pub mod sandbox;
pub mod share;
pub mod store;

pub use code::ApiRouter;
pub(crate) use code::{GRANT_PREFIX, grant};

use crate::Code;
use agent_proto::{CodeGrant, Command, ErrorCode, Machine, MachineId, RelayFrame, Reply, ReplyError, StreamEvent};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, broadcast, mpsc, oneshot};

pub use store::SCHEMA;

/// How long a command waits for the daemon's reply.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Commands waiting to go out on one link.
const LINK_QUEUE: usize = 1024;
/// Events an owner's channel buffers for slow streams (past it they catch up from the table).
const OWNER_EVENTS: usize = 4096;

/// The relay's live state on this node.
#[derive(Default)]
pub struct Agents {
    links: Mutex<HashMap<String, LinkHandle>>,
    owners: Mutex<HashMap<String, Arc<OwnerHub>>>,
    /// Commands sent and waiting for their reply, by (machine, command id).
    pending: Mutex<HashMap<(String, String), Pending>>,
    next_conn: AtomicU64,
    /// How long a command waits for its reply, in ms (0: [`COMMAND_TIMEOUT`]).
    timeout_ms: AtomicU64,
    /// The task that follows published changes onto their threads runs (`code.rs`).
    following: std::sync::atomic::AtomicBool,
    /// How often it looks, in ms (0: [`code::FOLLOW_EVERY`]).
    follow_ms: AtomicU64,
    /// When each thread whose claim is kept alive was last seen working, by (machine, thread):
    /// this node's clock, never the daemon's.
    working: Mutex<HashMap<(String, String), std::time::Instant>>,
    /// How long an idle thread's claim is still kept alive, in ms (0: [`code::WORK_IDLE`]).
    work_idle_ms: AtomicU64,
    /// The mirror's quota, when not [`store::QUOTA`] (tests).
    quota: Mutex<Option<store::Quota>>,
    /// When each machine last did something the relay saw go by (a command for it, terminal
    /// output from it), in ms by machine: the sandbox reconciler folds it into the durable
    /// `last_active_at` at each look.
    touched: Mutex<HashMap<String, i64>>,
    /// Woken whenever a link attaches (a woken sandbox's command waits for its daemon).
    linked: Notify,
    /// The task that asks roll code projects to pull runs (`pull.rs`).
    pulling: std::sync::atomic::AtomicBool,
    /// How often it looks, in ms (0: [`pull::PULL_EVERY`]).
    pull_ms: AtomicU64,
    /// The head each project was last asked to pull to, and when, by (machine, project).
    pulls: Mutex<HashMap<(String, String), (String, std::time::Instant)>>,
}

struct LinkHandle {
    conn: u64,
    owner_id: String,
    tx: mpsc::Sender<RelayFrame>,
    close: Arc<Closer>,
}

/// Why a link was closed from outside.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kick {
    /// A newer link of the same machine took over.
    Replaced,
    /// The machine was revoked: its token no longer works.
    Revoked,
    /// The owner's agents moved to another node (cluster): reconnect.
    Moved,
}

/// Closes a link from outside, saying why.
#[derive(Default)]
pub(crate) struct Closer {
    notify: Notify,
    reason: Mutex<Option<Kick>>,
}

impl Closer {
    fn kick(&self, why: Kick) {
        self.reason.lock().get_or_insert(why);
        self.notify.notify_one();
    }

    pub(crate) async fn kicked(&self) -> Kick {
        self.notify.notified().await;
        self.reason()
    }

    pub(crate) fn reason(&self) -> Kick {
        self.reason.lock().unwrap_or(Kick::Moved)
    }
}

/// One owner's live events, and the lock that keeps their cursors in order: a change is written
/// and published under it, so streams see cursors ascending.
pub(crate) struct OwnerHub {
    pub(crate) events: broadcast::Sender<Arc<StreamEvent>>,
    pub(crate) order: tokio::sync::Mutex<()>,
}

struct Pending {
    frame: RelayFrame,
    waiters: Vec<oneshot::Sender<Reply>>,
}

/// A link as its task sees it.
pub(crate) struct Attached {
    pub(crate) conn: u64,
    pub(crate) rx: mpsc::Receiver<RelayFrame>,
    pub(crate) close: Arc<Closer>,
}

impl Agents {
    pub(crate) fn hub(&self, owner_id: &str) -> Arc<OwnerHub> {
        let mut owners = self.owners.lock();
        if owners.len() > 1024 {
            // Owners nobody listens to and no machine of theirs is connected to.
            let links = self.links.lock();
            owners.retain(|o, h| h.events.receiver_count() > 0 || Arc::strong_count(h) > 1 || links.values().any(|l| &l.owner_id == o));
        }
        owners
            .entry(owner_id.to_string())
            .or_insert_with(|| Arc::new(OwnerHub { events: broadcast::channel(OWNER_EVENTS).0, order: tokio::sync::Mutex::new(()) }))
            .clone()
    }

    pub fn online(&self, machine_id: &str) -> bool {
        self.links.lock().contains_key(machine_id)
    }

    /// The machine's link, if it has one: a number no later link has.
    pub(crate) fn link_conn(&self, machine_id: &str) -> Option<u64> {
        self.links.lock().get(machine_id).map(|l| l.conn)
    }

    /// Wait until the machine has a link other than `stale`, until `deadline`: whether it has.
    pub(crate) async fn linked(&self, machine_id: &str, stale: Option<u64>, deadline: tokio::time::Instant) -> bool {
        loop {
            let attached = self.linked.notified();
            tokio::pin!(attached);
            attached.as_mut().enable();
            if self.link_conn(machine_id).is_some_and(|c| Some(c) != stale) {
                return true;
            }
            if tokio::time::timeout_at(deadline, attached).await.is_err() {
                return false;
            }
        }
    }

    /// The machine did something just now (the relay saw it).
    pub(crate) fn touch(&self, machine_id: &str) {
        let now = code_sql::now_ms();
        let mut touched = self.touched.lock();
        if touched.len() > 4096 {
            // Older than any idle window anyone sets: already folded into `last_active_at`.
            touched.retain(|_, t| now - *t < 24 * 3_600_000);
        }
        touched.insert(machine_id.to_string(), now);
    }

    /// When the machine last did something the relay saw on this node, if it did.
    pub(crate) fn touched(&self, machine_id: &str) -> Option<i64> {
        self.touched.lock().get(machine_id).copied()
    }

    /// How long commands wait for a daemon's reply (tests shorten it).
    pub fn set_command_timeout(&self, d: Duration) {
        self.timeout_ms.store(d.as_millis().max(1) as u64, Ordering::Relaxed);
    }

    pub(crate) fn command_timeout(&self) -> Duration {
        match self.timeout_ms.load(Ordering::Relaxed) {
            0 => COMMAND_TIMEOUT,
            ms => Duration::from_millis(ms),
        }
    }

    /// How often published changes are looked at again (tests shorten it).
    pub fn set_follow_every(&self, d: Duration) {
        self.follow_ms.store(d.as_millis().max(1) as u64, Ordering::Relaxed);
    }

    pub(crate) fn follow_every(&self) -> Duration {
        match self.follow_ms.load(Ordering::Relaxed) {
            0 => code::FOLLOW_EVERY,
            ms => Duration::from_millis(ms),
        }
    }

    /// How often roll code projects with auto-pull are looked at when nothing lands (tests
    /// shorten it).
    pub fn set_pull_every(&self, d: Duration) {
        self.pull_ms.store(d.as_millis().max(1) as u64, Ordering::Relaxed);
    }

    pub(crate) fn pull_every(&self) -> Duration {
        match self.pull_ms.load(Ordering::Relaxed) {
            0 => pull::PULL_EVERY,
            ms => Duration::from_millis(ms),
        }
    }

    /// How long a thread may sit idle and still have its work item's claim kept alive (tests
    /// shorten it).
    pub fn set_work_idle(&self, d: Duration) {
        self.work_idle_ms.store(d.as_millis().max(1) as u64, Ordering::Relaxed);
    }

    pub(crate) fn work_idle(&self) -> Duration {
        match self.work_idle_ms.load(Ordering::Relaxed) {
            0 => code::WORK_IDLE,
            ms => Duration::from_millis(ms),
        }
    }

    /// How much of each machine's records the relay keeps (tests shrink it).
    pub fn set_quota(&self, q: store::Quota) {
        *self.quota.lock() = Some(q);
    }

    pub(crate) fn quota(&self) -> store::Quota {
        self.quota.lock().unwrap_or(store::QUOTA)
    }

    /// Owners whose hub lives on this node (their streams or links are here).
    pub(crate) fn hub_owners(&self) -> Vec<String> {
        self.owners.lock().keys().cloned().collect()
    }

    /// Owners with a machine connected here (the cluster keeps their leases).
    pub fn connected_owners(&self) -> Vec<String> {
        let mut v: Vec<String> = self.links.lock().values().map(|l| l.owner_id.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Make this the machine's link; an older one closes. Commands waiting for a reply go out
    /// again on the new link (the daemon runs each id once).
    pub(crate) fn attach(&self, machine_id: &str, owner_id: &str) -> Attached {
        let conn = self.next_conn.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel(LINK_QUEUE);
        let close = Arc::new(Closer::default());
        let old = self.links.lock().insert(machine_id.to_string(), LinkHandle { conn, owner_id: owner_id.into(), tx: tx.clone(), close: close.clone() });
        if let Some(old) = old {
            old.close.kick(Kick::Replaced);
        }
        for ((m, _), p) in self.pending.lock().iter() {
            if m == machine_id {
                let _ = tx.try_send(p.frame.clone());
            }
        }
        self.linked.notify_waiters();
        Attached { conn, rx, close }
    }

    /// The link `conn` ended. True if it was still the machine's link (it's offline now).
    pub(crate) fn detach(&self, machine_id: &str, conn: u64) -> bool {
        let mut links = self.links.lock();
        if links.get(machine_id).is_some_and(|l| l.conn == conn) {
            links.remove(machine_id);
            true
        } else {
            false
        }
    }

    /// Close a machine's link at once (revoked); commands waiting on it fail.
    pub(crate) fn kick(&self, machine_id: &str) {
        if let Some(l) = self.links.lock().remove(machine_id) {
            l.close.kick(Kick::Revoked);
        }
        let gone: Vec<Pending> = {
            let mut pending = self.pending.lock();
            let keys: Vec<_> = pending.keys().filter(|(m, _)| m == machine_id).cloned().collect();
            keys.into_iter().filter_map(|k| pending.remove(&k)).collect()
        };
        for p in gone {
            for w in p.waiters {
                let _ = w.send(error(ErrorCode::MachineOffline, "the machine was removed"));
            }
        }
    }

    /// Close every link of an owner (its lease moved to another node; daemons reconnect there).
    pub fn kick_owner(&self, owner_id: &str) {
        self.links.lock().retain(|_, l| {
            if l.owner_id == owner_id {
                l.close.kick(Kick::Moved);
                false
            } else {
                true
            }
        });
    }

    /// A daemon's reply: to everyone waiting for that command.
    pub(crate) fn replied(&self, machine_id: &str, id: &str, reply: Reply) {
        if let Some(p) = self.pending.lock().remove(&(machine_id.to_string(), id.to_string())) {
            for w in p.waiters {
                let _ = w.send(reply.clone());
            }
        }
    }

    /// Send a command to a connected machine and wait for its reply. A second request from the
    /// same sender with the same id while the first waits shares its reply rather than sending
    /// it again (another sender's is a conflict); a link that
    /// drops and comes back within the wait gets the command again (the daemon runs an id once).
    pub(crate) async fn command(&self, machine_id: &str, id: String, by: String, command: Command, grant: Option<CodeGrant>, timeout: Duration) -> Reply {
        let tx = match self.links.lock().get(machine_id) {
            Some(l) => l.tx.clone(),
            None => return error(ErrorCode::MachineOffline, "the machine isn't connected"),
        };
        let (wtx, wrx) = oneshot::channel();
        let key = (machine_id.to_string(), id.clone());
        let send = {
            let mut pending = self.pending.lock();
            match pending.get_mut(&key) {
                // Ids are per sender (the daemon keeps a receipt per sender and id), but a reply
                // names only its id: one sender's id at a time per machine.
                Some(Pending { frame: RelayFrame::Command { by: first, .. }, .. }) if *first != by => {
                    return error(ErrorCode::Conflict, "another sender's command with this id is waiting for its reply; use another id");
                }
                Some(p) => {
                    p.waiters.push(wtx);
                    None
                }
                None => {
                    let frame = RelayFrame::Command { id, by, command: Box::new(command), grant: grant.map(Box::new) };
                    pending.insert(key.clone(), Pending { frame: frame.clone(), waiters: vec![wtx] });
                    Some(frame)
                }
            }
        };
        if let Some(frame) = send
            && tx.try_send(frame).is_err()
        {
            self.pending.lock().remove(&key);
            return error(ErrorCode::MachineOffline, "the machine's link is closed or too busy");
        }
        match tokio::time::timeout(timeout, wrx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => error(ErrorCode::MachineOffline, "the machine went away"),
            Err(_) => {
                let mut pending = self.pending.lock();
                if let Some(p) = pending.get_mut(&key) {
                    p.waiters.retain(|w| !w.is_closed());
                    if p.waiters.is_empty() {
                        pending.remove(&key);
                    }
                }
                error(ErrorCode::Timeout, &format!("the machine didn't answer within {} s", timeout.as_secs()))
            }
        }
    }
}

pub(crate) fn error(code: ErrorCode, message: &str) -> Reply {
    Reply::Error(ReplyError { code, message: message.into() })
}

impl Code {
    /// A machine as browsers see it.
    pub(crate) fn machine_json(&self, m: &store::MachineRow) -> Machine {
        Machine {
            id: MachineId(m.id.clone()),
            name: m.name.clone(),
            online: self.agents.online(&m.id),
            info: m.info.clone(),
            sandbox: m.sandbox.clone(),
            appearance: m.appearance,
            created_at: m.created_at.max(0) as u64,
            last_seen: m.last_seen.map(|t| t.max(0) as u64),
        }
    }

    /// Publish a machine's change. Callers hold the owner's order lock across the write that
    /// took its cursor and this.
    pub(crate) fn publish_machine(&self, m: &store::MachineRow) {
        let ev = StreamEvent::Machine { cursor: m.cursor, machine: self.machine_json(m) };
        let _ = self.agents.hub(&m.owner_id).events.send(Arc::new(ev));
    }
}
