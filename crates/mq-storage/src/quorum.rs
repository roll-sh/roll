//! Quorum-replicated logs over keepers.
//!
//! A [`QuorumLog`] is owned by exactly one writer (the broker that holds the partition's lease).
//! Each append goes to every keeper in parallel and returns once `write_quorum` of them have
//! fsynced it. Keepers are dumb: everything below happens in the writer.
//!
//! # Epochs and fencing
//!
//! Every open takes a fresh, strictly increasing *keeper epoch* and fences at least
//! `N - W + 1` keepers with it (`N` keepers, write quorum `W`). A fenced keeper refuses any
//! request from an older epoch, so a deposed writer can reach at most `W - 1` keepers and can
//! never complete another append: it gets a [`FencedError`] instead of a false confirm. Callers
//! pass a lease epoch to [`LogStore::open`]; `0` means "take over from whoever had it"
//! (the keeper epoch becomes the current one + 1). A non-zero lease epoch `e` maps to keeper
//! epochs in `[e << 16, (e + 1) << 16)`, so reopening with the same lease still gets a new keeper
//! epoch while an older lease is refused.
//!
//! # Invariants
//!
//! 1. **Unique entries per epoch.** A writer assigns each index exactly once and never changes
//!    what it sent for it (failed appends are retransmitted unchanged, never reused), and every
//!    open has its own epoch. So `(index, epoch)` identifies an entry.
//! 2. **Log matching.** Keepers record which epoch wrote each range of entries and accept an
//!    append only if it starts exactly at their tail *and* the epoch of the entry before it
//!    matches the writer's (`prev_epoch`). By induction, two keepers that hold an entry written by
//!    the same epoch at the same index hold identical logs up to it.
//! 3. **Acknowledged entries survive.** An entry is acknowledged once `W` keepers hold it. Any
//!    later open fences `N - W + 1` keepers, so at least one fenced keeper has it (it accepted
//!    the entry before being fenced, or it would have refused it). The new writer adopts the log
//!    of the fenced keeper with the greatest `(last epoch, tail)`. That log contains every
//!    acknowledged entry: a keeper whose last entry has a newer epoch got it from a writer that
//!    (inductively) adopted all acknowledged entries first, and among keepers ending in the same
//!    epoch the longer log extends the shorter one (invariant 2). This is the "max tail among
//!    the quorum" rule made safe against stale suffixes left behind by older writers.
//! 4. **Recovery.** Before `open` returns, the adopted log is copied onto `W` keepers. Keepers
//!    holding a divergent suffix (entries from an epoch that never reached a quorum) are
//!    truncated back to the common prefix computed from the epoch histories, then repaired.
//!    Only never-acknowledged entries are ever truncated (invariant 3).
//! 5. **Repair.** Each keeper has a replicator task. A keeper that is down is skipped; when it
//!    returns it is re-fenced, truncated to the common prefix, and caught up from the writer's
//!    retransmit buffer or by reading from another keeper. A keeper that missed entries that were
//!    since truncated or tiered is reset to the hot start instead.
//!
//! Memory is bounded: unreplicated data is capped by `max_pending_bytes` (appends fail fast past
//! it), and acknowledged entries are kept for retransmission only up to `retain_bytes`.

use crate::client::KeeperClient;
use crate::proto::{History, LogState, Reply, Request, common_prefix, epoch_at, prev_epoch, prune_history};
use crate::stats::{KeeperStatus, Metrics, StorageStats};
use crate::tier::{self, Cache, TierConfig};
use anyhow::{Context, bail};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::TryStreamExt;
use futures_util::future::join_all;
use mq_core::log::{Log, LogStore};
use object_store::ObjectStoreExt;
use object_store::path::Path;
use parking_lot::Mutex;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};
use tracing::{debug, info, warn};

const BATCH_ENTRIES: usize = 8192;
const BATCH_BYTES: usize = 8 << 20;
const MIN_BACKOFF: Duration = Duration::from_millis(20);
const MAX_BACKOFF: Duration = Duration::from_secs(2);

/// Configuration of a [`QuorumStore`].
#[derive(Clone, Debug)]
pub struct QuorumConfig {
    /// Keepers as `(address, availability zone)`.
    pub keepers: Vec<(SocketAddr, String)>,
    /// Acks needed before an append returns (2 of 3 in production).
    pub write_quorum: usize,
    /// This broker's availability zone; reads prefer keepers in it.
    pub az: String,
    /// Upload sealed segments to object storage.
    pub tier: Option<TierConfig>,
    /// Seal the active segment past this size.
    pub segment_bytes: u64,
    /// How long an append (or the recovery in `open`) waits for a quorum.
    pub append_timeout: Duration,
    /// Per-request timeout towards a keeper.
    pub rpc_timeout: Duration,
    /// Unreplicated bytes per log before appends fail fast.
    pub max_pending_bytes: usize,
    /// Acknowledged bytes per log kept in memory to repair lagging keepers.
    pub retain_bytes: usize,
    /// TLS and the shared secret towards keepers (see [`mq_core::transport`]).
    pub transport: std::sync::Arc<mq_core::transport::Transport>,
    /// Compress large frames to and from keepers that support it: cross-AZ traffic is billed
    /// per byte, and message payloads (JSON, text) usually shrink several times.
    pub compress: bool,
}

impl Default for QuorumConfig {
    fn default() -> Self {
        QuorumConfig {
            keepers: Vec::new(),
            write_quorum: 2,
            az: String::new(),
            tier: None,
            segment_bytes: 64 << 20,
            append_timeout: Duration::from_secs(10),
            rpc_timeout: Duration::from_secs(5),
            max_pending_bytes: 64 << 20,
            retain_bytes: 16 << 20,
            transport: mq_core::transport::Transport::plain("keeper"),
            compress: true,
        }
    }
}

/// The log was opened by a newer owner; this handle can no longer write.
#[derive(Debug, Clone)]
pub struct FencedError {
    pub log: String,
    pub epoch: u64,
    pub current: u64,
}

impl fmt::Display for FencedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "log {} is fenced: epoch {} was superseded by {}", self.log, self.epoch, self.current)
    }
}

impl std::error::Error for FencedError {}

struct Ctx {
    cfg: QuorumConfig,
    keepers: Vec<Arc<KeeperClient>>,
    metrics: Metrics,
    cache: Option<Cache>,
    uploads: Semaphore,
}

impl Ctx {
    fn read_quorum(&self) -> usize {
        self.keepers.len() + 1 - self.cfg.write_quorum
    }
}

/// [`LogStore`] whose logs are replicated over keepers with quorum appends and fencing, and
/// optionally tiered to object storage.
pub struct QuorumStore {
    ctx: Arc<Ctx>,
    open: Mutex<HashMap<String, Weak<QuorumLog>>>,
}

impl QuorumStore {
    /// Set up clients for every keeper. Keepers that are down are not an error; they are
    /// repaired once they come back.
    pub async fn connect(cfg: QuorumConfig) -> anyhow::Result<Arc<Self>> {
        let n = cfg.keepers.len();
        if n == 0 || cfg.write_quorum == 0 || cfg.write_quorum > n {
            bail!("invalid quorum: {} keepers, write quorum {}", n, cfg.write_quorum);
        }
        let keepers: Vec<_> = cfg.keepers.iter().map(|(a, az)| KeeperClient::with_options(*a, az.clone(), cfg.rpc_timeout, cfg.transport.clone(), cfg.compress)).collect();
        let up = join_all(keepers.iter().map(|k| k.call(&Request::Ping { features: crate::proto::FEATURES }))).await.into_iter().filter(|r| r.is_ok()).count();
        if up < cfg.write_quorum {
            warn!(up, keepers = n, "fewer keepers reachable than the write quorum; appends will fail until they return");
        }
        let cache = cfg.tier.as_ref().map(|t| Cache::new(t.cache_bytes));
        let ctx = Arc::new(Ctx { keepers, metrics: Metrics::default(), cache, uploads: Semaphore::new(4), cfg });
        Ok(Arc::new(QuorumStore { ctx, open: Mutex::new(HashMap::new()) }))
    }

    /// Open (or take over) a log. See the module docs for what `epoch` means.
    pub async fn open_log(&self, key: &str, epoch: u64) -> anyhow::Result<Arc<QuorumLog>> {
        {
            let mut open = self.open.lock();
            open.retain(|_, w| w.strong_count() > 0);
            if let Some(l) = open.get(key).and_then(Weak::upgrade)
                && (epoch == 0 || epoch == l.lease)
                && l.usable()
            {
                return Ok(l);
            }
        }
        let log = Arc::new(QuorumLog { sh: open_shared(self.ctx.clone(), key, epoch).await?, lease: epoch });
        self.open.lock().insert(key.to_string(), Arc::downgrade(&log));
        Ok(log)
    }

    /// Keys of every log on any reachable keeper.
    pub async fn list_logs(&self) -> anyhow::Result<Vec<String>> {
        let mut all = std::collections::BTreeSet::new();
        let mut ok = 0;
        for r in join_all(self.ctx.keepers.iter().map(|k| k.call(&Request::ListLogs))).await {
            if let Ok(Reply::Logs(l)) = r {
                ok += 1;
                all.extend(l);
            }
        }
        if ok == 0 {
            bail!("no keeper reachable");
        }
        Ok(all.into_iter().collect())
    }

    pub fn stats(&self) -> StorageStats {
        let mut s = StorageStats { backend: self.describe(), ..Default::default() };
        for l in self.open.lock().values().filter_map(Weak::upgrade) {
            let st = l.sh.st.lock();
            s.logs += 1;
            s.bytes_hot += st.hot_bytes;
            s.bytes_cold += st.cold_bytes;
        }
        s.cache_bytes = self.ctx.cache.as_ref().map(|c| c.bytes()).unwrap_or(0);
        s.keepers = self.ctx.keepers.iter().map(|k| KeeperStatus {
                addr: k.addr.to_string(),
                az: k.az.clone(),
                connected: k.is_connected(),
                sent_bytes: k.traffic.sent_raw.load(Ordering::Relaxed),
                sent_wire_bytes: k.traffic.sent_wire.load(Ordering::Relaxed),
                received_bytes: k.traffic.received_raw.load(Ordering::Relaxed),
                received_wire_bytes: k.traffic.received_wire.load(Ordering::Relaxed),
            }).collect();
        self.ctx.metrics.fill(&mut s);
        s
    }
}

#[async_trait]
impl LogStore for QuorumStore {
    async fn open(&self, key: &str, epoch: u64) -> anyhow::Result<Arc<dyn Log>> {
        Ok(self.open_log(key, epoch).await?)
    }

    fn describe(&self) -> String {
        let c = &self.ctx.cfg;
        let tier = c.tier.as_ref().map(|t| format!(", tiered to {}/{}", t.store, t.prefix)).unwrap_or_default();
        format!("quorum log ({} keepers, write quorum {}{tier})", c.keepers.len(), c.write_quorum)
    }
}

/// A replica's state as seen by the writer.
#[derive(Clone, Debug, Default)]
struct Rep {
    /// The keeper holds exactly our log up to here.
    acked: u64,
    synced: bool,
    up: bool,
    trim: u64,
    tiered: u64,
}

/// A fully consumed open segment is only truncated away once it holds this much.
const DRAINED_SEGMENT_BYTES: u64 = 1 << 20;

struct Seg {
    first: u64,
    bytes: u64,
}

struct Cold {
    end: u64,
    bytes: u64,
}

struct St {
    first: u64,
    tiered: u64,
    next: u64,
    committed: u64,
    hist: History,
    /// Entries `[buf_start, next)`: everything not yet acknowledged, plus recent history.
    buf: VecDeque<Bytes>,
    buf_start: u64,
    buf_bytes: usize,
    pending_bytes: usize,
    /// Hot segments (the last one is active unless `seal_next`).
    segs: Vec<Seg>,
    seal_next: bool,
    hot_bytes: u64,
    cold: BTreeMap<u64, Cold>,
    cold_bytes: u64,
    reps: Vec<Rep>,
    fenced: Option<u64>,
    destroyed: bool,
}

impl St {
    fn seg_end(&self, i: usize) -> u64 {
        self.segs.get(i + 1).map(|s| s.first).unwrap_or(self.next)
    }

    fn hot_start(&self) -> u64 {
        self.first.max(self.tiered).min(self.next)
    }

    fn up_count(&self) -> usize {
        self.reps.iter().filter(|r| r.up).count()
    }

    /// Recompute the commit point and trim the retransmit buffer. Returns whether it moved.
    fn advance(&mut self, w: usize, retain: usize) -> bool {
        let mut acked: Vec<u64> = self.reps.iter().map(|r| r.acked).collect();
        acked.sort_unstable_by(|a, b| b.cmp(a));
        let c = acked.get(w - 1).copied().unwrap_or(0).min(self.next);
        let moved = c > self.committed;
        if moved {
            for i in self.committed.max(self.buf_start)..c {
                self.pending_bytes -= self.buf[(i - self.buf_start) as usize].len();
            }
            self.committed = c;
        }
        let everyone = self.reps.iter().map(|r| r.acked).min().unwrap_or(0);
        while self.buf_start < self.committed && (self.buf_start < everyone || self.buf_bytes > retain) {
            let Some(e) = self.buf.pop_front() else { break };
            self.buf_bytes -= e.len();
            self.buf_start += 1;
        }
        moved
    }
}

struct Shared {
    key: String,
    epoch: u64,
    ctx: Arc<Ctx>,
    dir: Option<Path>,
    st: Mutex<St>,
    /// Commit point, fencing or replica health changed.
    changed: Notify,
    wake: Vec<Notify>,
    tier_wake: Notify,
    closed: AtomicBool,
}

enum Step {
    Busy,
    Idle,
}

enum RepErr {
    Fenced(u64),
    /// The keeper is unreachable or misbehaving.
    Down(String),
    /// Nothing wrong with this keeper, but it can't make progress yet.
    Retry(String),
}

/// One log replicated over the keepers. Cheap to share; dropping the last handle stops its
/// background tasks.
pub struct QuorumLog {
    sh: Arc<Shared>,
    lease: u64,
}

/// A keeper's replication state for one log.
#[derive(Clone, Debug)]
pub struct ReplicaInfo {
    pub addr: SocketAddr,
    pub az: String,
    /// Entries the keeper is known to hold.
    pub acked: u64,
    pub up: bool,
}

impl QuorumLog {
    /// The keeper epoch this handle writes with.
    pub fn epoch(&self) -> u64 {
        self.sh.epoch
    }

    /// Entries acknowledged by a write quorum.
    pub fn committed(&self) -> u64 {
        self.sh.st.lock().committed
    }

    pub fn replicas(&self) -> Vec<ReplicaInfo> {
        let st = self.sh.st.lock();
        self.sh.ctx.keepers.iter().zip(&st.reps).map(|(k, r)| ReplicaInfo { addr: k.addr, az: k.az.clone(), acked: r.acked, up: r.up }).collect()
    }

    fn usable(&self) -> bool {
        let st = self.sh.st.lock();
        st.fenced.is_none() && !st.destroyed
    }

    /// `(hot, cold)` bytes.
    pub fn tier_bytes(&self) -> (u64, u64) {
        let st = self.sh.st.lock();
        (st.hot_bytes, st.cold_bytes)
    }
}

impl Drop for QuorumLog {
    fn drop(&mut self) {
        self.sh.close();
    }
}

async fn fence(ctx: &Ctx, key: &str, lease: u64) -> anyhow::Result<(u64, Vec<Option<LogState>>)> {
    let n = ctx.keepers.len();
    let r = ctx.read_quorum();
    if lease >= 1 << 47 {
        bail!("lease epoch {lease} is too large");
    }
    let floor = lease << 16;
    let mut current = 0;
    let mut seen = 0;
    let probe = Request::State { log: key.to_string() };
    for rep in join_all(ctx.keepers.iter().map(|k| k.call(&probe))).await {
        if let Ok(Reply::State(s)) = rep {
            seen += 1;
            current = current.max(s.epoch);
        }
    }
    if seen < r {
        bail!("only {seen} of {n} keepers reachable; {r} are needed to open {key}");
    }
    for _ in 0..16 {
        if lease > 0 && current >> 16 > lease {
            return Err(FencedError { log: key.to_string(), epoch: floor, current }.into());
        }
        let epoch = (current + 1).max(floor);
        let req = Request::Fence { log: key.to_string(), epoch, strict: true };
        let replies = join_all(ctx.keepers.iter().map(|k| k.call(&req))).await;
        let mut states = vec![None; n];
        let mut higher = None;
        let mut ok = 0;
        for (i, rep) in replies.into_iter().enumerate() {
            match rep {
                Ok(Reply::State(s)) => {
                    states[i] = Some(s);
                    ok += 1;
                }
                Ok(Reply::Fenced { epoch }) => higher = higher.max(Some(epoch)),
                Ok(other) => warn!(log = key, keeper = %ctx.keepers[i].addr, reply = ?other, "unexpected fence reply"),
                Err(_) => {}
            }
        }
        if let Some(x) = higher {
            // Someone else is opening the same log; outbid them (or give up on an old lease).
            current = current.max(x);
            continue;
        }
        if ok >= r {
            return Ok((epoch, states));
        }
        bail!("only {ok} of {n} keepers accepted the fence; {r} are needed to open {key}");
    }
    bail!("could not fence {key}: competing writers keep opening it")
}

/// Cold segments of a log: `(first → Cold)` and the end of the last one. Objects entirely
/// before `first` are leftovers of an interrupted truncation and are deleted.
async fn list_cold(tier: &TierConfig, dir: &Path, first: u64) -> anyhow::Result<(BTreeMap<u64, Cold>, u64)> {
    let depth = dir.parts().count() + 1;
    let metas: Vec<_> = tier.store.list(Some(dir)).try_collect().await.with_context(|| format!("list {dir}"))?;
    let mut found: BTreeMap<u64, (Path, u64)> = BTreeMap::new();
    for m in metas {
        if m.location.parts().count() == depth
            && let Some(f) = m.location.filename().and_then(tier::parse_seg_name)
        {
            found.insert(f, (m.location, m.size));
        }
    }
    let mut out = BTreeMap::new();
    let mut last_end = 0;
    let firsts: Vec<u64> = found.keys().copied().collect();
    for (k, f) in firsts.iter().enumerate() {
        let (path, size) = &found[f];
        let end = match firsts.get(k + 1) {
            Some(n) => *n,
            None => {
                let tail = tier.store.get_range(path, size.saturating_sub(tier::FOOTER_LEN as u64)..*size).await?;
                tier::decode_footer(&tail)?.3
            }
        };
        if end <= first {
            let _ = tier.store.delete(path).await;
            continue;
        }
        last_end = last_end.max(end);
        out.insert(*f, Cold { end, bytes: *size });
    }
    Ok((out, last_end))
}

async fn open_shared(ctx: Arc<Ctx>, key: &str, lease: u64) -> anyhow::Result<Arc<Shared>> {
    let n = ctx.keepers.len();
    let (epoch, mut states) = fence(&ctx, key, lease).await?;
    let Some(src) = states.iter().flatten().max_by_key(|s| (s.last_epoch(), s.next)).cloned() else {
        bail!("no keeper state for {key}");
    };
    let next = src.next;
    let first = states.iter().flatten().map(|s| s.trim).max().unwrap_or(0).min(next);
    let mut tiered = states.iter().flatten().map(|s| s.tiered).max().unwrap_or(0).min(next);
    let dir = match &ctx.cfg.tier {
        Some(t) => Some(tier::log_dir(&t.prefix, key)?),
        None => None,
    };
    let mut cold = BTreeMap::new();
    if let (Some(t), Some(d)) = (&ctx.cfg.tier, &dir) {
        let (c, end) = list_cold(t, d, first).await?;
        cold = c;
        tiered = tiered.max(end.min(next));
    }
    let mut hist = src.history.clone();
    prune_history(&mut hist, first);
    let hot = first.max(tiered);
    let mut segs: Vec<Seg> = Vec::new();
    for (k, &(f, bytes)) in src.segments.iter().enumerate() {
        let end = src.segments.get(k + 1).map(|s| s.0).unwrap_or(next);
        if end > hot {
            segs.push(Seg { first: f, bytes });
        }
    }
    let hot_bytes = segs.iter().map(|s| s.bytes).sum();
    let seal_next = segs.last().is_some_and(|s| s.bytes >= ctx.cfg.segment_bytes);
    let cold_bytes = cold.values().map(|c| c.bytes).sum();
    let st = St {
        first,
        tiered,
        next,
        committed: 0,
        hist,
        buf: VecDeque::new(),
        buf_start: next,
        buf_bytes: 0,
        pending_bytes: 0,
        segs,
        seal_next,
        hot_bytes,
        cold,
        cold_bytes,
        reps: vec![Rep::default(); n],
        fenced: None,
        destroyed: false,
    };
    let sh = Arc::new(Shared {
        key: key.to_string(),
        epoch,
        dir,
        st: Mutex::new(st),
        changed: Notify::new(),
        wake: (0..n).map(|_| Notify::new()).collect(),
        tier_wake: Notify::new(),
        closed: AtomicBool::new(false),
        ctx: ctx.clone(),
    });
    for (i, s) in states.iter_mut().enumerate() {
        tokio::spawn(replicator(sh.clone(), i, s.take()));
    }
    if ctx.cfg.tier.is_some() {
        tokio::spawn(tierer(sh.clone()));
    }
    // Recovery: the adopted log must be on a write quorum before anyone builds on it.
    let w = ctx.cfg.write_quorum;
    let deadline = tokio::time::Instant::now() + ctx.cfg.append_timeout;
    loop {
        let notified = sh.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let st = sh.st.lock();
            if let Some(cur) = st.fenced {
                drop(st);
                sh.close();
                return Err(FencedError { log: key.to_string(), epoch, current: cur }.into());
            }
            if st.committed >= next && st.reps.iter().filter(|r| r.synced).count() >= w {
                break;
            }
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            sh.close();
            bail!("could not replicate {key} to a write quorum while opening it");
        }
    }
    info!(log = key, epoch, first, next, "opened replicated log");
    Ok(sh)
}

async fn replicator(sh: Arc<Shared>, i: usize, mut initial: Option<LogState>) {
    let mut backoff = MIN_BACKOFF;
    while !sh.closed.load(Ordering::Acquire) {
        let synced = sh.st.lock().reps[i].synced;
        let res = if synced { sh.push(i).await } else { sh.sync(i, initial.take()).await };
        match res {
            Ok(Step::Busy) => backoff = MIN_BACKOFF,
            Ok(Step::Idle) => {
                backoff = MIN_BACKOFF;
                tokio::select! {
                    _ = sh.wake[i].notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
            }
            Err(RepErr::Fenced(cur)) => {
                sh.fence_detected(cur);
                return;
            }
            Err(RepErr::Down(why)) => {
                {
                    let mut st = sh.st.lock();
                    let r = &mut st.reps[i];
                    if r.up {
                        warn!(log = %sh.key, keeper = %sh.ctx.keepers[i].addr, error = %why, "keeper replica down");
                    }
                    r.up = false;
                    r.synced = false;
                }
                sh.changed.notify_waiters();
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
            Err(RepErr::Retry(why)) => {
                debug!(log = %sh.key, keeper = %sh.ctx.keepers[i].addr, why, "replica waiting");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

async fn tierer(sh: Arc<Shared>) {
    while !sh.closed.load(Ordering::Acquire) {
        tokio::select! {
            _ = sh.tier_wake.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        loop {
            if sh.closed.load(Ordering::Acquire) {
                return;
            }
            let Some((a, b)) = sh.tier_candidate() else { break };
            if let Err(e) = sh.upload(a, b).await {
                warn!(log = %sh.key, error = %format!("{e:#}"), "tiering failed");
                break;
            }
        }
    }
}

impl Shared {
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.wake.iter().for_each(|w| w.notify_one());
        self.tier_wake.notify_one();
        self.changed.notify_waiters();
    }

    fn fenced_err(&self, current: u64) -> anyhow::Error {
        FencedError { log: self.key.clone(), epoch: self.epoch, current }.into()
    }

    fn fence_detected(&self, current: u64) {
        let mut st = self.st.lock();
        if st.fenced.is_none() {
            warn!(log = %self.key, epoch = self.epoch, current, "log fenced by a newer owner");
            st.fenced = Some(current);
        }
        drop(st);
        self.changed.notify_waiters();
    }

    fn advance(&self, st: &mut St) -> bool {
        st.advance(self.ctx.cfg.write_quorum, self.ctx.cfg.retain_bytes)
    }

    async fn rpc(&self, i: usize, req: &Request) -> Result<Reply, RepErr> {
        match self.ctx.keepers[i].call(req).await {
            Ok(Reply::Fenced { epoch }) => Err(RepErr::Fenced(epoch)),
            Ok(Reply::Error(e)) => Err(RepErr::Down(e)),
            Ok(r) => Ok(r),
            Err(e) => Err(RepErr::Down(e.to_string())),
        }
    }

    /// Bring a (re)connected keeper in line with our log.
    async fn sync(&self, i: usize, initial: Option<LogState>) -> Result<Step, RepErr> {
        let s = match initial {
            Some(s) => s,
            None => match self.rpc(i, &Request::Fence { log: self.key.clone(), epoch: self.epoch, strict: false }).await? {
                Reply::State(s) => s,
                other => return Err(RepErr::Down(format!("unexpected reply {other:?}"))),
            },
        };
        enum Fix {
            Keep,
            Suffix,
            Reset { prev: u64, trim: u64, tiered: u64 },
        }
        let (fix, d) = {
            let st = self.st.lock();
            let d = common_prefix(&st.hist, st.next, &s.history, s.next);
            let hot = st.hot_start();
            if d < hot || d < s.trim || (s.next > d && d < s.hot_first) {
                (Fix::Reset { prev: prev_epoch(&st.hist, hot), trim: st.first, tiered: st.tiered }, hot)
            } else if s.next > d {
                (Fix::Suffix, d)
            } else {
                (Fix::Keep, d)
            }
        };
        let s = match fix {
            Fix::Keep => s,
            Fix::Suffix => {
                info!(log = %self.key, keeper = %self.ctx.keepers[i].addr, from = d, to = s.next, "truncating divergent suffix");
                match self.rpc(i, &Request::TruncateSuffix { log: self.key.clone(), epoch: self.epoch, from: d }).await? {
                    Reply::State(s) => s,
                    other => return Err(RepErr::Down(format!("unexpected reply {other:?}"))),
                }
            }
            Fix::Reset { prev, trim, tiered } => {
                info!(log = %self.key, keeper = %self.ctx.keepers[i].addr, at = d, "resetting lagging replica");
                let req = Request::Reset { log: self.key.clone(), epoch: self.epoch, at: d, prev_epoch: prev, trim, tiered };
                match self.rpc(i, &req).await? {
                    Reply::State(s) => s,
                    other => return Err(RepErr::Down(format!("unexpected reply {other:?}"))),
                }
            }
        };
        if s.next != d {
            return Err(RepErr::Down(format!("keeper ended at {} after repair, expected {d}", s.next)));
        }
        {
            let mut st = self.st.lock();
            st.reps[i] = Rep { acked: d, synced: true, up: true, trim: s.trim, tiered: s.tiered };
            self.advance(&mut st);
        }
        self.changed.notify_waiters();
        Ok(Step::Busy)
    }

    /// Send the next batch (or housekeeping) to keeper `i`.
    async fn push(&self, i: usize) -> Result<Step, RepErr> {
        enum Job {
            Append { start: u64, end: u64, seal: bool, entry_epoch: u64, prev: u64, entries: Vec<Bytes> },
            Trim(u64),
            Tiered(u64),
            Idle,
        }
        let job = {
            let st = self.st.lock();
            let r = &st.reps[i];
            if st.destroyed {
                Job::Idle
            } else if r.acked < st.next {
                let start = r.acked;
                let mut end = st.next;
                if let Some(s) = st.segs.get(st.segs.partition_point(|s| s.first <= start)) {
                    end = end.min(s.first);
                }
                if let Some(h) = st.hist.get(st.hist.partition_point(|h| h.0 <= start)) {
                    end = end.min(h.0);
                }
                let mut entries = Vec::new();
                if start >= st.buf_start {
                    let mut bytes = 0;
                    for e in st.buf.iter().skip((start - st.buf_start) as usize).take((end - start) as usize) {
                        if !entries.is_empty() && (entries.len() >= BATCH_ENTRIES || bytes + e.len() > BATCH_BYTES) {
                            break;
                        }
                        bytes += e.len();
                        entries.push(e.clone());
                    }
                }
                Job::Append {
                    start,
                    end,
                    seal: st.segs.binary_search_by_key(&start, |s| s.first).is_ok(),
                    entry_epoch: epoch_at(&st.hist, start),
                    prev: prev_epoch(&st.hist, start),
                    entries,
                }
            } else if r.trim < st.first {
                Job::Trim(st.first)
            } else if r.tiered < st.tiered {
                Job::Tiered(st.tiered)
            } else {
                Job::Idle
            }
        };
        match job {
            Job::Idle => Ok(Step::Idle),
            Job::Append { start, end, seal, entry_epoch, prev, mut entries } => {
                if entries.is_empty() {
                    entries = self.fetch(i, start, end).await?;
                }
                let n = entries.len() as u64;
                let req = Request::Append { log: self.key.clone(), epoch: self.epoch, first: start, prev_epoch: prev, entry_epoch, seal, entries };
                match self.rpc(i, &req).await? {
                    Reply::Appended { .. } => {
                        let moved = {
                            let mut st = self.st.lock();
                            if st.reps[i].acked == start && st.reps[i].synced {
                                st.reps[i].acked = start + n;
                            }
                            self.advance(&mut st)
                        };
                        self.changed.notify_waiters();
                        if moved {
                            self.tier_wake.notify_one();
                        }
                        Ok(Step::Busy)
                    }
                    Reply::Mismatch { next, last_epoch } => {
                        debug!(log = %self.key, keeper = %self.ctx.keepers[i].addr, next, last_epoch, "replica out of step; resyncing");
                        self.st.lock().reps[i].synced = false;
                        Ok(Step::Busy)
                    }
                    other => Err(RepErr::Down(format!("unexpected reply {other:?}"))),
                }
            }
            Job::Trim(t) => {
                self.rpc(i, &Request::TruncatePrefix { log: self.key.clone(), epoch: self.epoch, before: t }).await?;
                self.st.lock().reps[i].trim = t;
                Ok(Step::Busy)
            }
            Job::Tiered(t) => {
                self.rpc(i, &Request::Tiered { log: self.key.clone(), epoch: self.epoch, upto: t }).await?;
                self.st.lock().reps[i].tiered = t;
                Ok(Step::Busy)
            }
        }
    }

    /// Entries `[start, end)` (or a prefix) from another keeper, for repairs that fall outside
    /// the retransmit buffer.
    async fn fetch(&self, i: usize, start: u64, end: u64) -> Result<Vec<Bytes>, RepErr> {
        let cands = self.hot_sources(start, Some(i));
        for (j, limit) in cands {
            let req = Request::Read { log: self.key.clone(), from: start, max: BATCH_ENTRIES as u32, max_bytes: BATCH_BYTES as u32 };
            match self.rpc(j, &req).await {
                Ok(Reply::Entries { first, mut entries }) if first == start && !entries.is_empty() => {
                    entries.truncate((end.min(limit) - start) as usize);
                    return Ok(entries);
                }
                Err(RepErr::Fenced(e)) => return Err(RepErr::Fenced(e)),
                _ => {}
            }
        }
        Err(RepErr::Retry(format!("no keeper can supply entries from {start}")))
    }

    /// Synced keepers holding index `at`, same-AZ first, with how far each can be read.
    fn hot_sources(&self, at: u64, except: Option<usize>) -> Vec<(usize, u64)> {
        let st = self.st.lock();
        let mut c: Vec<(usize, u64)> =
            st.reps.iter().enumerate().filter(|(j, r)| Some(*j) != except && r.synced && r.acked > at).map(|(j, r)| (j, r.acked)).collect();
        let az = &self.ctx.cfg.az;
        c.sort_by_key(|(j, _)| self.ctx.keepers[*j].az != *az);
        c
    }

    async fn append(&self, entries: Vec<Bytes>) -> anyhow::Result<u64> {
        let started = Instant::now();
        let bytes: usize = entries.iter().map(|e| e.len()).sum();
        let w = self.ctx.cfg.write_quorum;
        let (idx, target) = {
            let mut st = self.st.lock();
            if let Some(cur) = st.fenced {
                return Err(self.fenced_err(cur));
            }
            if st.destroyed {
                bail!("log {} was destroyed", self.key);
            }
            if entries.is_empty() {
                return Ok(st.next);
            }
            if st.pending_bytes > 0 && st.pending_bytes + bytes > self.ctx.cfg.max_pending_bytes {
                self.ctx.metrics.record(started.elapsed(), 0, false);
                bail!("log {}: {} bytes are waiting for a write quorum; refusing more", self.key, st.pending_bytes);
            }
            if st.up_count() < w {
                self.ctx.metrics.record(started.elapsed(), 0, false);
                bail!("log {}: only {} of {} keepers available, write quorum is {w}", self.key, st.up_count(), st.reps.len());
            }
            let idx = st.next;
            if st.seal_next || st.segs.is_empty() {
                st.segs.push(Seg { first: idx, bytes: 0 });
                st.seal_next = false;
            }
            if st.hist.last().map(|h| h.1) != Some(self.epoch) {
                st.hist.push((idx, self.epoch));
            }
            let framed: u64 = entries.iter().map(|e| 8 + e.len() as u64).sum();
            if let Some(s) = st.segs.last_mut() {
                s.bytes += framed;
                if s.bytes >= self.ctx.cfg.segment_bytes {
                    st.seal_next = true;
                }
            }
            st.hot_bytes += framed;
            st.next += entries.len() as u64;
            st.buf_bytes += bytes;
            st.pending_bytes += bytes;
            st.buf.extend(entries);
            (idx, st.next)
        };
        self.wake.iter().for_each(|w| w.notify_one());
        let deadline = tokio::time::Instant::from_std(started) + self.ctx.cfg.append_timeout;
        let res = loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let st = self.st.lock();
                if st.committed >= target {
                    break Ok(idx);
                }
                if let Some(cur) = st.fenced {
                    break Err(self.fenced_err(cur));
                }
                if st.up_count() < w {
                    break Err(anyhow::anyhow!("log {}: write quorum lost ({} of {} keepers available)", self.key, st.up_count(), st.reps.len()));
                }
            }
            if self.closed.load(Ordering::Acquire) {
                break Err(anyhow::anyhow!("log {} closed", self.key));
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                break Err(anyhow::anyhow!("log {}: timed out waiting for a write quorum", self.key));
            }
        };
        self.ctx.metrics.record(started.elapsed(), bytes as u64, res.is_ok());
        res
    }

    async fn read(&self, from: u64, max: usize, max_bytes: usize) -> anyhow::Result<Vec<(u64, Bytes)>> {
        enum Src {
            Done,
            Cold(Path),
            Hot,
        }
        let mut out = Vec::new();
        let mut bytes = 0usize;
        let mut at = from.max(self.st.lock().first);
        while out.len() < max && (out.is_empty() || bytes < max_bytes) {
            let src = {
                let st = self.st.lock();
                at = at.max(st.first);
                if at >= st.next {
                    Src::Done
                } else if at >= st.buf_start {
                    for e in st.buf.iter().skip((at - st.buf_start) as usize) {
                        if out.len() >= max || (bytes > 0 && bytes + e.len() > max_bytes) {
                            break;
                        }
                        bytes += e.len();
                        out.push((at, e.clone()));
                        at += 1;
                    }
                    Src::Done
                } else {
                    let cold = (at < st.tiered)
                        .then(|| st.cold.range(..=at).next_back())
                        .flatten()
                        .filter(|(_, c)| at < c.end)
                        .and_then(|(f, _)| self.dir.as_ref().map(|d| tier::seg_path(d, *f)));
                    match cold {
                        Some(p) => Src::Cold(p),
                        None => Src::Hot,
                    }
                }
            };
            match src {
                Src::Done => break,
                Src::Cold(path) => {
                    let (Some(t), Some(cache)) = (&self.ctx.cfg.tier, &self.ctx.cache) else { break };
                    let seg = tier::fetch_segment(t.store.as_ref(), cache, &path).await?;
                    let before = out.len();
                    let next_at = seg.read_into(at, max, max_bytes, &mut out, &mut bytes);
                    if next_at <= at && out.len() == before {
                        bail!("cold segment {path} does not hold index {at}");
                    }
                    at = next_at;
                    if out.len() >= max || next_at < seg.end {
                        break;
                    }
                }
                Src::Hot => {
                    let mut got = false;
                    let before = out.len();
                    for (j, limit) in self.hot_sources(at, None) {
                        let want = (max - out.len()).min(BATCH_ENTRIES);
                        let req = Request::Read { log: self.key.clone(), from: at, max: want as u32, max_bytes: max_bytes.saturating_sub(bytes).max(1) as u32 };
                        if let Ok(Reply::Entries { first, entries }) = self.ctx.keepers[j].call(&req).await
                            && first == at
                            && !entries.is_empty()
                        {
                            for e in entries.into_iter().take((limit - at) as usize) {
                                if out.len() >= max || (bytes > 0 && bytes + e.len() > max_bytes) {
                                    break;
                                }
                                bytes += e.len();
                                out.push((at, e));
                                at += 1;
                            }
                            got = true;
                            break;
                        }
                    }
                    if !got {
                        if out.is_empty() {
                            bail!("log {}: no keeper can serve index {at}", self.key);
                        }
                        break;
                    }
                    if out.len() == before {
                        // The byte budget is spent.
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    async fn truncate_prefix(&self, before: u64) -> anyhow::Result<u64> {
        let (cut, first) = {
            let st = self.st.lock();
            if let Some(cur) = st.fenced {
                return Err(self.fenced_err(cur));
            }
            let cut = st
                .cold
                .iter()
                .find(|(_, c)| c.end > before)
                .map(|(f, _)| *f)
                .or_else(|| (0..st.segs.len()).find(|&k| st.seg_end(k) > before).map(|k| st.segs[k].first))
                // A drained log keeps a small open segment rather than having every keeper
                // delete it and start another for the next message.
                .unwrap_or_else(|| match st.segs.last() {
                    Some(s) if s.bytes < DRAINED_SEGMENT_BYTES.min(self.ctx.cfg.segment_bytes) => s.first,
                    _ => st.next,
                });
            (cut.min(st.committed).max(st.first), st.first)
        };
        if cut <= first {
            return Ok(first);
        }
        let targets: Vec<usize> = self.st.lock().reps.iter().enumerate().filter(|(_, r)| r.synced).map(|(i, _)| i).collect();
        let req = Request::TruncatePrefix { log: self.key.clone(), epoch: self.epoch, before: cut };
        let replies = join_all(targets.iter().map(|&i| self.rpc(i, &req))).await;
        let mut ok = 0;
        for (&i, r) in targets.iter().zip(replies) {
            match r {
                Ok(_) => {
                    ok += 1;
                    self.st.lock().reps[i].trim = cut;
                }
                Err(RepErr::Fenced(cur)) => {
                    self.fence_detected(cur);
                    return Err(self.fenced_err(cur));
                }
                Err(_) => {}
            }
        }
        if ok < self.ctx.cfg.write_quorum {
            bail!("log {}: only {ok} keepers accepted the truncation", self.key);
        }
        let drop_cold: Vec<u64> = {
            let mut st = self.st.lock();
            st.first = st.first.max(cut);
            while !st.segs.is_empty() && st.seg_end(0) <= cut {
                let s = st.segs.remove(0);
                st.hot_bytes -= s.bytes;
            }
            let gone: Vec<u64> = st.cold.iter().filter(|(_, c)| c.end <= cut).map(|(f, _)| *f).collect();
            for f in &gone {
                if let Some(c) = st.cold.remove(f) {
                    st.cold_bytes -= c.bytes;
                }
            }
            let first = st.first;
            prune_history(&mut st.hist, first);
            gone
        };
        if let (Some(t), Some(d)) = (&self.ctx.cfg.tier, &self.dir) {
            for f in drop_cold {
                let p = tier::seg_path(d, f);
                if let Some(c) = &self.ctx.cache {
                    c.remove(p.as_ref());
                }
                if let Err(e) = t.store.delete(&p).await
                    && !matches!(e, object_store::Error::NotFound { .. })
                {
                    warn!(path = %p, error = %e, "could not delete cold segment");
                }
            }
        }
        Ok(cut)
    }

    /// The oldest sealed, committed hot segment, if the hot tail is over its budget.
    fn tier_candidate(&self) -> Option<(u64, u64)> {
        let st = self.st.lock();
        if st.fenced.is_some() || st.destroyed {
            return None;
        }
        let sealed = if st.seal_next { st.segs.len() } else { st.segs.len().saturating_sub(1) };
        if sealed == 0 {
            return None;
        }
        let end = st.seg_end(0);
        let sealed_bytes: u64 = st.segs[..sealed].iter().map(|s| s.bytes).sum();
        let keep = self.ctx.cfg.tier.as_ref().map(|t| t.keep_hot_bytes).unwrap_or(u64::MAX);
        (end <= st.committed && sealed_bytes > keep).then(|| (st.segs[0].first.max(st.first), end))
    }

    async fn upload(&self, a: u64, b: u64) -> anyhow::Result<()> {
        let (Some(t), Some(dir)) = (&self.ctx.cfg.tier, &self.dir) else { return Ok(()) };
        let _permit = self.ctx.uploads.acquire().await?;
        let mut entries = Vec::new();
        let mut at = a;
        while at < b {
            let batch = self.read(at, BATCH_ENTRIES, BATCH_BYTES).await?;
            if batch.is_empty() || batch[0].0 != at {
                break;
            }
            for (i, e) in batch {
                if i >= b {
                    break;
                }
                at = i + 1;
                entries.push((i, e));
            }
        }
        let first_present = entries.first().map(|(i, _)| *i);
        if at < b && first_present.is_some() {
            bail!("could only read {a}..{at} of segment {a}..{b}");
        }
        let start = first_present.unwrap_or(a);
        let data = tier::encode_segment(&entries, start, b);
        let size = data.len() as u64;
        let path = tier::seg_path(dir, start);
        if !entries.is_empty() {
            t.store.put(&path, data.into()).await.with_context(|| format!("put {path}"))?;
        }
        let stale = {
            let mut st = self.st.lock();
            if st.first >= b || st.segs.first().is_none_or(|s| s.first > a) {
                true
            } else {
                let s = st.segs.remove(0);
                st.hot_bytes -= s.bytes;
                if !entries.is_empty() {
                    st.cold.insert(start, Cold { end: b, bytes: size });
                    st.cold_bytes += size;
                }
                st.tiered = st.tiered.max(b);
                false
            }
        };
        if stale {
            // Truncated while we were uploading.
            let _ = t.store.delete(&path).await;
            return Ok(());
        }
        debug!(log = %self.key, first = start, end = b, bytes = size, "tiered segment");
        let targets: Vec<usize> = self.st.lock().reps.iter().enumerate().filter(|(_, r)| r.synced).map(|(i, _)| i).collect();
        let req = Request::Tiered { log: self.key.clone(), epoch: self.epoch, upto: b };
        for (&i, r) in targets.iter().zip(join_all(targets.iter().map(|&i| self.rpc(i, &req))).await) {
            match r {
                Ok(_) => self.st.lock().reps[i].tiered = b,
                Err(RepErr::Fenced(cur)) => self.fence_detected(cur),
                Err(_) => {}
            }
        }
        Ok(())
    }

    async fn destroy(&self) -> anyhow::Result<()> {
        {
            let mut st = self.st.lock();
            if let Some(cur) = st.fenced {
                return Err(self.fenced_err(cur));
            }
            st.destroyed = true;
        }
        let req = Request::Destroy { log: self.key.clone(), epoch: self.epoch };
        let replies = join_all(self.ctx.keepers.iter().map(|k| k.call(&req))).await;
        let ok = replies.iter().filter(|r| matches!(r, Ok(Reply::State(_)))).count();
        if ok < replies.len() {
            warn!(log = %self.key, reached = ok, keepers = replies.len(), "destroy did not reach every keeper; the rest keep orphaned data");
        }
        if let (Some(t), Some(dir)) = (&self.ctx.cfg.tier, &self.dir) {
            let metas: Vec<_> = t.store.list(Some(dir)).try_collect().await.unwrap_or_default();
            for m in metas {
                let _ = t.store.delete(&m.location).await;
                if let Some(c) = &self.ctx.cache {
                    c.remove(m.location.as_ref());
                }
            }
        }
        {
            let mut st = self.st.lock();
            st.first = st.next;
            st.committed = st.next;
            st.segs.clear();
            st.cold.clear();
            st.hot_bytes = 0;
            st.cold_bytes = 0;
            st.buf.clear();
            st.buf_start = st.next;
            st.buf_bytes = 0;
            st.pending_bytes = 0;
        }
        self.close();
        Ok(())
    }
}

#[async_trait]
impl Log for QuorumLog {
    async fn append(&self, entries: Vec<Bytes>) -> anyhow::Result<u64> {
        self.sh.append(entries).await
    }

    async fn read(&self, from: u64, max: usize, max_bytes: usize) -> anyhow::Result<Vec<(u64, Bytes)>> {
        self.sh.read(from, max, max_bytes).await
    }

    fn bounds(&self) -> (u64, u64) {
        let st = self.sh.st.lock();
        (st.first, st.next)
    }

    async fn truncate_prefix(&self, before: u64) -> anyhow::Result<u64> {
        self.sh.truncate_prefix(before).await
    }

    fn bytes(&self) -> u64 {
        let st = self.sh.st.lock();
        st.hot_bytes + st.cold_bytes
    }

    async fn destroy(&self) -> anyhow::Result<()> {
        self.sh.destroy().await
    }

    async fn roll(&self) -> anyhow::Result<()> {
        {
            let mut st = self.sh.st.lock();
            if st.segs.last().is_some_and(|s| s.bytes > 0) {
                st.seal_next = true;
            }
        }
        self.sh.tier_wake.notify_one();
        Ok(())
    }
}
