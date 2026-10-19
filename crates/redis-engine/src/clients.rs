//! The clients of a database, as the engine sees them: a registry of connections (CLIENT LIST,
//! KILL, UNBLOCK, INFO clients) refreshed after each command, CLIENT PAUSE, and server-assisted
//! client-side caching (CLIENT TRACKING): the table of keys clients read, BCAST prefixes and
//! the invalidation messages (Redis' tracking.c).

use crate::acl::User;
use crate::client::{Client, ConnStats};
use crate::hub::Sink;
use crate::value::Str;
use bytes::Bytes;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// What CLIENT LIST shows about a client, copied from its [`Client`] after each command.
#[derive(Clone)]
pub struct View {
    pub name: Option<Bytes>,
    pub db: usize,
    pub resp: u8,
    pub user: Arc<User>,
    pub lib_name: Option<Arc<str>>,
    pub lib_ver: Option<Arc<str>>,
    pub sub: usize,
    pub psub: usize,
    pub ssub: usize,
    /// Queued commands in MULTI, -1 outside.
    pub multi: i64,
    pub multi_mem: usize,
    /// Memory of the HIMPORT fieldsets.
    pub himport_mem: usize,
    pub readonly: bool,
    pub no_evict: bool,
    pub no_touch: bool,
    pub monitor: bool,
    pub last_cmd: &'static str,
    pub addr: String,
    pub laddr: String,
    pub created_ms: i64,
}

/// Queued commands and their argument bytes (-1 outside MULTI).
fn multi_of(c: &Client) -> (i64, usize) {
    match (&c.multi, c.executing) {
        (Some(m), _) => (m.queued.len() as i64, m.queued.iter().flatten().map(|a| a.len()).sum()),
        (None, Some(e)) => e,
        (None, None) => (-1, 0),
    }
}

impl View {
    pub fn of(c: &Client) -> Self {
        Self {
            name: c.name.clone(),
            db: c.db,
            resp: c.resp,
            user: c.user.clone(),
            lib_name: c.lib_name.clone(),
            lib_ver: c.lib_ver.clone(),
            sub: c.subs.len(),
            psub: c.psubs.len(),
            ssub: c.ssubs.len(),
            multi: multi_of(c).0,
            multi_mem: multi_of(c).1,
            himport_mem: c.himport.as_ref().map_or(0, |f| f.mem()),
            readonly: c.readonly,
            no_evict: c.no_evict,
            no_touch: c.no_touch,
            monitor: c.monitor,
            last_cmd: c.last_cmd,
            addr: c.addr.clone(),
            laddr: c.laddr.clone(),
            created_ms: c.created_ms,
        }
    }

    /// Refresh from the client, without reallocating what didn't change.
    fn refresh(&mut self, c: &Client) {
        self.name.clone_from(&c.name);
        self.db = c.db;
        self.resp = c.resp;
        if !Arc::ptr_eq(&self.user, &c.user) {
            self.user = c.user.clone();
        }
        self.lib_name.clone_from(&c.lib_name);
        self.lib_ver.clone_from(&c.lib_ver);
        self.sub = c.subs.len();
        self.psub = c.psubs.len();
        self.ssub = c.ssubs.len();
        (self.multi, self.multi_mem) = multi_of(c);
        self.himport_mem = c.himport.as_ref().map_or(0, |f| f.mem());
        self.readonly = c.readonly;
        self.no_evict = c.no_evict;
        self.no_touch = c.no_touch;
        self.monitor = c.monitor;
        self.last_cmd = c.last_cmd;
    }

    pub fn subscriptions(&self) -> usize {
        self.sub + self.psub + self.ssub
    }
}

/// CLIENT TRACKING state of one client.
#[derive(Clone, Debug, Default)]
pub struct Track {
    pub bcast: bool,
    pub optin: bool,
    pub optout: bool,
    pub noloop: bool,
    /// CLIENT CACHING given for the next command.
    pub caching: bool,
    /// Client id receiving the invalidations (0: none).
    pub redirect: u64,
    /// The redirect client is gone.
    pub broken: bool,
    pub prefixes: BTreeSet<Bytes>,
}

pub struct Entry {
    pub stats: Arc<ConnStats>,
    pub sink: Option<Arc<dyn Sink>>,
    pub view: View,
    pub track: Option<Track>,
}

/// Keys modified in this cycle that match a BCAST prefix, with the client that modified each
/// (for NOLOOP), and the clients subscribed to the prefix.
#[derive(Default)]
struct Bcast {
    keys: BTreeMap<Str, u64>,
    clients: BTreeSet<u64>,
}

#[derive(Default)]
pub struct Clients {
    pub by_id: BTreeMap<u64, Entry>,
    /// Tracked keys → clients that read them (Redis' TrackingTable; databases share it).
    table: HashMap<Str, HashSet<u64>>,
    /// Client ids stored in the table, summed over keys.
    items: u64,
    prefixes: BTreeMap<Bytes, Bcast>,
    pub tracking_clients: usize,
    /// The client running the current top-level command (0: none, e.g. the cron).
    pub current: u64,
    /// Invalidations for the current client, sent after its reply (`None`: a flush).
    pending: Vec<Option<Str>>,
    /// Pushes for clients that collect them after the reply ([`Client::defer_pushes`]).
    after_reply: Vec<Bytes>,
    /// The current client collects its pushes after the reply.
    pub defer_current: bool,
    limit_misses: u32,
    /// CLIENT PAUSE: until (Unix ms), and whether all commands or only writes are paused.
    pub pause: Option<(i64, bool)>,
    /// FAILOVER's write pause (`PAUSE_DURING_FAILOVER`): until the failover ends, apart from
    /// CLIENT PAUSE (CLIENT UNPAUSE doesn't lift it).
    pub failover_pause: bool,
    /// Where the clients cron continues (client id).
    pub cron_cursor: u64,
}

const TRACKING_CHANNEL: &[u8] = b"__redis__:invalidate";

fn bulk(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(format!("${}\r\n", b.len()).as_bytes());
    out.extend_from_slice(b);
    out.extend_from_slice(b"\r\n");
}

/// The `[key]` array of an invalidation for one key.
fn one_key(key: &[u8]) -> Vec<u8> {
    let mut v = b"*1\r\n".to_vec();
    bulk(&mut v, key);
    v
}

fn null(resp: u8) -> Vec<u8> {
    if resp > 2 { b"_\r\n".to_vec() } else { b"$-1\r\n".to_vec() }
}

impl Clients {
    pub fn register(&mut self, c: &Client) {
        let Some(stats) = c.conn.clone() else { return };
        self.by_id.insert(c.id, Entry { stats, sink: c.sink.clone(), view: View::of(c), track: None });
    }

    pub fn unregister(&mut self, id: u64) {
        self.disable_tracking(id);
        self.by_id.remove(&id);
    }

    /// Copy the client's state after a command.
    pub fn refresh(&mut self, c: &Client) {
        if let Some(e) = self.by_id.get_mut(&c.id) {
            e.view.refresh(c);
        }
    }

    pub fn track(&self, id: u64) -> Option<&Track> {
        // Every client with tracking state is counted (looked up after each command).
        if self.tracking_clients == 0 {
            return None;
        }
        self.by_id.get(&id).and_then(|e| e.track.as_ref())
    }

    pub fn track_mut(&mut self, id: u64) -> Option<&mut Track> {
        if self.tracking_clients == 0 {
            return None;
        }
        self.by_id.get_mut(&id).and_then(|e| e.track.as_mut())
    }

    pub fn is_tracking(&self) -> bool {
        self.tracking_clients > 0
    }

    // ---- CLIENT TRACKING ----

    /// `checkPrefixCollisionsOrReply`.
    pub fn prefix_collision(&self, id: u64, prefixes: &[Bytes]) -> Option<String> {
        let overlap = |a: &[u8], b: &[u8]| {
            let n = a.len().min(b.len());
            a[..n] == b[..n]
        };
        for (i, p) in prefixes.iter().enumerate() {
            if let Some(t) = self.track(id) {
                for q in &t.prefixes {
                    if overlap(q, p) {
                        return Some(format!(
                            "ERR Prefix '{}' overlaps with an existing prefix '{}'. Prefixes for a single client must not overlap.",
                            String::from_utf8_lossy(p),
                            String::from_utf8_lossy(q)
                        ));
                    }
                }
            }
            for q in &prefixes[i + 1..] {
                if overlap(p, q) {
                    return Some(format!(
                        "ERR Prefix '{}' overlaps with another provided prefix '{}'. Prefixes for a single client must not overlap.",
                        String::from_utf8_lossy(p),
                        String::from_utf8_lossy(q)
                    ));
                }
            }
        }
        None
    }

    /// `enableTracking`: (re)enable with the options in `opts` (its `prefixes` are the requested
    /// ones); BCAST prefixes add to the existing ones.
    pub fn enable_tracking(&mut self, id: u64, opts: Track) {
        let Some(e) = self.by_id.get_mut(&id) else { return };
        if e.track.is_none() {
            self.tracking_clients += 1;
        }
        let t = e.track.get_or_insert_with(Track::default);
        t.redirect = opts.redirect;
        t.broken = false;
        t.bcast = opts.bcast;
        t.optin = opts.optin;
        t.optout = opts.optout;
        t.noloop = opts.noloop;
        if opts.bcast {
            let list: Vec<Bytes> = if opts.prefixes.is_empty() { vec![Bytes::new()] } else { opts.prefixes.into_iter().collect() };
            for p in list {
                let bs = self.prefixes.entry(p.clone()).or_default();
                if bs.clients.insert(id) {
                    t.prefixes.insert(p);
                }
            }
        }
    }

    /// `disableTracking`.
    pub fn disable_tracking(&mut self, id: u64) {
        let Some(e) = self.by_id.get_mut(&id) else { return };
        let Some(t) = e.track.take() else { return };
        self.tracking_clients -= 1;
        for p in &t.prefixes {
            if let Some(bs) = self.prefixes.get_mut(p) {
                bs.clients.remove(&id);
                if bs.clients.is_empty() {
                    self.prefixes.remove(p);
                }
            }
        }
    }

    /// `trackingRememberKeys`: the current client read these keys.
    pub fn remember(&mut self, tracker: u64, keys: impl IntoIterator<Item = Bytes>) {
        let Some(t) = self.track(tracker) else { return };
        if t.bcast || (t.optin && !t.caching) || (t.optout && t.caching) {
            return;
        }
        for k in keys {
            if self.table.entry(Str::from_slice(&k)).or_default().insert(tracker) {
                self.items += 1;
            }
        }
    }

    /// After each command: CLIENT CACHING applies to one command (or transaction) only.
    pub fn command_done(&mut self, id: u64, in_multi: bool, was_client_cmd: bool) {
        if !in_multi
            && !was_client_cmd
            && let Some(t) = self.track_mut(id)
        {
            t.caching = false;
        }
    }

    /// Does anything need to know about modified keys?
    pub fn wants_touches(&self) -> bool {
        !self.table.is_empty() || !self.prefixes.is_empty()
    }

    /// Whether any client tracks keys (or remembers some to invalidate).
    pub fn tracks_keys(&self) -> bool {
        self.tracking_clients != 0 || !self.table.is_empty()
    }

    /// `trackingInvalidateKey`: `key` was modified by the current client (if any).
    pub fn invalidate(&mut self, key: &[u8], bcast: bool) {
        if self.tracking_clients == 0 && self.table.is_empty() {
            return;
        }
        if bcast && !self.prefixes.is_empty() {
            for (p, bs) in self.prefixes.iter_mut() {
                if key.starts_with(p) {
                    bs.keys.insert(Str::from_slice(key), self.current);
                }
            }
        }
        let Some(ids) = self.table.remove(key) else { return };
        self.items -= ids.len() as u64;
        let mut ids: Vec<u64> = ids.into_iter().collect();
        ids.sort_unstable();
        for id in ids {
            let Some(t) = self.track(id) else { continue };
            if t.bcast {
                continue;
            }
            if t.noloop && id == self.current {
                continue;
            }
            if id == self.current {
                self.pending.push(Some(Str::from_slice(key)));
            } else {
                self.send(id, &one_key(key));
            }
        }
    }

    /// `trackingInvalidateKeysOnFlush`: every tracking client gets a null invalidation.
    pub fn flushed(&mut self) {
        if self.tracking_clients > 0 {
            let ids: Vec<u64> = self.by_id.iter().filter(|(_, e)| e.track.is_some()).map(|(id, _)| *id).collect();
            for id in ids {
                if id == self.current {
                    self.pending.push(None);
                } else {
                    let resp = self.by_id[&id].view.resp;
                    self.send(id, &null(resp));
                }
            }
        }
        self.table.clear();
        self.items = 0;
    }

    /// Send the invalidations queued for the current client (after its reply).
    pub fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let id = self.current;
        for k in std::mem::take(&mut self.pending) {
            if !self.by_id.contains_key(&id) {
                break;
            }
            let value = match k {
                Some(k) => one_key(&k),
                None => null(self.by_id[&id].view.resp),
            };
            self.send(id, &value);
        }
    }

    /// Pushes to deliver after the current reply.
    pub fn take_after_reply(&mut self) -> Vec<Bytes> {
        std::mem::take(&mut self.after_reply)
    }

    /// `sendTrackingMessage`: `value` is the RESP of the keys array (or a null).
    fn send(&mut self, id: u64, value: &[u8]) {
        let Some(e) = self.by_id.get(&id) else { return };
        let Some(t) = &e.track else { return };
        let mut target = id;
        let mut redirected = false;
        if t.redirect != 0 {
            let redirect = t.redirect;
            if !self.by_id.contains_key(&redirect) {
                let resp = e.view.resp;
                if let Some(t) = self.track_mut(id) {
                    t.broken = true;
                }
                if resp > 2 {
                    let mut msg = b">2\r\n".to_vec();
                    bulk(&mut msg, b"tracking-redir-broken");
                    msg.extend_from_slice(format!(":{redirect}\r\n").as_bytes());
                    self.deliver(id, msg);
                }
                return;
            }
            target = redirect;
            redirected = true;
        }
        let te = &self.by_id[&target];
        let mut msg = Vec::with_capacity(32 + value.len());
        if te.view.resp > 2 {
            msg.extend_from_slice(b">2\r\n");
            bulk(&mut msg, b"invalidate");
        } else if redirected && te.view.subscriptions() > 0 {
            msg.extend_from_slice(b"*3\r\n");
            bulk(&mut msg, b"message");
            bulk(&mut msg, TRACKING_CHANNEL);
        } else {
            return;
        }
        msg.extend_from_slice(value);
        self.deliver(target, msg);
    }

    fn deliver(&mut self, id: u64, msg: Vec<u8>) {
        let Some(e) = self.by_id.get(&id) else { return };
        if id == self.current && self.defer_current {
            self.after_reply.push(Bytes::from(msg));
        } else if let Some(sink) = &e.sink {
            sink.push(Bytes::from(msg));
        }
    }

    /// `trackingBroadcastInvalidationMessages`: send the keys modified since the last call to
    /// the BCAST clients of each prefix (filtered by what each user may read).
    pub fn broadcast(&mut self) {
        if self.tracking_clients == 0 || self.prefixes.iter().all(|(_, b)| b.keys.is_empty()) {
            return;
        }
        let prefixes: Vec<Bytes> = self.prefixes.keys().cloned().collect();
        for p in prefixes {
            let Some(bs) = self.prefixes.get_mut(&p) else { continue };
            if bs.keys.is_empty() {
                continue;
            }
            let keys = std::mem::take(&mut bs.keys);
            let clients: Vec<u64> = bs.clients.iter().copied().collect();
            for id in clients {
                let Some(e) = self.by_id.get(&id) else { continue };
                let noloop = e.track.as_ref().is_some_and(|t| t.noloop);
                let user = e.view.user.clone();
                let mut body = Vec::new();
                let mut n = 0;
                for (k, by) in &keys {
                    if noloop && *by == id {
                        continue;
                    }
                    if !user.can_read_key(k) {
                        continue;
                    }
                    bulk(&mut body, k);
                    n += 1;
                }
                if n == 0 {
                    continue;
                }
                let mut value = format!("*{n}\r\n").into_bytes();
                value.extend_from_slice(&body);
                self.send(id, &value);
            }
        }
    }

    /// `trackingLimitUsedSlots`: keep the table under `tracking-table-max-keys`, invalidating
    /// random keys.
    pub fn limit(&mut self, max_keys: u64, rng: &mut crate::util::Rng) {
        if max_keys == 0 || self.table.len() as u64 <= max_keys {
            self.limit_misses = 0;
            return;
        }
        let mut effort = 100 * (self.limit_misses + 1);
        let current = std::mem::replace(&mut self.current, 0);
        while effort > 0 && !self.table.is_empty() {
            effort -= 1;
            let i = rng.below(self.table.len());
            let Some(k) = self.table.keys().nth(i).cloned() else { break };
            self.invalidate(&k, false);
            if self.table.len() as u64 <= max_keys {
                self.limit_misses = 0;
                self.current = current;
                return;
            }
        }
        self.current = current;
        self.limit_misses += 1;
    }

    pub fn tracking_keys(&self) -> usize {
        self.table.len()
    }

    pub fn tracking_items(&self) -> u64 {
        self.items
    }

    pub fn tracking_prefixes(&self) -> usize {
        self.prefixes.len()
    }

    // ---- CLIENT PAUSE ----

    /// Is a command with these properties paused right now?
    pub fn paused(&mut self, now: i64, may_replicate: bool) -> Option<i64> {
        let by_client = match self.pause {
            Some((end, _)) if end <= now => {
                self.pause = None;
                None
            }
            Some((end, all)) if all || may_replicate => Some(end),
            _ => None,
        };
        by_client.or((self.failover_pause && may_replicate).then_some(i64::MAX))
    }

    /// Bytes of the connection's live counters, for INFO.
    pub fn stats(&self) -> impl Iterator<Item = &Arc<ConnStats>> {
        self.by_id.values().map(|e| &e.stats)
    }

    pub fn killed(&self, id: u64) -> bool {
        self.by_id.get(&id).is_some_and(|e| e.stats.killed.load(Ordering::Relaxed))
    }
}
